//! Compiles checked-in translated shader sources into platform binaries.
//!
//! `shaderloom::build::package_wgsl_source` writes the translated WGSL,
//! SPIR-V, MSL and HLSL artifacts next to a crate's sources so they can be
//! committed and shipped in the published `.crate`. At build time only the
//! platform toolchains still run: `xcrun` on Apple targets turns the
//! checked-in `.metal` into a `MetalLib` under `OUT_DIR`, and `dxc` on
//! Windows compiles the checked-in per-entry `.hlsl` into DXIL. Neither step
//! needs `naga`, so a consumer's host graph stays free of the WGSL compiler.
//!
//! Each packaged shader carries a `<name>.manifest` written by the
//! packager: a TOML-serialized [`PackagedManifest`] recording the Metal
//! language version the checked-in MSL targets and the DXIL entry-point
//! table — one [`DxilEntry`] per entry point.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};

/// Compiles the checked-in `.metal` source of one packaged shader into the
/// `MetalLib` artifact under `OUT_DIR` that the generated `CompiledShader`
/// expression embeds.
///
/// # Panics
///
/// Panics when the target is not an Apple platform, when the packaged
/// manifest or Metal source is missing, or when `xcrun` fails.
pub fn compile_packaged_metallib(packaged_dir: &str, artifact_name: &str) {
    let manifest_dir = PathBuf::from(required_env("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join(packaged_dir);
    let manifest = read_manifest(&source_dir, artifact_name);
    let target_os = required_env("CARGO_CFG_TARGET_OS");
    assert_eq!(
        required_env("CARGO_CFG_TARGET_VENDOR"),
        "apple",
        "packaged MetalLib compilation is only valid for Apple targets"
    );

    let source_path = source_dir.join(format!("{artifact_name}.metal"));
    rerun_if_changed(&source_path);
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    let air_path = out_dir.join(format!("{artifact_name}.air"));
    let metallib_path = out_dir.join(format!("{artifact_name}.metallib"));

    let sdk = apple_sdk(&target_os);
    let standard = metal_language_standard(&target_os, manifest.msl_version);
    run_tool(
        Command::new("xcrun")
            .args(["--sdk", sdk, "metal", "-c"])
            .arg(standard)
            .arg("-target")
            .arg(metal_target(&target_os))
            .arg(&source_path)
            .arg("-o")
            .arg(&air_path),
        "Metal compilation",
        artifact_name,
    );
    run_tool(
        Command::new("xcrun")
            .args(["--sdk", sdk, "metallib"])
            .arg(&air_path)
            .arg("-o")
            .arg(&metallib_path),
        "Metal library linking",
        artifact_name,
    );
}

/// Compiles every checked-in `.hlsl` entry-point source of one packaged
/// shader into the stage-specific DXIL artifacts under `OUT_DIR` that the
/// generated `CompiledShader` expression embeds.
///
/// # Panics
///
/// Panics when the target is not Windows, when the packaged manifest or an
/// HLSL source is missing, or when `dxc` fails.
pub fn compile_packaged_dxil(packaged_dir: &str, artifact_name: &str) {
    let manifest_dir = PathBuf::from(required_env("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join(packaged_dir);
    let manifest = read_manifest(&source_dir, artifact_name);
    assert_eq!(
        required_env("CARGO_CFG_TARGET_OS"),
        "windows",
        "packaged DXIL compilation is only valid for Windows targets"
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"));

    for entry in &manifest.dxil {
        let hlsl_path = source_dir.join(format!("{}.hlsl", entry.stem));
        rerun_if_changed(&hlsl_path);
        run_tool(
            Command::new("dxc")
                .args(["-T", &entry.profile, "-E"])
                .arg(&entry.entry_name)
                .args(["-Qstrip_debug", "-Qstrip_reflect", "-Fo"])
                .arg(out_dir.join(format!("{}.dxil", entry.stem)))
                .arg(&hlsl_path),
            "DXIL compilation",
            artifact_name,
        );
    }
}

/// One entry point's DXIL compilation request: which checked-in `.hlsl`
/// source feeds `dxc`, at which profile, under which translated entry name.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DxilEntry {
    /// Artifact stem of the `.hlsl` file and the `.dxil` it produces.
    pub(crate) stem: String,
    /// `dxc` target profile, e.g. `vs_6_0`.
    pub(crate) profile: String,
    /// Entry-point symbol inside the HLSL source.
    pub(crate) entry_name: String,
}

/// The recorded toolchain parameters of one packaged shader, shared verbatim
/// between the `build` writer and this reader: serialized as TOML into
/// `<name>.manifest`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PackagedManifest {
    /// Metal language version the checked-in `.metal` source targets.
    pub(crate) msl_version: (u8, u8),
    /// Per-entry-point DXIL compilation table.
    pub(crate) dxil: Vec<DxilEntry>,
}

fn read_manifest(dir: &Path, artifact_name: &str) -> PackagedManifest {
    let path = dir.join(format!("{artifact_name}.manifest"));
    rerun_if_changed(&path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    toml::from_str(&text)
        .unwrap_or_else(|error| panic!("malformed shader manifest {}: {error}", path.display()))
}

fn rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

pub(crate) fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for shader compilation"))
}

pub(crate) fn run_tool(command: &mut Command, operation: &str, label: &str) {
    let executable = command.get_program().to_owned();
    let output = command.output().unwrap_or_else(|error| {
        panic!(
            "{operation} failed for {label}: unable to execute {}: {error}",
            executable.to_string_lossy()
        )
    });
    if !output.status.success() {
        panic_tool_failure(operation, label, &output);
    }
}

fn panic_tool_failure(operation: &str, label: &str, output: &Output) -> ! {
    panic!(
        "{operation} failed for {label} with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub(crate) fn metal_language_standard(target_os: &str, version: (u8, u8)) -> String {
    let prefix = if version.0 >= 3 {
        "metal"
    } else if target_os == "macos" {
        "macos-metal"
    } else {
        "ios-metal"
    };
    format!("-std={prefix}{}.{}", version.0, version.1)
}

/// The Apple platform a Cargo target builds for, shared by SDK selection
/// (`xcrun --sdk`) and the Metal driver's `-target` triple so the two can
/// never disagree.
#[derive(Debug, Clone, Copy)]
enum ApplePlatform {
    MacOs,
    Ios,
    IosSimulator,
    /// Mac Catalyst: the `ios` target OS with a `macabi` ABI, built with the
    /// macOS SDK.
    MacCatalyst,
    Tvos,
    TvosSimulator,
    Watchos,
    WatchosSimulator,
    Visionos,
    VisionosSimulator,
}

impl ApplePlatform {
    /// The `xcrun --sdk` name for the platform.
    const fn sdk(self) -> &'static str {
        match self {
            Self::MacOs | Self::MacCatalyst => "macosx",
            Self::Ios => "iphoneos",
            Self::IosSimulator => "iphonesimulator",
            Self::Tvos => "appletvos",
            Self::TvosSimulator => "appletvsimulator",
            Self::Watchos => "watchos",
            Self::WatchosSimulator => "watchsimulator",
            Self::Visionos => "xros",
            Self::VisionosSimulator => "xrsimulator",
        }
    }

    /// The OS component of a Metal driver `-target` triple.
    const fn metal_os(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Ios | Self::IosSimulator | Self::MacCatalyst => "ios",
            Self::Tvos | Self::TvosSimulator => "tvos",
            Self::Watchos | Self::WatchosSimulator => "watchos",
            Self::Visionos | Self::VisionosSimulator => "visionos",
        }
    }

    /// The environment suffix of a Metal driver `-target` triple.
    const fn metal_suffix(self) -> &'static str {
        match self {
            Self::MacCatalyst => "-macabi",
            Self::IosSimulator
            | Self::TvosSimulator
            | Self::WatchosSimulator
            | Self::VisionosSimulator => "-simulator",
            _ => "",
        }
    }
}

/// Maps cargo's target OS and triple to the Apple platform the shader
/// toolchain builds for. A `*-sim` triple or an `x86_64` Apple triple —
/// Intel simulator targets predate the `-sim` suffix — selects the simulator
/// platform. Mac Catalyst is the `ios` target OS with the `macabi` ABI
/// (`CARGO_CFG_TARGET_ABI`); it is checked before the simulator rule, which
/// `x86_64-apple-ios-macabi` would otherwise trip.
fn apple_platform(target_os: &str) -> ApplePlatform {
    let target = required_env("TARGET");
    let simulator = target.contains("-sim") || target.starts_with("x86_64-");
    let macabi = required_env("CARGO_CFG_TARGET_ABI") == "macabi";
    match target_os {
        "macos" => ApplePlatform::MacOs,
        "ios" if macabi => ApplePlatform::MacCatalyst,
        "ios" if simulator => ApplePlatform::IosSimulator,
        "ios" => ApplePlatform::Ios,
        "tvos" if simulator => ApplePlatform::TvosSimulator,
        "tvos" => ApplePlatform::Tvos,
        "watchos" if simulator => ApplePlatform::WatchosSimulator,
        "watchos" => ApplePlatform::Watchos,
        "visionos" if simulator => ApplePlatform::VisionosSimulator,
        "visionos" => ApplePlatform::Visionos,
        other => panic!("unsupported Apple shader target OS '{other}'"),
    }
}

pub(crate) fn apple_sdk(target_os: &str) -> &'static str {
    apple_platform(target_os).sdk()
}

/// The `-target` triple for the Metal driver, for example
/// `air64-apple-ios14.0-simulator`.
///
/// Without an explicit target the driver infers the platform and OS version
/// from whichever `*_DEPLOYMENT_TARGET` variables are in the environment, so
/// a workspace that declares more than one of them can compile an
/// `aarch64-apple-ios-sim` shader against a runtime library that does not
/// exist (`libmetal_rt_osxsim.a`). The platform instead comes from the same
/// cargo configuration that selects the SDK, and the OS version comes from
/// the compiler: `rustc --print deployment-target` honours the platform's
/// `*_DEPLOYMENT_TARGET` variable — whose name it prints, becoming the
/// `rerun-if-env-changed` — and otherwise reports rustc's default.
pub(crate) fn metal_target(target_os: &str) -> String {
    let platform = apple_platform(target_os);
    let target = required_env("TARGET");
    let output = Command::new(required_env("RUSTC"))
        .args(["--print", "deployment-target", "--target", &target])
        .output()
        .unwrap_or_else(|error| {
            panic!("failed to query the rustc deployment target for {target}: {error}")
        });
    assert!(
        output.status.success(),
        "rustc --print deployment-target failed for {target}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout =
        String::from_utf8(output.stdout).expect("rustc --print deployment-target must print UTF-8");
    let (variable, version) = stdout
        .trim()
        .split_once('=')
        .expect("rustc --print deployment-target must print <VAR>=<version>");
    println!("cargo:rerun-if-env-changed={variable}");
    format!(
        "air64-apple-{}{version}{}",
        platform.metal_os(),
        platform.metal_suffix()
    )
}
