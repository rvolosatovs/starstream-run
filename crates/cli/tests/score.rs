//! Builds the `score` example guest (`tests/score`) and runs it through the
//! `starstream-run` CLI.
//!
//! `tests/score` is its own workspace targeting `wasm32-unknown-unknown` and
//! carries no `.cargo/config.toml`, so the target is passed explicitly here —
//! a plain `cargo build` would build for the host and produce no `.wasm`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use starstream_run::{Contract, MethodExport, Utxo};
use wasmtime::component::Val;

const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// Build the `score` guest to a core module carrying a `component-type` custom
/// section, returning the path to it. The target is mandatory — see the module
/// comment.
fn build_score() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let score_manifest = manifest_dir.join("tests/score/Cargo.toml");

    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--target",
            WASM_TARGET,
            "--manifest-path",
        ])
        .arg(&score_manifest)
        .status()
        .expect("failed to spawn cargo to build the score guest");
    assert!(status.success(), "building the score guest failed");

    let wasm = manifest_dir
        .join("tests/score/target")
        .join(WASM_TARGET)
        .join("release/score.wasm");
    assert!(wasm.exists(), "expected built guest at {}", wasm.display());
    wasm
}

#[test]
fn builds_and_runs_score_component() {
    let wasm = build_score();

    // Run the produced module through the CLI: the host wraps the core module
    // into a component and instantiates it.
    let status = Command::new(env!("CARGO_BIN_EXE_starstream-run"))
        .arg(&wasm)
        .status()
        .expect("failed to spawn the starstream-run CLI");
    assert!(
        status.success(),
        "the CLI failed to run the score component"
    );
}

/// Invoke the `[method]utxo.<name>` ABI method on a live handle, injecting the
/// resource as the borrowed `self` first parameter.
fn call_method(
    handle: &mut Utxo,
    methods: &BTreeMap<String, MethodExport>,
    name: &str,
    args: &[Val],
) {
    let export = methods
        .get(&format!("[method]utxo.{name}"))
        .unwrap_or_else(|| panic!("missing `{name}` method"));
    let mut params = vec![Val::Resource(handle.resource())];
    params.extend_from_slice(args);
    handle
        .call(export, &params)
        .unwrap_or_else(|err| panic!("calling `{name}` failed: {err:?}"));
}

/// Drive the `score` contract's `utxo` resource through the [`Contract`] API:
/// discover it, mint a handle via the `new` constructor, run the `Score` ABI
/// methods (`plus-chips`/`plus-mult`/`mult-mult`) and read the accumulated
/// score back through the typed storage accessor.
#[test]
fn drives_score_utxo_resource() {
    let wasm = std::fs::read(build_score()).expect("failed to read built score guest");
    let contract = Contract::new(wasm).expect("failed to instantiate score contract");

    // The exported `score-progress` instance owns a `utxo` resource with a
    // `new` constructor and the `Score` ABI methods from `example.star`.
    let (instance_name, utxo) = contract
        .utxos()
        .next()
        .expect("expected an instance exporting a `utxo` resource");
    let utxo = utxo.expect("failed to describe the `utxo` resource");

    // The by-name lookups (the path the web UI takes) resolve the same
    // exports: the instance at the root, constructors/methods within it.
    let utxo_by_name = contract
        .get_utxo(instance_name)
        .expect("failed to look up the `utxo` instance by name");
    contract
        .get_utxo_constructor(&utxo_by_name, "[static]utxo.new")
        .expect("failed to look up the `new` constructor by name");
    contract
        .get_utxo_method(&utxo_by_name, "[method]utxo.plus-chips")
        .expect("failed to look up the `plus-chips` method by name");

    // It exposes the `new` constructor ...
    let new = contract
        .utxo_constructors(&utxo)
        .find_map(|(name, ctor)| (name == "[static]utxo.new").then_some(ctor))
        .expect("expected a `[static]utxo.new` constructor")
        .expect("failed to describe the `new` constructor");

    // ... and the `Score` ABI methods.
    let methods: BTreeMap<String, MethodExport> = contract
        .utxo_methods(&utxo)
        .map(|(name, method)| (name.to_string(), method.expect("failed to describe method")))
        .collect();
    for method in ["plus-chips", "plus-mult", "mult-mult", "finish"] {
        assert!(
            methods.contains_key(&format!("[method]utxo.{method}")),
            "expected a `{method}` method, got {:?}",
            methods.keys().collect::<Vec<_>>(),
        );
    }

    // The `utxo` resource owns a `storage` record read back via `get-storage`.
    let storage = utxo
        .storage()
        .expect("expected the `utxo` resource to own `storage`")
        .clone();

    // Mint a handle with `new`.
    let mut handle = contract
        .instantiate_utxo(&new, [])
        .expect("calling `new` failed");

    // Run the `Score` ABI: chips = 0 + 10; mult = 0 + 4; mult = 4 * 150 / 100 = 6.
    call_method(&mut handle, &methods, "plus-chips", &[Val::U64(10)]);
    call_method(&mut handle, &methods, "plus-mult", &[Val::U64(4)]);
    call_method(&mut handle, &methods, "mult-mult", &[Val::U64(150)]);

    // Read the accumulated score back through the typed storage accessor.
    let got = handle
        .storage(&storage)
        .get()
        .expect("reading storage failed");
    assert_eq!(
        got,
        vec![
            ("chips".to_string(), Val::U64(10)),
            ("mult".to_string(), Val::U64(6)),
        ],
        "storage did not match the expected score"
    );

    // `finish` emits the `Finish(chips * mult)` ABI event (host-side: logged).
    call_method(&mut handle, &methods, "finish", &[]);
}
