//! Runs the `score` example guest (`examples/components/score`) through the
//! `starstream-run` CLI binary.
//!
//! The guest is built and embedded by the `test-components` crate (its
//! `build.rs` targets `wasm32-unknown-unknown`); here we exercise the binary
//! end to end, so we materialize the embedded bytes to a file and hand the CLI
//! its path.

use std::process::Command;

use tempfile::NamedTempFile;
use test_components::EXAMPLE_SCORE;

#[test]
fn builds_and_runs_score_component() {
    // The CLI takes a path; write the embedded guest to a temp file for it.
    let mut wasm = NamedTempFile::new().expect("failed to create a temp file");
    std::io::Write::write_all(&mut wasm, EXAMPLE_SCORE)
        .expect("failed to write the score guest to a temp file");

    // Run the produced module through the CLI: the host wraps the core module
    // into a component, instantiates it and mints a UTXO via `[static]utxo.new`.
    let status = Command::new(env!("CARGO_BIN_EXE_starstream-run"))
        .arg("new")
        .arg(wasm.path())
        .status()
        .expect("failed to spawn the starstream-run CLI");
    assert!(
        status.success(),
        "the CLI failed to run the score component"
    );
}
