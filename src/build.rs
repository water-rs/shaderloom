//! Build-time compilation of WGSL into backend-native embedded artifacts.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::packaged::{
    DxilEntry, PackagedManifest, apple_sdk, metal_language_standard, required_env, run_tool,
};
use naga::back::{hlsl, msl, spv};
use naga::valid::{Capabilities, ModuleInfo, ValidationFlags, Validator};
use naga::{AddressSpace, Handle, ImageClass, ImageDimension, Module, ShaderStage, TypeInner};
use proc_macro2::{Literal, TokenStream};
use quote::quote;

/// Compiles a complete WGSL module read from `source_path`.
///
/// The generated Rust expression is written to `<OUT_DIR>/<artifact_name>.rs`.
/// Native artifacts are emitted for every backend selectable on the Cargo
/// target: `MetalLib` on Apple, DXIL and SPIR-V on Windows, and SPIR-V on other
/// non-Web targets. WebGPU and GLES use the validated WGSL emitted alongside
/// the native artifacts because those APIs have no portable offline binary.
///
/// # Panics
///
/// Panics when the source is outside the package, cannot be read, is invalid
/// WGSL, or cannot be compiled for one of the target's native backends.
pub fn compile_wgsl_shader(source_path: impl AsRef<Path>, artifact_name: &str) {
    let manifest_dir = PathBuf::from(required_env("CARGO_MANIFEST_DIR"));
    let source_path = if source_path.as_ref().is_absolute() {
        source_path.as_ref().to_path_buf()
    } else {
        manifest_dir.join(source_path)
    };
    writeln!(
        std::io::stdout().lock(),
        "cargo:rerun-if-changed={}",
        source_path.display()
    )
    .expect("failed to emit Cargo shader source tracking directive");
    let source = fs::read_to_string(&source_path).unwrap_or_else(|error| {
        panic!(
            "failed to read shader source {}: {error}",
            source_path.display()
        )
    });
    let label = source_path.strip_prefix(&manifest_dir).unwrap_or_else(|_| {
        panic!(
            "shader source {} must be inside the package at {}",
            source_path.display(),
            manifest_dir.display()
        )
    });
    compile_wgsl_source(
        label
            .to_str()
            .expect("shader source path must contain valid UTF-8"),
        &source,
        artifact_name,
    );
}

/// Validates a complete WGSL module — parse plus full validation — without
/// emitting artifacts, the same front-end checks [`compile_wgsl_source`] runs.
///
/// # Panics
///
/// Panics when the source is invalid WGSL.
pub fn validate_wgsl_source(label: &str, source: &str) {
    let _ = parse_and_validate(label, source);
}

/// Compiles an already-composed complete WGSL module.
///
/// # Panics
///
/// Panics when the source is invalid WGSL, the artifact name is invalid, or
/// the shader cannot be compiled for one of the target's native backends.
pub fn compile_wgsl_source(label: &str, source: &str, artifact_name: &str) {
    assert_artifact_name(artifact_name);

    let (module, info) = parse_and_validate(label, source);
    let reflection = ShaderReflection::new(&module, &info);

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    write_wgsl(&out_dir, artifact_name, source, label);

    let target_arch = required_env("CARGO_CFG_TARGET_ARCH");
    let target_os = required_env("CARGO_CFG_TARGET_OS");
    let target_vendor = required_env("CARGO_CFG_TARGET_VENDOR");

    if target_arch != "wasm32" && target_vendor != "apple" {
        compile_spirv(
            &module,
            &info,
            &reflection,
            &out_dir.join(format!("{artifact_name}.spv")),
            label,
        );
    }
    if target_vendor == "apple" {
        compile_metallib(
            &module,
            &info,
            &reflection,
            &out_dir,
            artifact_name,
            label,
            &target_os,
        );
    }
    if target_os == "windows" {
        compile_dxil_entry_points(&module, &info, &reflection, &out_dir, artifact_name, label);
    }

    let rust = reflection.rust_expression(label, artifact_name, &IncludeRoot::OutDir);
    fs::write(out_dir.join(format!("{artifact_name}.rs")), rust).unwrap_or_else(|error| {
        panic!("failed to write generated Rust shader expression for {label}: {error}")
    });
}

/// Compiles a complete WGSL module into a package of checked-in artifacts,
/// held in memory.
///
/// This is the publish-time counterpart of [`compile_wgsl_source`]. The
/// returned [`ShaderPackage`] carries every *translated* artifact for
/// `packaged_dir` — a directory inside the crate that owns the shader — so
/// they can be committed and shipped in the `.crate`:
///
/// - `<name>.wgsl`, the validated module;
/// - `<name>.spv`, SPIR-V for Vulkan-family targets;
/// - `<name>.metal`, MSL for Apple targets;
/// - `<stem>.hlsl`, one file per entry point for Windows targets;
/// - `<name>.manifest`, the TOML-serialized [`PackagedManifest`] recording
///   the MSL language version and DXIL entry-point table the platform
///   toolchains need at build time;
/// - `<name>.rs`, the `CompiledShader` expression, which reads the checked-in
///   WGSL/SPIR-V through `CARGO_MANIFEST_DIR` and the build-time `metallib` /
///   `dxil` binaries through `OUT_DIR`.
///
/// Nothing is written: [`ShaderPackage::write`] persists the package and
/// [`ShaderPackage::assert_current`] verifies a checked-in copy is fresh.
///
/// The consumer's `build.rs` then calls only
/// [`crate::packaged::compile_packaged_metallib`] on Apple targets and
/// [`crate::packaged::compile_packaged_dxil`] on Windows — no `naga` — and
/// the crate's code `include!`s `<name>.rs` from `packaged_dir`.
///
/// `packaged_dir` is written verbatim into the generated `include_str!` /
/// `include_packaged_spirv!` paths, so it must be the directory's path
/// relative to the consuming crate's manifest dir (for example
/// `src/shaders/compiled`).
///
/// # Panics
///
/// Panics when the source is invalid WGSL or the artifact name is invalid.
///
/// [`PackagedManifest`]: crate::packaged::PackagedManifest
#[must_use]
pub fn package_wgsl(
    label: &str,
    source: &str,
    artifact_name: &str,
    packaged_dir: &str,
) -> ShaderPackage {
    assert_artifact_name(artifact_name);

    let (module, info) = parse_and_validate(label, source);
    let reflection = ShaderReflection::new(&module, &info);

    let mut files = Vec::new();
    files.push((format!("{artifact_name}.wgsl"), source.as_bytes().to_vec()));
    files.push((
        format!("{artifact_name}.spv"),
        spirv_bytes(&module, &info, &reflection, label),
    ));

    let (msl_source, language_version, translated_names) =
        generate_msl(&module, &info, &reflection, label);
    reflection.set_metal_names(&translated_names);
    files.push((format!("{artifact_name}.metal"), msl_source.into_bytes()));

    let hlsl_options = reflection.hlsl_options();
    let mut dxil = Vec::new();
    for entry in &reflection.entry_points {
        let (hlsl_source, translated_name) =
            generate_hlsl_entry_point(&module, &info, &hlsl_options, entry, label);
        let stem = entry.artifact_stem(artifact_name);
        files.push((format!("{stem}.hlsl"), hlsl_source.into_bytes()));
        dxil.push(DxilEntry {
            stem,
            profile: entry.dxil_profile().to_owned(),
            entry_name: translated_name,
        });
    }
    let manifest = toml::to_string(&PackagedManifest {
        msl_version: language_version,
        dxil,
    })
    .unwrap_or_else(|error| panic!("failed to serialize shader manifest for {label}: {error}"));
    files.push((format!("{artifact_name}.manifest"), manifest.into_bytes()));

    let rust =
        reflection.rust_expression(label, artifact_name, &IncludeRoot::Packaged(packaged_dir));
    files.push((format!("{artifact_name}.rs"), rust.into_bytes()));

    ShaderPackage {
        packaged_dir: PathBuf::from(packaged_dir),
        files,
    }
}

/// One packaged shader's complete artifact set, produced by
/// [`package_wgsl`] and not yet on disk.
#[derive(Debug)]
pub struct ShaderPackage {
    packaged_dir: PathBuf,
    files: Vec<(String, Vec<u8>)>,
}

impl ShaderPackage {
    /// Writes every artifact under `manifest_dir.join(packaged_dir)`,
    /// creating the directory when missing.
    ///
    /// # Panics
    ///
    /// Panics when a file cannot be written.
    pub fn write(&self, manifest_dir: &Path) {
        let dir = manifest_dir.join(&self.packaged_dir);
        fs::create_dir_all(&dir).unwrap_or_else(|error| {
            panic!(
                "failed to create packaged shader dir {}: {error}",
                dir.display()
            )
        });
        for (name, bytes) in &self.files {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap_or_else(|error| {
                panic!(
                    "failed to write packaged shader file {}: {error}",
                    path.display()
                )
            });
        }
    }

    /// Fails unless the checked-in copy under
    /// `manifest_dir.join(packaged_dir)` is byte-identical to this package.
    ///
    /// # Panics
    ///
    /// Panics naming every artifact that is missing or whose bytes differ,
    /// and points at the crate's `package-shaders.sh`.
    pub fn assert_current(&self, manifest_dir: &Path) {
        let dir = manifest_dir.join(&self.packaged_dir);
        let mut stale = Vec::new();
        for (name, bytes) in &self.files {
            let path = dir.join(name);
            match fs::read(&path) {
                Ok(on_disk) if on_disk == *bytes => {}
                Ok(_) => stale.push(format!("{} (differs)", path.display())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    stale.push(format!("{} (missing)", path.display()));
                }
                Err(error) => panic!(
                    "failed to read packaged shader file {}: {error}",
                    path.display()
                ),
            }
        }
        assert!(
            stale.is_empty(),
            "packaged shader artifacts are stale: {}\n\
             regenerate them by running the crate's package-shaders.sh",
            stale.join(", ")
        );
    }
}

/// Where the generated `CompiledShader` expression reads its WGSL and SPIR-V
/// artifacts.
///
/// `metallib` and `dxil` always come from `OUT_DIR`: their producers
/// (`xcrun`, `dxc`) only exist on the matching hosts, so the platform
/// binaries still materialize per-build.
enum IncludeRoot<'a> {
    OutDir,
    Packaged(&'a str),
}

fn parse_and_validate(label: &str, source: &str) -> (Module, ModuleInfo) {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|error| panic!("WGSL parse error in {label}: {error}"));
    let info = Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|error| panic!("WGSL validation error in {label}: {error}"));
    (module, info)
}

fn assert_artifact_name(name: &str) {
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "shader artifact name must contain only ASCII letters, digits, or underscores"
    );
}

fn write_wgsl(dir: &Path, artifact_name: &str, source: &str, label: &str) {
    fs::write(dir.join(format!("{artifact_name}.wgsl")), source)
        .unwrap_or_else(|error| panic!("failed to write generated WGSL for {label}: {error}"));
}

fn compile_spirv(
    module: &Module,
    info: &ModuleInfo,
    reflection: &ShaderReflection,
    output: &Path,
    label: &str,
) {
    fs::write(output, spirv_bytes(module, info, reflection, label))
        .unwrap_or_else(|error| panic!("failed to write SPIR-V for {label}: {error}"));
}

fn spirv_bytes(
    module: &Module,
    info: &ModuleInfo,
    reflection: &ShaderReflection,
    label: &str,
) -> Vec<u8> {
    let words = spv::write_vec(module, info, &reflection.spirv_options(), None)
        .unwrap_or_else(|error| panic!("SPIR-V generation failed for {label}: {error}"));
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

/// Pipeline options for the MSL artifact: every entry point is written
/// (`set_metal_names` zips the translated names positionally), and vertex
/// attributes stay `[[stage_in]]`.
///
/// wgpu-hal's Metal backend binds vertex data two ways: it always configures
/// an `MTLVertexDescriptor` from the pipeline's vertex buffers — which feeds
/// `stage_in` attributes for any module, passthrough included — and it binds
/// the buffers themselves at `MAX_BUFFERS - 1 - i` for naga's vertex-pulling
/// transform. Pulling is not available to a build-time artifact anyway: it
/// requires the pipeline's `vertex_buffer_mappings` (strides, step modes,
/// attribute offsets and formats), which are only known when the pipeline is
/// created, and naga asserts a non-zero stride for each mapping.
/// `allow_and_force_point_size` is likewise pipeline-specific — wgpu-hal sets
/// it only for point topologies — so it stays off.
fn msl_pipeline_options() -> msl::PipelineOptions {
    msl::PipelineOptions {
        entry_point: None,
        allow_and_force_point_size: false,
        vertex_pulling_transform: false,
        vertex_buffer_mappings: Vec::new(),
        ..Default::default()
    }
}

fn generate_msl(
    module: &Module,
    info: &ModuleInfo,
    reflection: &ShaderReflection,
    label: &str,
) -> (String, (u8, u8), Vec<String>) {
    let mut options = reflection.msl_options();
    let pipeline_options = msl_pipeline_options();
    let mut versions = MSL_LANGUAGE_VERSIONS.iter().copied();
    let (source, translation, language_version) = loop {
        let version = versions.next().unwrap_or_else(|| {
            panic!("MSL generation failed for {label}: no supported Metal language version")
        });
        options.lang_version = version;
        match msl::write_string(module, info, &options, &pipeline_options) {
            Ok((source, translation)) => break (source, translation, version),
            Err(error) if is_msl_version_error(&error) => {}
            Err(error) => panic!("MSL generation failed for {label}: {error}"),
        }
    };

    let translated_names = translation
        .entry_point_names
        .into_iter()
        .map(|name| {
            name.unwrap_or_else(|error| {
                panic!("MSL entry-point translation failed for {label}: {error}")
            })
        })
        .collect::<Vec<_>>();
    (source, language_version, translated_names)
}

fn compile_metallib(
    module: &Module,
    info: &ModuleInfo,
    reflection: &ShaderReflection,
    out_dir: &Path,
    artifact_name: &str,
    label: &str,
    target_os: &str,
) {
    let (source, language_version, translated_names) =
        generate_msl(module, info, reflection, label);
    reflection.set_metal_names(&translated_names);

    let source_path = out_dir.join(format!("{artifact_name}.metal"));
    let air_path = out_dir.join(format!("{artifact_name}.air"));
    let metallib_path = out_dir.join(format!("{artifact_name}.metallib"));
    fs::write(&source_path, source)
        .unwrap_or_else(|error| panic!("failed to write generated MSL for {label}: {error}"));

    let sdk = apple_sdk(target_os);
    let standard = metal_language_standard(target_os, language_version);
    run_tool(
        Command::new("xcrun")
            .args(["--sdk", sdk, "metal", "-c"])
            .arg(standard)
            .arg(&source_path)
            .arg("-o")
            .arg(&air_path),
        "Metal compilation",
        label,
    );
    run_tool(
        Command::new("xcrun")
            .args(["--sdk", sdk, "metallib"])
            .arg(&air_path)
            .arg("-o")
            .arg(&metallib_path),
        "Metal library linking",
        label,
    );
}

const MSL_LANGUAGE_VERSIONS: &[(u8, u8)] = &[
    (1, 0),
    (1, 1),
    (1, 2),
    (2, 0),
    (2, 1),
    (2, 2),
    (2, 3),
    (2, 4),
    (3, 0),
    (3, 1),
    (3, 2),
    (4, 0),
];

const fn is_msl_version_error(error: &msl::Error) -> bool {
    matches!(
        error,
        msl::Error::UnsupportedAttribute(_)
            | msl::Error::UnsupportedFunction(_)
            | msl::Error::UnsupportedWritableStorageBuffer
            | msl::Error::UnsupportedWritableStorageTexture(_)
            | msl::Error::UnsupportedRWStorageTexture
            | msl::Error::UnsupportedArrayOf(_)
            | msl::Error::UnsupportedRayTracing
            | msl::Error::UnsupportedCooperativeMatrix
    )
}

fn compile_dxil_entry_points(
    module: &Module,
    info: &ModuleInfo,
    reflection: &ShaderReflection,
    out_dir: &Path,
    artifact_name: &str,
    label: &str,
) {
    let options = reflection.hlsl_options();
    for entry in &reflection.entry_points {
        let (source, translated_name) =
            generate_hlsl_entry_point(module, info, &options, entry, label);

        let stem = entry.artifact_stem(artifact_name);
        let hlsl_path = out_dir.join(format!("{stem}.hlsl"));
        let dxil_path = out_dir.join(format!("{stem}.dxil"));
        fs::write(&hlsl_path, source)
            .unwrap_or_else(|error| panic!("failed to write generated HLSL for {label}: {error}"));

        run_tool(
            Command::new("dxc")
                .args(["-T", entry.dxil_profile(), "-E"])
                .arg(translated_name)
                .args(["-Qstrip_debug", "-Qstrip_reflect", "-Fo"])
                .arg(&dxil_path)
                .arg(&hlsl_path),
            "DXIL compilation",
            label,
        );
    }
}

fn generate_hlsl_entry_point(
    module: &Module,
    info: &ModuleInfo,
    options: &hlsl::Options,
    entry: &ReflectedEntryPoint,
    label: &str,
) -> (String, String) {
    let pipeline_options = hlsl::PipelineOptions {
        entry_point: Some((entry.stage, entry.name.clone())),
    };
    let mut source = String::new();
    let mut writer = hlsl::Writer::new(&mut source, options, &pipeline_options);
    let mut output = writer
        .write(module, info, None)
        .unwrap_or_else(|error| panic!("HLSL generation failed for {label}: {error}"));
    assert_eq!(
        output.entry_point_names.len(),
        1,
        "HLSL generation for {label} must emit exactly one entry point"
    );
    let translated_name = output
        .entry_point_names
        .pop()
        .expect("one HLSL entry point was asserted")
        .unwrap_or_else(|error| panic!("HLSL entry-point translation failed for {label}: {error}"));
    (source, translated_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    UniformBuffer,
    StorageBuffer { read_only: bool },
    Sampler,
    Texture,
    StorageTexture { mutable: bool },
}

#[derive(Debug, Clone)]
struct ReflectedBinding {
    group: u32,
    binding: u32,
    visibility: u32,
    kind: BindingKind,
    rust_type: String,
}

#[derive(Debug, Clone)]
struct ReflectedEntryPoint {
    name: String,
    metal_name: std::cell::RefCell<String>,
    stage: ShaderStage,
    workgroup_size: [u32; 3],
}

impl ReflectedEntryPoint {
    fn artifact_stem(&self, shader_stem: &str) -> String {
        format!(
            "{shader_stem}_{}_{}",
            stage_token(self.stage),
            sanitize_identifier(&self.name)
        )
    }

    fn dxil_profile(&self) -> &'static str {
        match self.stage {
            ShaderStage::Vertex => "vs_6_0",
            ShaderStage::Fragment => "ps_6_0",
            ShaderStage::Compute => "cs_6_0",
            other => panic!("DXIL AOT does not support {other:?} shader stages"),
        }
    }
}

#[derive(Debug)]
struct ShaderReflection {
    bindings: Vec<ReflectedBinding>,
    entry_points: Vec<ReflectedEntryPoint>,
}

impl ShaderReflection {
    fn new(module: &Module, info: &ModuleInfo) -> Self {
        let mut bindings = Vec::new();
        for (handle, global) in module.global_variables.iter() {
            let Some(resource) = global.binding else {
                continue;
            };
            let visibility = module
                .entry_points
                .iter()
                .enumerate()
                .filter(|(index, _)| !info.get_entry_point(*index)[handle].is_empty())
                .fold(0, |bits, (_, entry)| bits | stage_visibility(entry.stage));
            assert_ne!(
                visibility, 0,
                "bound global group {} binding {} is unused by every entry point",
                resource.group, resource.binding
            );
            let (kind, rust_type) = reflect_binding_type(module, handle);
            bindings.push(ReflectedBinding {
                group: resource.group,
                binding: resource.binding,
                visibility,
                kind,
                rust_type,
            });
        }
        bindings.sort_by_key(|binding| (binding.group, binding.binding));

        let entry_points = module
            .entry_points
            .iter()
            .map(|entry| ReflectedEntryPoint {
                name: entry.name.clone(),
                metal_name: std::cell::RefCell::new(entry.name.clone()),
                stage: entry.stage,
                workgroup_size: entry.workgroup_size,
            })
            .collect();

        Self {
            bindings,
            entry_points,
        }
    }

    fn set_metal_names(&self, translated_names: &[String]) {
        assert_eq!(
            self.entry_points.len(),
            translated_names.len(),
            "MSL translation must report every shader entry point"
        );
        for (entry, translated) in self.entry_points.iter().zip(translated_names) {
            entry.metal_name.borrow_mut().clone_from(translated);
        }
    }

    /// SPIR-V writer options equivalent to the ones wgpu's Vulkan backend uses.
    ///
    /// `spv::Options::default()` is naga's default, not wgpu's, and the two
    /// disagree on flags that change generated code. The reference is
    /// `wgpu_hal::vulkan::Adapter::open`, which builds its options from
    /// `spv::WriterFlags::empty()` and then opts in explicitly; every field below
    /// is set from that same starting point.
    ///
    /// Ahead-of-time compilation has no device, so each device-dependent option
    /// takes the value that is valid on every Vulkan device wgpu supports rather
    /// than the value wgpu would pick for one particular adapter:
    ///
    /// - `lang_version` is SPIR-V 1.0, what wgpu selects for a Vulkan 1.0 device
    ///   and what every later device still accepts.
    /// - `LABEL_VARYINGS` stays off. wgpu sets it except on Qualcomm, whose
    ///   driver mishandles the names; the build cannot know the vendor and the
    ///   flag only emits `OpName` decorations.
    /// - `DEBUG` and `PRINT_ON_RAY_QUERY_INITIALIZATION_FAIL` stay off. wgpu ties
    ///   them to `InstanceFlags::DEBUG`, and leaving them off also keeps the
    ///   artifact byte-identical between debug and release build-script runs.
    /// - `use_storage_input_output_16` stays off, so `f16` shader I/O is
    ///   polyfilled through `f32` instead of requiring a capability the target
    ///   device may not advertise.
    /// - `zero_initialize_workgroup_memory` polyfills, because the native mode
    ///   needs an extension wgpu only uses when the device reports it.
    /// - `capabilities` is `None`. It is a writer-side gate that rejects modules
    ///   needing more than the listed capabilities and never changes generated
    ///   code, and the build already validates with `Capabilities::all()`.
    /// - `task_dispatch_limits` is `None`, since those limits come from the
    ///   device.
    ///
    /// Bounds checks, loop bounding, ray-query initialization tracking,
    /// integer-division checks and mesh-shader index clamping are all off. A
    /// passthrough binary is a trusted module, and wgpu drops exactly these
    /// checks for trusted modules in `wgpu_hal::vulkan::Device::compile_stage`;
    /// the MSL and HLSL options above take the same position.
    /// `trace_ray_argument_validation` is not a runtime check — wgpu enables it
    /// unconditionally and never drops it — so it stays on.
    ///
    /// `binding_map` reproduces the remapping wgpu performs: `wgpu-core` sorts a
    /// bind group's entries by binding number and `wgpu-hal` then numbers the
    /// Vulkan descriptor bindings densely from zero in that order, so a shader
    /// whose `@binding` numbers are not contiguous would otherwise decorate its
    /// globals with numbers the descriptor set layout never uses. With the map
    /// populated, `fake_missing_bindings` is off, so a binding the reflection
    /// missed fails the build instead of emitting an invalid module.
    fn spirv_options(&self) -> spv::Options<'static> {
        let mut binding_map = spv::BindingMap::default();
        let mut group_bindings = BTreeMap::<u32, u32>::new();
        for binding in &self.bindings {
            let next = group_bindings.entry(binding.group).or_default();
            binding_map.insert(
                naga::ResourceBinding {
                    group: binding.group,
                    binding: binding.binding,
                },
                spv::BindingInfo {
                    descriptor_set: binding.group,
                    binding: *next,
                    binding_array_size: None,
                },
            );
            *next += 1;
        }

        spv::Options {
            lang_version: (1, 0),
            flags: spv::WriterFlags::FORCE_POINT_SIZE,
            fake_missing_bindings: false,
            binding_map,
            capabilities: None,
            bounds_check_policies: naga::proc::BoundsCheckPolicies {
                index: naga::proc::BoundsCheckPolicy::Unchecked,
                buffer: naga::proc::BoundsCheckPolicy::Unchecked,
                image_load: naga::proc::BoundsCheckPolicy::Unchecked,
                binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
            },
            zero_initialize_workgroup_memory: spv::ZeroInitializeWorkgroupMemoryMode::Polyfill,
            force_loop_bounding: false,
            ray_query_initialization_tracking: false,
            use_storage_input_output_16: false,
            debug_info: None,
            task_dispatch_limits: None,
            mesh_shader_primitive_indices_clamp: false,
            trace_ray_argument_validation: true,
            emit_int_div_checks: false,
        }
    }

    fn msl_options(&self) -> msl::Options {
        let mut stage_resources = BTreeMap::new();
        for stage in [
            ShaderStage::Vertex,
            ShaderStage::Fragment,
            ShaderStage::Compute,
        ] {
            let visibility = stage_visibility(stage);
            let mut buffers = 0;
            let mut textures = 0;
            let mut samplers = 0;
            let needs_sizes_buffer = stage == ShaderStage::Vertex
                || self.bindings.iter().any(|binding| {
                    binding.visibility & visibility != 0
                        && matches!(binding.kind, BindingKind::StorageBuffer { .. })
                });
            let mut resources = BTreeMap::new();
            for binding in &self.bindings {
                if binding.visibility & visibility == 0 {
                    continue;
                }
                let mut target = msl::BindTarget::default();
                match binding.kind {
                    BindingKind::UniformBuffer => {
                        target.buffer = Some(buffers);
                        buffers += 1;
                    }
                    BindingKind::StorageBuffer { read_only } => {
                        target.buffer = Some(buffers);
                        target.mutable = !read_only;
                        buffers += 1;
                    }
                    BindingKind::Sampler => {
                        target.sampler = Some(msl::BindSamplerTarget::Resource(samplers));
                        samplers += 1;
                    }
                    BindingKind::Texture => {
                        target.texture = Some(textures);
                        textures += 1;
                    }
                    BindingKind::StorageTexture { mutable } => {
                        target.texture = Some(textures);
                        target.mutable = mutable;
                        textures += 1;
                    }
                }
                resources.insert(
                    naga::ResourceBinding {
                        group: binding.group,
                        binding: binding.binding,
                    },
                    target,
                );
            }
            stage_resources.insert(
                stage,
                msl::EntryPointResources {
                    resources,
                    sizes_buffer: needs_sizes_buffer.then_some(buffers),
                    ..Default::default()
                },
            );
        }

        let per_entry_point_map = self
            .entry_points
            .iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    stage_resources
                        .get(&entry.stage)
                        .expect("supported MSL shader stage")
                        .clone(),
                )
            })
            .collect();

        msl::Options {
            per_entry_point_map,
            fake_missing_bindings: false,
            bounds_check_policies: naga::proc::BoundsCheckPolicies {
                index: naga::proc::BoundsCheckPolicy::Unchecked,
                buffer: naga::proc::BoundsCheckPolicy::Unchecked,
                image_load: naga::proc::BoundsCheckPolicy::Unchecked,
                binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
            },
            force_loop_bounding: false,
            task_dispatch_limits: None,
            mesh_shader_primitive_indices_clamp: false,
            ray_query_initialization_tracking: false,
            emit_int_div_checks: false,
            ..Default::default()
        }
    }

    fn hlsl_options(&self) -> hlsl::Options {
        let mut binding_map = hlsl::BindingMap::default();
        let mut sampler_buffer_binding_map = hlsl::SamplerIndexBufferBindingMap::default();
        let mut cbv = hlsl::BindTarget::default();
        let mut srv = hlsl::BindTarget::default();
        let mut uav = hlsl::BindTarget::default();

        let groups = self
            .bindings
            .iter()
            .map(|binding| binding.group)
            .max()
            .map_or(0, |group| group + 1);
        for group in 0..groups {
            let mut sampler_index = 0;
            for binding in self
                .bindings
                .iter()
                .filter(|binding| binding.group == group)
            {
                let target = match binding.kind {
                    BindingKind::UniformBuffer => next_hlsl_target(&mut cbv),
                    BindingKind::StorageBuffer { read_only: true } | BindingKind::Texture => {
                        next_hlsl_target(&mut srv)
                    }
                    BindingKind::StorageBuffer { read_only: false }
                    | BindingKind::StorageTexture { mutable: true } => next_hlsl_target(&mut uav),
                    BindingKind::StorageTexture { mutable: false } => next_hlsl_target(&mut srv),
                    BindingKind::Sampler => {
                        let target = hlsl::BindTarget {
                            space: u8::MAX,
                            register: sampler_index,
                            ..Default::default()
                        };
                        sampler_index += 1;
                        target
                    }
                };
                binding_map.insert(
                    naga::ResourceBinding {
                        group: binding.group,
                        binding: binding.binding,
                    },
                    target,
                );
            }
            if sampler_index != 0 {
                sampler_buffer_binding_map.insert(
                    hlsl::SamplerIndexBufferKey { group },
                    next_hlsl_target(&mut srv),
                );
            }
        }

        let special_constants_binding = Some(next_hlsl_target(&mut cbv));
        hlsl::Options {
            shader_model: hlsl::ShaderModel::V6_0,
            binding_map,
            fake_missing_bindings: false,
            special_constants_binding,
            sampler_heap_target: hlsl::SamplerHeapBindTargets {
                standard_samplers: hlsl::BindTarget {
                    register: 0,
                    space: 0,
                    ..Default::default()
                },
                comparison_samplers: hlsl::BindTarget {
                    register: 2048,
                    space: 0,
                    ..Default::default()
                },
            },
            sampler_buffer_binding_map,
            zero_initialize_workgroup_memory: true,
            restrict_indexing: false,
            force_loop_bounding: false,
            ray_query_initialization_tracking: false,
            task_dispatch_limits: None,
            mesh_shader_primitive_indices_clamp: false,
            ..Default::default()
        }
    }

    fn rust_expression(
        &self,
        label: &str,
        artifact_name: &str,
        include_root: &IncludeRoot<'_>,
    ) -> String {
        let (wgsl_include, spv_include) = match include_root {
            IncludeRoot::OutDir => {
                let wgsl = format!("/{artifact_name}.wgsl");
                let spv = format!("{artifact_name}.spv");
                (
                    quote! { include_str!(concat!(env!("OUT_DIR"), #wgsl)) },
                    quote! { ::shaderloom::include_compiled_spirv!(#spv) },
                )
            }
            IncludeRoot::Packaged(root) => {
                let wgsl = format!("/{root}/{artifact_name}.wgsl");
                let spv = format!("{root}/{artifact_name}.spv");
                (
                    quote! { include_str!(concat!(env!("CARGO_MANIFEST_DIR"), #wgsl)) },
                    quote! { ::shaderloom::include_packaged_spirv!(#spv) },
                )
            }
        };
        let metallib = format!("{artifact_name}.metallib");

        let entry_points = self.entry_points.iter().map(|entry| {
            let stem = entry.artifact_stem(artifact_name);
            let dxil = format!("{stem}.dxil");
            let name = &entry.name;
            let stage = rust_stage(entry.stage);
            let metal_name = entry.metal_name.borrow().clone();
            let workgroup = entry.workgroup_size.map(Literal::u32_unsuffixed);
            quote! {
                ::shaderloom::CompiledEntryPoint {
                    name: #name,
                    stage: #stage,
                    metal_name: #metal_name,
                    workgroup_size: (#(#workgroup),*),
                    dxil: ::shaderloom::include_compiled_dxil!(#dxil),
                }
            }
        });

        let group_count = self
            .bindings
            .iter()
            .map(|binding| binding.group)
            .max()
            .map_or(0, |group| group + 1);
        let bind_groups = (0..group_count).map(|group| {
            let entries = self
                .bindings
                .iter()
                .filter(|binding| binding.group == group)
                .map(|binding| {
                    let index = Literal::u32_unsuffixed(binding.binding);
                    let visibility = Literal::u32_unsuffixed(binding.visibility);
                    let ty = binding
                        .rust_type
                        .parse::<TokenStream>()
                        .expect("a reflected wgpu binding type must be valid Rust");
                    quote! {
                        ::shaderloom::wgpu::BindGroupLayoutEntry {
                            binding: #index,
                            visibility: ::shaderloom::wgpu::ShaderStages::from_bits_retain(#visibility),
                            ty: #ty,
                            count: None,
                        }
                    }
                });
            quote! {
                ::shaderloom::ReflectedBindGroup {
                    entries: &[#(#entries),*],
                }
            }
        });

        format_rust_tokens(&quote! {
            ::shaderloom::CompiledShader::new(
                #label,
                #wgsl_include,
                #spv_include,
                ::shaderloom::include_compiled_metallib!(#metallib),
                &[#(#entry_points),*],
                &[#(#bind_groups),*],
            )
        })
    }
}

/// Formats a generated expression through `prettyplease`.
///
/// `prettyplease` only accepts a whole `syn::File`, so the expression is
/// wrapped in a throwaway `const` item, formatted, and unwrapped again. The
/// wrapper is an assignment whose `=` is the first one in the output: the
/// generated expression carries none of its own.
fn format_rust_tokens(tokens: &TokenStream) -> String {
    let file: syn::File = syn::parse_quote!(const __SHADERLOOM_FORMAT: () = #tokens;);
    let text = prettyplease::unparse(&file);
    let after_marker = text
        .find("__SHADERLOOM_FORMAT")
        .map(|index| &text[index + "__SHADERLOOM_FORMAT".len()..])
        .expect("the const wrapper survives prettyplease");
    let after_equals = after_marker
        .split_once('=')
        .expect("the const wrapper always contains '='")
        .1;
    after_equals
        .trim_end()
        .strip_suffix(';')
        .expect("prettyplease terminates the const item with ';'")
        .trim()
        .to_owned()
}

const fn next_hlsl_target(counter: &mut hlsl::BindTarget) -> hlsl::BindTarget {
    let target = *counter;
    counter.register += 1;
    target
}

fn reflect_binding_type(
    module: &Module,
    handle: Handle<naga::GlobalVariable>,
) -> (BindingKind, String) {
    let global = &module.global_variables[handle];
    match global.space {
        AddressSpace::Uniform => (
            BindingKind::UniformBuffer,
            "::shaderloom::wgpu::BindingType::Buffer { ty: ::shaderloom::wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }".to_owned(),
        ),
        AddressSpace::Storage { access } => {
            let read_only = !access.intersects(naga::StorageAccess::STORE | naga::StorageAccess::ATOMIC);
            (
                BindingKind::StorageBuffer { read_only },
                format!(
                    "::shaderloom::wgpu::BindingType::Buffer {{ ty: ::shaderloom::wgpu::BufferBindingType::Storage {{ read_only: {read_only} }}, has_dynamic_offset: false, min_binding_size: None }}"
                ),
            )
        }
        AddressSpace::Handle => reflect_handle_type(module, global.ty),
        other => panic!("unsupported bound shader address space {other:?}"),
    }
}

fn reflect_handle_type(module: &Module, ty: Handle<naga::Type>) -> (BindingKind, String) {
    match module.types[ty].inner {
        TypeInner::Sampler { comparison } => (
            BindingKind::Sampler,
            format!(
                "::shaderloom::wgpu::BindingType::Sampler({})",
                if comparison {
                    "::shaderloom::wgpu::SamplerBindingType::Comparison"
                } else {
                    "::shaderloom::wgpu::SamplerBindingType::Filtering"
                }
            ),
        ),
        TypeInner::Image {
            dim,
            arrayed,
            class,
        } => reflect_image_type(dim, arrayed, class),
        ref other => panic!("unsupported bound handle type {other:?}"),
    }
}

fn reflect_image_type(
    dimension: ImageDimension,
    arrayed: bool,
    class: ImageClass,
) -> (BindingKind, String) {
    let view_dimension = rust_view_dimension(dimension, arrayed);
    match class {
        ImageClass::Sampled { kind, multi } => {
            let sample_type = match kind {
                naga::ScalarKind::Float | naga::ScalarKind::AbstractFloat => {
                    "::shaderloom::wgpu::TextureSampleType::Float { filterable: true }"
                }
                naga::ScalarKind::Sint | naga::ScalarKind::AbstractInt => {
                    "::shaderloom::wgpu::TextureSampleType::Sint"
                }
                naga::ScalarKind::Uint => "::shaderloom::wgpu::TextureSampleType::Uint",
                naga::ScalarKind::Bool => {
                    panic!("boolean sampled textures are not supported")
                }
            };
            (
                BindingKind::Texture,
                format!(
                    "::shaderloom::wgpu::BindingType::Texture {{ sample_type: {sample_type}, view_dimension: {view_dimension}, multisampled: {multi} }}"
                ),
            )
        }
        ImageClass::Depth { multi } => (
            BindingKind::Texture,
            format!(
                "::shaderloom::wgpu::BindingType::Texture {{ sample_type: ::shaderloom::wgpu::TextureSampleType::Depth, view_dimension: {view_dimension}, multisampled: {multi} }}"
            ),
        ),
        ImageClass::Storage { format, access } => {
            let mutable =
                access.intersects(naga::StorageAccess::STORE | naga::StorageAccess::ATOMIC);
            let rust_access = if access.contains(naga::StorageAccess::ATOMIC) {
                "::shaderloom::wgpu::StorageTextureAccess::Atomic"
            } else if access.contains(naga::StorageAccess::LOAD)
                && access.contains(naga::StorageAccess::STORE)
            {
                "::shaderloom::wgpu::StorageTextureAccess::ReadWrite"
            } else if access.contains(naga::StorageAccess::STORE) {
                "::shaderloom::wgpu::StorageTextureAccess::WriteOnly"
            } else {
                "::shaderloom::wgpu::StorageTextureAccess::ReadOnly"
            };
            (
                BindingKind::StorageTexture { mutable },
                format!(
                    "::shaderloom::wgpu::BindingType::StorageTexture {{ access: {rust_access}, format: ::shaderloom::wgpu::TextureFormat::{format:?}, view_dimension: {view_dimension} }}"
                ),
            )
        }
        ImageClass::External => panic!("external textures are not supported by shader AOT"),
    }
}

fn rust_view_dimension(dimension: ImageDimension, arrayed: bool) -> &'static str {
    match (dimension, arrayed) {
        (ImageDimension::D1, false) => "::shaderloom::wgpu::TextureViewDimension::D1",
        (ImageDimension::D2, false) => "::shaderloom::wgpu::TextureViewDimension::D2",
        (ImageDimension::D2, true) => "::shaderloom::wgpu::TextureViewDimension::D2Array",
        (ImageDimension::D3, false) => "::shaderloom::wgpu::TextureViewDimension::D3",
        (ImageDimension::Cube, false) => "::shaderloom::wgpu::TextureViewDimension::Cube",
        (ImageDimension::Cube, true) => "::shaderloom::wgpu::TextureViewDimension::CubeArray",
        (ImageDimension::D1 | ImageDimension::D3, true) => {
            panic!("wgpu does not support arrayed {dimension:?} texture bindings")
        }
    }
}

fn stage_visibility(stage: ShaderStage) -> u32 {
    match stage {
        ShaderStage::Vertex => 1,
        ShaderStage::Fragment => 2,
        ShaderStage::Compute => 4,
        other => panic!("shader AOT does not support {other:?} stages"),
    }
}

fn stage_token(stage: ShaderStage) -> &'static str {
    match stage {
        ShaderStage::Vertex => "vertex",
        ShaderStage::Fragment => "fragment",
        ShaderStage::Compute => "compute",
        other => panic!("shader AOT does not support {other:?} stages"),
    }
}

fn rust_stage(stage: ShaderStage) -> TokenStream {
    match stage {
        ShaderStage::Vertex => quote! { ::shaderloom::ShaderStage::Vertex },
        ShaderStage::Fragment => quote! { ::shaderloom::ShaderStage::Fragment },
        ShaderStage::Compute => quote! { ::shaderloom::ShaderStage::Compute },
        other => panic!("shader AOT does not support {other:?} stages"),
    }
}

fn sanitize_identifier(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SHADER: &str = include_str!("shader_test.wgsl");
    const SPARSE_BINDING_SHADER: &str = include_str!("shader_test_sparse_bindings.wgsl");

    fn parse_and_reflect(source: &str) -> (Module, ModuleInfo, ShaderReflection) {
        let module = naga::front::wgsl::parse_str(source).expect("test WGSL must parse");
        let info = Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("test WGSL must validate");
        let reflection = ShaderReflection::new(&module, &info);
        (module, info, reflection)
    }

    /// Opcodes of every instruction in a SPIR-V module.
    ///
    /// A module is a five-word header followed by instructions whose first word
    /// packs the word count in the high half and the opcode in the low half.
    fn spirv_opcodes(words: &[u32]) -> Vec<u16> {
        let mut opcodes = Vec::new();
        let mut cursor = 5;
        while cursor < words.len() {
            let word = words[cursor];
            let length = (word >> 16) as usize;
            assert!(length > 0, "SPIR-V instruction must have a non-zero length");
            opcodes.push((word & 0xffff) as u16);
            cursor += length;
        }
        assert_eq!(
            cursor,
            words.len(),
            "SPIR-V instructions must tile the module"
        );
        opcodes
    }

    #[test]
    fn reflection_drives_native_binding_maps_and_rust_layout() {
        let (module, info, reflection) = parse_and_reflect(TEST_SHADER);

        assert_eq!(reflection.bindings.len(), 1);
        assert_eq!(reflection.bindings[0].visibility, 3);
        assert!(
            reflection
                .msl_options()
                .per_entry_point_map
                .contains_key("vs_main")
        );
        assert!(
            reflection
                .hlsl_options()
                .binding_map
                .contains_key(&naga::ResourceBinding {
                    group: 0,
                    binding: 0,
                })
        );
        let hlsl_options = reflection.hlsl_options();
        for entry in &reflection.entry_points {
            let (source, translated_name) =
                generate_hlsl_entry_point(&module, &info, &hlsl_options, entry, "test WGSL");
            assert_ne!(source, "");
            assert!(source.contains(&translated_name));
        }
        let rust = reflection.rust_expression("test.wgsl", "test_shader", &IncludeRoot::OutDir);
        assert!(rust.contains("CompiledShader::new"));
        assert!(rust.contains("ShaderStages::from_bits_retain(3)"));
    }

    /// The emitted module must not negate the clip-space Y of `@builtin(position)`.
    ///
    /// wgpu's Vulkan backend maps WebGPU clip space with a negative-height
    /// viewport, so a writer-side flip would compose with it and turn every frame
    /// upside down. The fixture contains no floating-point negation of its own,
    /// which makes `OpFNegate` a direct witness of the epilogue flip; compiling
    /// the same module with naga's defaults is the positive control that proves
    /// this test can still see one.
    #[test]
    fn emitted_spirv_does_not_flip_clip_space_y() {
        let (module, info, reflection) = parse_and_reflect(TEST_SHADER);

        let words = spv::write_vec(&module, &info, &reflection.spirv_options(), None)
            .expect("test WGSL must compile to SPIR-V");
        assert!(
            !spirv_opcodes(&words).contains(&(spirv::Op::FNegate as u16)),
            "shaderloom SPIR-V must not negate clip-space Y; wgpu flips with the viewport"
        );

        let flipped = spv::write_vec(&module, &info, &spv::Options::default(), None)
            .expect("test WGSL must compile to SPIR-V");
        assert!(
            spirv_opcodes(&flipped).contains(&(spirv::Op::FNegate as u16)),
            "naga's default writer options must still emit the flip this test guards against"
        );
    }

    /// Every option that changes generated code must match wgpu's Vulkan backend.
    #[test]
    fn spirv_options_match_the_vulkan_backend() {
        let (_, _, reflection) = parse_and_reflect(TEST_SHADER);
        let options = reflection.spirv_options();

        assert!(
            !options
                .flags
                .contains(spv::WriterFlags::ADJUST_COORDINATE_SPACE)
        );
        assert!(!options.flags.contains(spv::WriterFlags::CLAMP_FRAG_DEPTH));
        assert!(!options.flags.contains(spv::WriterFlags::DEBUG));
        assert!(!options.flags.contains(spv::WriterFlags::LABEL_VARYINGS));
        assert!(options.flags.contains(spv::WriterFlags::FORCE_POINT_SIZE));
        assert_eq!(options.lang_version, (1, 0));
        assert!(!options.fake_missing_bindings);
        assert!(!options.use_storage_input_output_16);
        assert!(!options.force_loop_bounding);
        assert!(!options.emit_int_div_checks);
        assert!(options.trace_ray_argument_validation);
        assert_eq!(
            options.bounds_check_policies,
            naga::proc::BoundsCheckPolicies {
                index: naga::proc::BoundsCheckPolicy::Unchecked,
                buffer: naga::proc::BoundsCheckPolicy::Unchecked,
                image_load: naga::proc::BoundsCheckPolicy::Unchecked,
                binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
            }
        );
    }

    /// The MSL artifact keeps `[[stage_in]]` vertex inputs: wgpu-hal feeds
    /// them through the `MTLVertexDescriptor` it configures for any pipeline
    /// with vertex buffers, while its vertex-pulling transform needs
    /// `vertex_buffer_mappings` that only exist at pipeline-creation time.
    #[test]
    fn msl_pipeline_options_keep_stage_in_vertex_inputs() {
        let options = msl_pipeline_options();

        assert!(options.entry_point.is_none());
        assert!(!options.vertex_pulling_transform);
        assert!(options.vertex_buffer_mappings.is_empty());
        assert!(!options.allow_and_force_point_size);
        assert!(options.binding_array_length_map.is_empty());
    }

    /// Descriptor bindings are numbered densely, the way wgpu numbers them.
    #[test]
    fn spirv_binding_map_matches_wgpu_descriptor_numbering() {
        let (module, info, reflection) = parse_and_reflect(SPARSE_BINDING_SHADER);
        let options = reflection.spirv_options();

        let target = |group, binding| {
            *options
                .binding_map
                .get(&naga::ResourceBinding { group, binding })
                .expect("every reflected binding must be mapped")
        };
        assert_eq!(target(0, 0).binding, 0);
        assert_eq!(
            target(0, 3).binding,
            1,
            "wgpu allocates @binding(3) densely"
        );
        assert_eq!(target(0, 3).descriptor_set, 0);

        spv::write_vec(&module, &info, &options, None)
            .expect("a fully mapped module must compile without faked bindings");
    }
}
