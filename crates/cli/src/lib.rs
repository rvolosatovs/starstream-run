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

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use starstream_run::bindings;
use tracing::{debug, info};

/// The 256-bit method identity a contract declares via `implements-method`,
/// carried as the four `u64` words wasmtime hands us from the guest.
pub type MethodHash = (u64, u64, u64, u64);

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
/// this carries the [`CardanoCtx`] the caller configures plus the set of
/// method hashes this instantiation declared via `implements-method`.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub cardano: CardanoCtx,
    /// The method hashes the guest declared via `implements-method` (populated
    /// during the constructor). A method is only advertised once its hash
    /// appears here — see [`method_hash`].
    pub implemented: HashSet<MethodHash>,
}

impl bindings::starstream::std::builtin::Host for Ctx {
    /// The guest declares a method it implements by its 256-bit hash; record it
    /// so callers can gate method listings and invocations on it.
    fn implements_method(&mut self, hash: MethodHash) -> wasmtime::Result<()> {
        debug!("implements-method {hash:?}");
        self.implemented.insert(hash);
        Ok(())
    }
}

/// The hash a contract declares for `export` via `implements-method`.
///
/// The Starstream compiler identifies each method by `sha256` of its source
/// name (`snake_case`), split into four little-endian `u64` words. An exported
/// method is named `[method]utxo.plus-chips` in WIT (`kebab-case`); take the
/// trailing segment and undo the `kebab-case` mangling (`-` → `_`) to recover
/// the name the compiler hashed.
pub fn method_hash(export: &str) -> MethodHash {
    let name = export
        .rsplit('.')
        .next()
        .unwrap_or(export)
        .replace('-', "_");
    let digest = Sha256::digest(name.as_bytes());
    let word = |i: usize| {
        u64::from_le_bytes(
            digest[i * 8..i * 8 + 8]
                .try_into()
                .expect("a sha256 digest is 32 bytes"),
        )
    };
    (word(0), word(1), word(2), word(3))
}

impl bindings::starstream::std::cardano::Host for Ctx {
    fn block_height(&mut self) -> i64 {
        self.cardano.block_height
    }

    fn current_slot(&mut self) -> i64 {
        self.cardano.current_slot
    }
}

impl starstream_run::EventHandler for Ctx {
    fn emit_event(&mut self, instance: &str, name: &str, params: &[wasmtime::component::Val]) {
        info!(instance, name, ?params, "ABI event emitted");
    }
}
