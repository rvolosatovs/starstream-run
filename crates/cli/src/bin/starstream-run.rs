use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, bail, ensure};
use clap::{Parser, Subcommand};
use starstream_run::Contract;
use starstream_run_cli::{CardanoCtx, Ctx, method_hash};
use wasmtime::AsContext as _;
use wasmtime::component::{Type, wasm_wave};

/// Run a Wasm component.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,

    /// Cardano block height reported to the contract (`cardano#block-height`).
    #[arg(long, default_value_t = 0)]
    cardano_block_height: i64,

    /// Cardano current slot reported to the contract (`cardano#current-slot`).
    #[arg(long, default_value_t = 0)]
    cardano_current_slot: i64,

    /// Exported instance owning the `utxo` resource. Defaults to the only
    /// such instance the contract exports.
    #[arg(long, global = true)]
    utxo: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Mint a UTXO by calling a `[static]` constructor.
    New {
        /// Path to the Wasm component to run.
        wasm: PathBuf,

        /// Constructor name, without the `[static]utxo.` prefix.
        #[arg(long, default_value = "new")]
        constructor: String,

        /// WAVE-encoded constructor parameters, one per argument.
        params: Vec<String>,
    },
    /// Reconstruct a UTXO from a stored `storage` record via `set-storage`.
    // Help is deferred to the command generated from the contract's `storage`
    // record, so that it lists the actual `--<field>` flags.
    #[command(disable_help_flag = true)]
    Load {
        /// Path to the Wasm component to run.
        wasm: PathBuf,

        /// Storage field values as `--<field> <WAVE value>` flags, one per
        /// field of the contract's `storage` record.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        fields: Vec<String>,
    },
}

/// Parse WAVE-encoded `args` against the expected `types`, one value per
/// argument.
fn parse_vals<'a>(
    what: &str,
    types: impl ExactSizeIterator<Item = (&'a str, Type)>,
    args: &[String],
) -> anyhow::Result<Vec<wasmtime::component::Val>> {
    ensure!(
        types.len() == args.len(),
        "expected {} {what}(s), got {}",
        types.len(),
        args.len(),
    );
    types
        .zip(args)
        .map(|((name, ty), arg)| {
            wasm_wave::from_str(&ty, arg)
                .with_context(|| format!("failed to parse {what} `{name}` from `{arg}`"))
        })
        .collect()
}

fn main() -> anyhow::Result<()> {
    let Args {
        cardano_block_height,
        cardano_current_slot,
        utxo,
        command,
    } = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let wasm = match &command {
        Command::New { wasm, .. } | Command::Load { wasm, .. } => wasm,
    };
    let wasm = fs::read(wasm)
        .with_context(|| format!("failed to read bytes from `{}`", wasm.display()))?;
    let contract = Contract::<Ctx>::new(wasm)?;

    // Resolve the exported instance owning the `utxo` resource: by name if
    // given, otherwise the contract's only one.
    let export = if let Some(name) = utxo {
        contract.get_utxo(&name)?
    } else {
        let mut utxos = contract.utxos();
        let Some((name, utxo)) = utxos.next() else {
            bail!("contract exports no instance owning a `utxo` resource")
        };
        let rest: Vec<_> = utxos.map(|(name, ..)| name).collect();
        ensure!(
            rest.is_empty(),
            "contract exports multiple instances owning a `utxo` resource (`{name}`, `{}`); select one with `--utxo`",
            rest.join("`, `"),
        );
        utxo?
    };

    let cardano = CardanoCtx {
        block_height: cardano_block_height,
        current_slot: cardano_current_slot,
    };
    let ctx = Ctx {
        cardano,
        ..Default::default()
    };
    let utxo = match command {
        Command::New {
            constructor,
            params,
            ..
        } => {
            let constructor =
                contract.get_utxo_constructor(&export, &format!("[static]utxo.{constructor}"))?;
            let params = parse_vals("parameter", constructor.ty().params(), &params)?;
            contract.create_utxo(ctx, &constructor, params)?
        }
        Command::Load { fields, .. } => {
            let Some(storage) = export.storage() else {
                bail!("the `utxo` resource does not advertise a `storage` record")
            };
            // The flags depend on the contract, so parse the raw trailing args
            // against a command generated from the `storage` record fields.
            let mut cmd = clap::Command::new("load")
                .about("Reconstruct a UTXO from a stored `storage` record via `set-storage`")
                .no_binary_name(true);
            for f in storage.ty().fields() {
                cmd = cmd.arg(
                    clap::Arg::new(f.name.to_string())
                        .long(f.name.to_string())
                        .required(true)
                        .value_name("WAVE"),
                );
            }
            let matches = cmd
                .try_get_matches_from(&fields)
                .unwrap_or_else(|err| err.exit());
            let fields = storage
                .ty()
                .fields()
                .map(|f| {
                    let arg: &String = matches.get_one(f.name).expect("flag is required");
                    let val = wasm_wave::from_str(&f.ty, arg).with_context(|| {
                        format!("failed to parse storage field `{}` from `{arg}`", f.name)
                    })?;
                    Ok((f.name.to_string(), val))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            contract.load_utxo(ctx, storage, fields)?
        }
    };

    // TODO: serve methods
    // Only the methods this instantiation declared via `implements-method`
    // (recorded in its store's `Ctx` during the guest constructor) are
    // advertised.
    let implemented = &utxo.as_context().data().implemented;
    for (name, method) in contract.utxo_methods(&export) {
        let method = method?;
        if !implemented.contains(&method_hash(name)) {
            continue;
        }
        let ty = method.ty();
        let params: Vec<_> = ty.params().collect();
        let results: Vec<_> = ty.results().collect();
        println!("{name}: {params:?} -> {results:?}");
    }
    utxo.drop()?;
    Ok(())
}
