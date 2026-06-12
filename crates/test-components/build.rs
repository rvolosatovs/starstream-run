//! Compiles the example guest contracts under `examples/components` and copies
//! each produced `.wasm` into `$OUT_DIR`, where `src/lib.rs` `include_bytes!`s
//! them.
//!
//! Each guest is its own workspace targeting `wasm32-unknown-unknown` and
//! carries no `.cargo/config.toml`, so the target is passed explicitly — a
//! plain `cargo build` would build for the host and produce no `.wasm`. The
//! target must also be installed (`rustup target add wasm32-unknown-unknown`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// Build `examples/components/<name>` to a core module carrying a
/// `component-type` custom section and copy the result to `$OUT_DIR/<name>.wasm`.
fn build_guest(examples: &Path, out_dir: &Path, name: &str) {
    let dir = examples.join(name);
    let manifest = dir.join("Cargo.toml");

    let status = Command::new(env::var_os("CARGO").expect("CARGO set by cargo"))
        .args([
            "build",
            "--release",
            "--target",
            WASM_TARGET,
            "--manifest-path",
        ])
        .arg(&manifest)
        .status()
        .unwrap_or_else(|err| panic!("failed to spawn cargo to build the `{name}` guest: {err}"));
    assert!(status.success(), "building the `{name}` guest failed");

    let built = dir
        .join("target")
        .join(WASM_TARGET)
        .join("release")
        .join(format!("{name}.wasm"));
    assert!(
        built.exists(),
        "expected built guest at {}",
        built.display()
    );
    fs::copy(&built, out_dir.join(format!("{name}.wasm")))
        .unwrap_or_else(|err| panic!("failed to copy the `{name}` guest: {err}"));

    // Rebuild when the guest's sources change.
    for sub in ["src", "wit", "build.rs", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={}", dir.join(sub).display());
    }
}

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let examples = manifest_dir
        .join("../../examples/components")
        .canonicalize()
        .expect("examples/components directory exists");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));

    build_guest(&examples, &out_dir, "score");
}
