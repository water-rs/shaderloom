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

pub(crate) fn apple_sdk(target_os: &str) -> &'static str {
    let target = required_env("TARGET");
    match target_os {
        "macos" => "macosx",
        "ios" if target.contains("-sim") || target.starts_with("x86_64-") => "iphonesimulator",
        "ios" => "iphoneos",
        "tvos" if target.contains("-sim") || target.starts_with("x86_64-") => "appletvsimulator",
        "tvos" => "appletvos",
        "watchos" if target.contains("-sim") || target.starts_with("x86_64-") => "watchsimulator",
        "watchos" => "watchos",
        "visionos" if target.contains("-sim") => "xrsimulator",
        "visionos" => "xros",
        other => panic!("unsupported Apple shader target OS '{other}'"),
    }
}
