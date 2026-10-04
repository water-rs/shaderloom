//! `shaderloom` command line: WGSL → a package of checked-in artifacts.
//!
//! Requires the `build` feature (`required-features`), since packaging runs
//! the full `naga` front-end and the MSL/HLSL/SPIR-V translators.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
shaderloom — WGSL packaging for checked-in shader artifacts

USAGE:
    shaderloom wgsl-package [OPTIONS] <SOURCE>

Arguments:
    <SOURCE>            WGSL file to package (read after all --prepend files)

Options:
    --name NAME         Artifact name: ASCII letters, digits, underscores
                        (defaults to the source file stem)
    --out DIR           Artifact directory, relative to the crate's manifest
                        dir (e.g. src/shaders/compiled); also the include root
                        emitted into <name>.rs
    --prepend FILE      WGSL file prepended to the source, repeatable
                        (e.g. a shared prelude for a fragment-only shader)
    --label LABEL       Label embedded in the generated CompiledShader
                        (defaults to the source path as given)
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("wgsl-package") {
        wgsl_package(args)
    } else {
        eprint!("{USAGE}");
        ExitCode::FAILURE
    }
}

fn wgsl_package(args: impl Iterator<Item = String>) -> ExitCode {
    let mut name = None;
    let mut out = None;
    let mut label = None;
    let mut prepends = Vec::new();
    let mut source_path = None;

    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => name = args.next(),
            "--out" => out = args.next(),
            "--label" => label = args.next(),
            "--prepend" => prepends.push(args.next().unwrap_or_else(|| missing("--prepend"))),
            "--help" | "-h" => {
                eprint!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ if arg.starts_with("--") => {
                eprintln!("unknown option {arg}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
            _ if source_path.is_none() => source_path = Some(arg),
            _ => {
                eprintln!("unexpected extra argument {arg}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    let (Some(source_path), Some(out)) = (source_path, out) else {
        eprintln!("wgsl-package requires <SOURCE> and --out\n\n{USAGE}");
        return ExitCode::FAILURE;
    };

    let source_path = PathBuf::from(source_path);
    let name = name.unwrap_or_else(|| {
        source_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_else(|| missing("a --name (the source file stem is not usable)"))
            .to_owned()
    });
    let label = label.unwrap_or_else(|| source_path.display().to_string());

    let mut source = String::new();
    for prepend in &prepends {
        source.push_str(
            &std::fs::read_to_string(prepend)
                .unwrap_or_else(|error| panic!("failed to read {prepend}: {error}")),
        );
    }
    source.push_str(
        &std::fs::read_to_string(&source_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", source_path.display())),
    );

    shaderloom::build::package_wgsl(&label, &source, &name, &out).write(
        &std::env::current_dir()
            .unwrap_or_else(|error| panic!("failed to resolve the working directory: {error}")),
    );
    ExitCode::SUCCESS
}

fn missing(what: &str) -> ! {
    panic!("{what} requires a value");
}
