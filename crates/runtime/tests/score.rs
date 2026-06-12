//! Drives the `score` example guest (`examples/components/score`) through the
//! typed [`Contract`] runtime API.
//!
//! The guest is built and embedded by the `test-components` crate (its
//! `build.rs` targets `wasm32-unknown-unknown`); this is the reference example
//! of using the runtime as a library, mirroring the by-name lookups the web UI
//! takes.

use std::collections::BTreeMap;

use starstream_run::{Contract, EventHandler, MethodExport, Utxo, bindings};
use test_components::EXAMPLE_SCORE;
use wasmtime::component::Val;

/// Store data for the test contract. The runtime supplies no [`Host`] impl, so
/// a consumer must provide one; this is a minimal stub mirroring the CLI/web
/// `Ctx`es (it records nothing and reports a zeroed Cardano context).
///
/// [`Host`]: starstream_run::Host
#[derive(Clone, Debug, Default)]
struct Ctx;

impl bindings::starstream::std::builtin::Host for Ctx {
    fn implements_method(&mut self, _hash: (u64, u64, u64, u64)) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl bindings::starstream::std::cardano::Host for Ctx {
    fn block_height(&mut self) -> i64 {
        0
    }

    fn current_slot(&mut self) -> i64 {
        0
    }
}

impl EventHandler for Ctx {
    fn emit_event(&mut self, _instance: &str, _name: &str, _params: &[Val]) {}
}

/// Invoke the `[method]utxo.<name>` ABI method on a live handle, injecting the
/// resource as the borrowed `self` first parameter.
fn call_method(
    handle: &mut Utxo<Ctx>,
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
    let contract =
        Contract::<Ctx>::new(EXAMPLE_SCORE).expect("failed to instantiate score contract");

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

    // Mint a handle with `new`, passing the store data for its fresh store.
    let mut handle = contract
        .create_utxo(Ctx, &new, [])
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

    // `finish` emits the `Finish(chips * mult)` ABI event (host-side: a no-op).
    call_method(&mut handle, &methods, "finish", &[]);
}
