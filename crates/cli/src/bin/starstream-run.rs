use std::fs;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use starstream_run_cli::{CardanoCtx, Ctx};

/// Run a Wasm component.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Path to the Wasm component to run.
    wasm: PathBuf,

    /// Cardano block height reported to the contract (`cardano#block-height`).
    #[arg(long, default_value_t = 0)]
    cardano_block_height: i64,

    /// Cardano current slot reported to the contract (`cardano#current-slot`).
    #[arg(long, default_value_t = 0)]
    cardano_current_slot: i64,
}

fn main() -> anyhow::Result<()> {
    let Args {
        wasm,
        cardano_block_height,
        cardano_current_slot,
    } = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // The Cardano context a minted UTXO would observe. The CLI only loads, links
    // and pre-instantiates (it never calls a constructor or runs guest code), so
    // this `Ctx` isn't consumed by a guest yet — log it so the configured values
    // are visible.
    let ctx = Ctx {
        cardano: CardanoCtx {
            block_height: cardano_block_height,
            current_slot: cardano_current_slot,
        },
    };
    tracing::debug!(?ctx, "cardano context");

    let wasm = fs::read(&wasm)
        .with_context(|| format!("failed to read bytes from `{}`", wasm.display()))?;
    starstream_run::Contract::<Ctx>::new(wasm)?;
    Ok(())
}
