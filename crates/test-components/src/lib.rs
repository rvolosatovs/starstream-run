//! Compiled example guest contracts (from `examples/components`) for the host
//! crates' tests.
//!
//! `build.rs` builds each guest for `wasm32-unknown-unknown` and copies the
//! produced `.wasm` into `$OUT_DIR`; the bytes are embedded here. Each is a
//! *core* module carrying a `component-type` custom section — the host's
//! `componentize` step wraps it into a component at run time, so it can be fed
//! straight to [`starstream_run::Contract::new`].

/// The `score` example guest (`examples/components/score`): a Rust contract
/// implementing the `root` world from `example.star` — a `score-progress`
/// instance owning a `utxo` resource with the `Score` ABI methods and
/// `get-storage`/`set-storage`.
pub const EXAMPLE_SCORE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/score.wasm"));
