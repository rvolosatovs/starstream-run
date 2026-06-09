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

The same library (`crates/runtime`) runs both natively, via the CLI
(`crates/cli`), and **in the browser** (`crates/web`), where wasmtime executes
guests with the Pulley interpreter.

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
`cranelift-codegen`, kept on the same git rev) is pinned to a git rev of
bytecodealliance/wasmtime, not a crates.io release; the WASI adapter provider is
`45.0.1`.

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

    get-storage: func(utxo: borrow<utxo>) -> storage;
    set-storage: func(utxo: borrow<utxo>, storage: storage);

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
`get-storage` / `set-storage` functions (taking `borrow<utxo>`) that read and
write the resource's mutable `storage` record.

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

Building and running it through the CLI is also covered by an integration test
([`crates/cli/tests/score.rs`](crates/cli/tests/score.rs)):

```bash
cargo test --test score
```

## Run in the browser

[`crates/web`](crates/web) compiles the runtime to `wasm32-unknown-unknown` and
drives it from a small upload page. wasmtime runs the guest with its Pulley
interpreter, and a custom-virtual-memory shim
([`src/wasmtime.rs`](crates/web/src/wasmtime.rs)) stands in for the mmap/TLS
facilities an OS would normally provide.

```bash
cd crates/web
npm start         # build (cargo + wasm-bindgen + score.wasm) and serve
# then open http://localhost:8080
```

A plain `file://` open won't work — the page is an ES module and fetches the
multi-MB `.wasm` runtime, both of which need real HTTP responses with correct
MIME types, so a static server ([`serve.mjs`](crates/web/serve.mjs)) is
included.
