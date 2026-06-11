//! Host context for the `starstream-run` CLI.
//!
//! [`starstream_run::Contract`] is generic over its store-data type, which must
//! implement [`starstream_run::Host`] (the `starstream:std` builtin/cardano host
//! traits). The CLI runs contracts headless and does not model a host ledger, so
//! [`Ctx`] carries only the Cardano context the guest can observe — the block
//! height and current slot, both supplied by the caller (the
//! `--cardano-block-height` / `--cardano-current-slot` CLI flags, defaulting to
//! 0).
//!
//! It lives in a library target (rather than the binary) so both the
//! `starstream-run` binary and the `score` integration test share one
//! implementation.

use starstream_run::bindings;
use tracing::error;

/// The Cardano context a contract can observe via the `starstream:std/cardano`
/// host functions.
#[derive(Clone, Copy, Debug, Default)]
pub struct CardanoCtx {
    /// Block height reported to the guest via `cardano#block-height`.
    pub block_height: i64,
    /// Current slot reported to the guest via `cardano#current-slot`.
    pub current_slot: i64,
}

/// Store data for CLI-run contracts. The CLI does not model a host ledger, so
/// this only carries the [`CardanoCtx`] the caller configures.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx {
    pub cardano: CardanoCtx,
}

impl bindings::starstream::std::builtin::Host for Ctx {
    fn implements_method(&mut self, hash: (u64, u64, u64, u64)) -> wasmtime::Result<()> {
        error!("called builtin#implements_method {hash:?}");
        Ok(())
    }
}

impl bindings::starstream::std::cardano::Host for Ctx {
    fn block_height(&mut self) -> i64 {
        self.cardano.block_height
    }

    fn current_slot(&mut self) -> i64 {
        self.cardano.current_slot
    }
}
