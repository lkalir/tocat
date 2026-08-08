//! Generates the C view of the ABI from the Rust one.
//!
//!     cargo run -p tocat-abi --features generate --bin tocat-abi-header
//!     cargo run -p tocat-abi --features generate --bin tocat-abi-header --
//! --check
//!
//! `--check` writes nothing and exits non-zero when the checked-in header is
//! stale, which is what CI should run: the header is committed so that a C
//! guest can be built without a Rust toolchain, and a committed generated file
//! is only trustworthy if something notices when it drifts.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

fn main() -> ExitCode {
    let mut check = false;
    let mut output: Option<PathBuf> = None;

    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("usage: tocat-abi-header [--check] [PATH]");
                return ExitCode::SUCCESS;
            }
            path => output = Some(PathBuf::from(path)),
        }
    }

    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = output.unwrap_or_else(|| default_output(crate_dir));

    let config = match cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("cbindgen.toml: {error}");
            return ExitCode::FAILURE;
        }
    };

    let bindings = match cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
    {
        Ok(bindings) => bindings,
        Err(error) => {
            eprintln!("generating: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut generated = Vec::new();
    bindings.write(&mut generated);

    if let Err(missing) = check_complete(&generated) {
        eprintln!(
            "generated header declares no {missing}. cbindgen emits only the \
             types an exported function reaches, and this crate exports no \
             functions, so every type it should carry has to be named under \
             [export] include in cbindgen.toml."
        );

        return ExitCode::FAILURE;
    }

    if !check {
        // write_to_file leaves the file alone when the contents match, which
        // keeps make and cmake from rebuilding the world on every run.
        bindings.write_to_file(&path);
        println!("{}", path.display());

        return ExitCode::SUCCESS;
    }

    match std::fs::read(&path) {
        Ok(current) if current == generated => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!(
                "{} is stale. Regenerate it with:\n    cargo run -p tocat-abi \
                 --features generate --bin tocat-abi-header",
                path.display()
            );

            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// A header missing a type still compiles here and fails in `tocat.h`, a long
/// way from the cause, so the generator checks what it produced.
fn check_complete(generated: &[u8]) -> Result<(), &'static str> {
    let text = String::from_utf8_lossy(generated);

    for required in [
        "TOCAT_ABI_VERSION",
        "TOCAT_OUTBOX_LEN",
        "tocat_outbox_t",
        "tocat_log_record_t",
    ] {
        if !text.contains(required) {
            return Err(required);
        }
    }

    Ok(())
}

/// The C SDK's copy, which is the one that is committed and shipped.
fn default_output(crate_dir: &Path) -> PathBuf {
    crate_dir.join("../..").join("sdk/wasm/include/tocat/abi.h")
}
