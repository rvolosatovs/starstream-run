use core::iter::zip;

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, bail, ensure};
use clap::{Parser, Subcommand};
use starstream_run::Contract;
use starstream_run_cli::{CardanoCtx, Ctx, method_hash};
use wasmtime::AsContext as _;
use wasmtime::component::{Type, types, wasm_wave};

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

/// Render a component-model [`Type`] as WIT. `wasm_wave`'s `DisplayType` covers
/// the structural types but renders resource handles as `<<UNSUPPORTED>>`; the
/// `utxo` resource is the only one in play here, so spell its handles by name.
fn wit_type(ty: &Type) -> String {
    match ty {
        Type::Own(_) => "utxo".into(),
        Type::Borrow(_) => "borrow<utxo>".into(),
        ty => wasm_wave::wasm::DisplayType(ty).to_string(),
    }
}

/// Render a `[method]utxo.<name>` export as a WIT function declaration: the
/// trailing `<name>` segment and its params (the leading implicit
/// `self: borrow<utxo>` receiver dropped) and results.
fn wit_func(export: &str, ty: &wasmtime::component::types::ComponentFunc) -> String {
    let name = export.rsplit('.').next().unwrap_or(export);
    let params = ty
        .params()
        .skip(1) // the implicit `self` receiver
        .map(|(n, t)| format!("{n}: {}", wit_type(&t)))
        .collect::<Vec<_>>()
        .join(", ");
    let results: Vec<_> = ty.results().collect();
    let ret = match results.as_slice() {
        [] => String::new(),
        [t] => format!(" -> {}", wit_type(t)),
        types => format!(
            " -> ({})",
            types.iter().map(wit_type).collect::<Vec<_>>().join(", "),
        ),
    };
    format!("{name}: func({params}){ret};")
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

    let (export_name, export) = if let Some(name) = utxo {
        let export = contract.get_utxo(&name)?;
        (name, export)
    } else {
        let mut utxos = contract.utxos();
        let Some((name, utxo)) = utxos.next() else {
            bail!("contract exports no instance owning a `utxo` resource")
        };
        let utxos: Vec<_> = utxos.map(|(name, ..)| name).collect();
        ensure!(
            utxos.is_empty(),
            "contract exports multiple instances owning a `utxo` resource (`{name}`, `{}`); select one with `--utxo`",
            utxos.join("`, `"),
        );
        (name.to_string(), utxo?)
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

            let params_ty = constructor.ty().params();
            ensure!(
                params_ty.len() == params.len(),
                "expected {} parameter(s), got {}",
                params_ty.len(),
                params.len(),
            );
            let params = zip(params_ty, params)
                .map(|((name, ty), s)| {
                    wasm_wave::from_str(&ty, &s)
                        .with_context(|| format!("failed to parse parameter `{name}` from `{s}`"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
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
            for types::Field { name, .. } in storage.ty().fields() {
                cmd = cmd.arg(
                    clap::Arg::new(name.to_string())
                        .long(name.to_string())
                        .required(true)
                        .value_name("WAVE"),
                );
            }
            let matches = cmd
                .try_get_matches_from(&fields)
                .unwrap_or_else(|err| err.exit());

            let fields_ty = storage.ty().fields();
            let fields = fields_ty
                .map(|types::Field { name, ty }| {
                    let s = matches
                        .get_one::<String>(name)
                        .context("failed to get flag")?;
                    let v = wasm_wave::from_str(&ty, s).with_context(|| {
                        format!("failed to parse parameter `{name}` from `{s}`")
                    })?;
                    Ok((name.into(), v))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            contract.load_utxo(ctx, storage, fields)?
        }
    };

    let implemented = &utxo.as_context().data().implemented;

    let (_, instance) = export_name.split_once('/').unwrap_or(("", &export_name));
    let (instance, ..) = instance.split_once('@').unwrap_or((instance, ""));

    println!("package starstream:utxo;\n");
    println!("interface {instance} {{");
    for (name, method) in contract.utxo_methods(&export) {
        let method = method?;
        if implemented.contains(&method_hash(name)) {
            println!("    {}", wit_func(name, method.ty()));
        }
    }
    println!("}}");

    // TODO: serve methods

    utxo.drop()?;
    Ok(())
}
