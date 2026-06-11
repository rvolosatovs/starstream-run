use std::fs;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;

/// Run a Wasm component.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Path to the Wasm component to run.
    wasm: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let Args { wasm } = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let wasm = fs::read(&wasm)
        .with_context(|| format!("failed to read bytes from `{}`", wasm.display()))?;
    starstream_run::Contract::<starstream_run_cli::Ctx>::new(wasm)?;
    Ok(())
}
