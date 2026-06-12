# starstream-run

A CLI that loads, links, instantiates and **runs** a WebAssembly
[component](https://component-model.bytecodealliance.org) using
[wasmtime](https://wasmtime.dev). The `.wasm` inputs are
[Starstream](example.star) contracts — a UTXO smart-contract language with
`abi`/`utxo`/`storage`/`event` constructs.

Inputs may be either a fully-encoded component **or** a *core* module carrying a
`component-type` custom section (the `wit-component embed` form the Starstream
compiler emits). Core modules are wrapped into components in-process via
`wit_component::ComponentEncoder` (the equivalent of `wasm-tools component
new`), so both forms run directly.

The library (`crates/runtime`) holds all the real logic and exposes a typed API
(`Contract<T>` / `Utxo<T>`, generic over a caller-supplied store-data type). It
runs both natively, via the CLI (`crates/cli`), and **in the browser**
(`crates/web`), where wasmtime executes guests with the Pulley interpreter.
Guest-invoking operations come in sync and async pairs over one engine; the
async halves are gated behind the runtime's `async` cargo feature (used by the
web crate).

## Run the host

```bash
cargo run -- <component.wasm>            # load + link + pre-instantiate a component
RUST_LOG=debug cargo run -- example.wasm # tracing via EnvFilter (default INFO)
```

The CLI itself only loads, links and pre-instantiates the component; driving a
contract's `utxo` resource (minting handles, calling its ABI methods, reading
`storage`) is done through the `crates/runtime` library API — see the
integration test below.

Requires a Rust toolchain supporting **edition 2024**. wasmtime (and
`cranelift-codegen`, kept on the same git rev) is pinned to the
**rvolosatovs/wasmtime fork, branch `feat/custom-fiber`** (until the
`custom-fiber` feature lands upstream), not a crates.io release; the WASI
adapter provider is `45.0.1` and `wasmparser` / `wit-component` are `0.251.0`.

## Example component: `crates/cli/tests/score`

[`crates/cli/tests/score`](crates/cli/tests/score) is a Rust guest that builds a
component for this WIT:

```wit
package root:component;

interface score {
    finish: func(x: u64);
}

interface score-progress {
    record storage {
      chips: u64,
      mult: u64,
    }

    get-storage: func(self: borrow<utxo>) -> storage;
    set-storage: func(storage: storage) -> utxo;

    resource utxo {
        new: static func() -> utxo;

        plus-chips: func(chips2: u64);
        plus-mult: func(mult2: u64);
        mult-mult: func(mult-pct: u64);
        finish: func();
    }
}

world root {
    import score;
    export score-progress;
}
```

It imports the host's `score` interface and exports `score-progress`, whose
`utxo` resource exposes a `new` constructor and the `Score` ABI methods
(`plus-chips` / `plus-mult` / `mult-mult` / `finish`). The instance also exports
`get-storage` (reads the resource's `storage` record from a `borrow<utxo>`) and
`set-storage` (reconstructs a fresh `utxo` from a stored `storage` record — how
a UTXO is reloaded from saved state).

The crate is its own workspace, so it stays out of the host crate's build
graph. It builds to a *core* module carrying a `component-type` custom section
— the same `wit-component embed` form as [`example.wasm`](example.wasm) — which
the host's `componentize` step wraps into a full component at run time.

The `wasm32-unknown-unknown` target must be passed explicitly — a plain `cargo
build` would build for the host and produce no `.wasm` (the target also needs
to be installed: `rustup target add wasm32-unknown-unknown`).

```bash
# build the guest
cargo build --release --target wasm32-unknown-unknown \
  --manifest-path crates/cli/tests/score/Cargo.toml

# run it through the host
cargo run -- crates/cli/tests/score/target/wasm32-unknown-unknown/release/score.wasm
```

Inspect the embedded WIT of the produced module with:

```bash
wasm-tools component wit crates/cli/tests/score/target/wasm32-unknown-unknown/release/score.wasm
```

The guest is also exercised by integration tests
([`crates/cli/tests/score.rs`](crates/cli/tests/score.rs)):
`builds_and_runs_score_component` runs the built module through the CLI binary,
and `drives_score_utxo_resource` drives the typed runtime API directly —
discovering the `utxo`, minting a handle with `new`, calling the ABI methods,
and asserting the accumulated `storage` record read back through the typed
accessor. The latter is the reference example of using the runtime as a library.

```bash
cargo test --test score
```

## Run in the browser

[`crates/web`](crates/web) compiles the runtime to `wasm32-unknown-unknown` and
drives it from a small upload page. wasmtime runs the guest with its Pulley
interpreter, and a custom-virtual-memory shim
([`src/wasmtime.rs`](crates/web/src/wasmtime.rs)) stands in for the mmap/TLS
facilities an OS would normally provide. Guest-invoking calls go through the
runtime's `*_async` APIs, whose fibers are backed by
[JSPI](https://github.com/WebAssembly/js-promise-integration) — so running the
page needs **Chromium ≥ 137** (or Node ≥ 24 with `--experimental-wasm-jspi`).

```bash
cd crates/web
npm start         # build (cargo + wasm-bindgen + score.wasm) and serve
# then open http://localhost:8080
```

A plain `file://` open won't work — the page is an ES module and fetches the
multi-MB `.wasm` runtime, both of which need real HTTP responses with correct
MIME types, so a static server ([`serve.mjs`](crates/web/serve.mjs)) is
included.
