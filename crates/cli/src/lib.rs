//! Host context for the `starstream-run` CLI.
//!
//! [`starstream_run::Contract`] is generic over its store-data type, which must
//! implement [`starstream_run::Host`] (the `starstream:std` builtin/cardano host
//! traits, plus [`Default`]). The CLI runs contracts headless and does not model
//! a host ledger, so [`Ctx`] implements those host functions as stubs that log
//! and return defaults.
//!
//! It lives in a library target (rather than the binary) so both the
//! `starstream-run` binary and the `score` integration test share one
//! implementation.

use starstream_run::bindings;
use tracing::error;

#[derive(Clone, Copy, Default)]
pub struct Ctx;

impl bindings::starstream::std::builtin::Host for Ctx {
    fn implements_method(&mut self, hash: (u64, u64, u64, u64)) -> wasmtime::Result<()> {
        error!("called builtin#implements_method {hash:?}");
        Ok(())
    }
}

impl bindings::starstream::std::cardano::Host for Ctx {
    fn block_height(&mut self) -> i64 {
        error!("called cardano#block_height");
        0
    }

    fn current_slot(&mut self) -> i64 {
        error!("called cardano#current_slot");
        0
    }
}
