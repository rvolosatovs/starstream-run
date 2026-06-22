use core::fmt::Write as _;
use core::future::poll_fn;
use core::iter::zip;
use core::pin::pin;
use core::task::Poll;

use std::fs;
use std::io::stderr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, ensure};
use bytes::BytesMut;
use clap::{Parser, Subcommand};
use futures::StreamExt as _;
use futures::future::try_join_all;
use starstream_run::Contract;
use starstream_run_cli::codec::{ValEncoder, read_value};
use starstream_run_cli::{CardanoCtx, Ctx, method_hash};
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio_util::codec::Encoder as _;
use tracing::{debug, error};
use wasmtime::AsContext as _;
use wasmtime::component::{Type, Val, types, wasm_wave};
use wasmtime::error::Context as _;
use wrpc_transport::{Serve as _, Server};

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

    /// After minting/loading the UTXO, serve its ABI methods over wRPC framed
    /// on WebSockets at this address (e.g. `127.0.0.1:8080`), until Ctrl-C.
    #[arg(long, global = true, value_name = "ADDR")]
    serve: Option<std::net::SocketAddr>,
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

/// What an accepted connection turned out to be, sniffed from its peeked request
/// head. A partial head still parses the request line and headers seen so far,
/// which is enough since both a handshake and a `GET` line fit one segment.
enum Peeked<'a> {
    /// A WebSocket upgrade, i.e. an `Upgrade: websocket` header — served as wRPC.
    WebSocket,
    /// A plain HTTP request, with its method and path (query stripped).
    Http { method: &'a str, path: &'a str },
}

/// Sniff an accepted connection's peeked request head.
fn peek(head: &[u8]) -> Peeked<'_> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let _ = req.parse(head);
    let upgrade = req.headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("upgrade")
            && h.value
                .split(|&b| b == b',' || b == b' ')
                .any(|v| v.eq_ignore_ascii_case(b"websocket"))
    });
    if upgrade {
        Peeked::WebSocket
    } else {
        let path = req.path.unwrap_or_default();
        Peeked::Http {
            method: req.method.unwrap_or_default(),
            path: path.split(['?', '#']).next().unwrap_or(path),
        }
    }
}

/// Write a minimal `text/plain` HTTP/1.1 response and close the connection.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         content-type: text/plain; charset=utf-8\r\n\
         content-length: {}\r\n\
         connection: close\r\n\
         \r\n{body}",
        body.len(),
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Args {
        cardano_block_height,
        cardano_current_slot,
        utxo,
        serve,
        command,
    } = Args::parse();

    tracing_subscriber::fmt()
        .with_writer(stderr)
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
    let mut utxo = match command {
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
                .collect::<wasmtime::Result<Vec<_>>>()?;
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

    let implemented = utxo.as_context().data().implemented.clone();

    let (_, iface) = export_name.split_once('/').unwrap_or(("", &export_name));
    let (iface, ..) = iface.split_once('@').unwrap_or((iface, ""));

    let mut wit = String::new();
    writeln!(wit, "package starstream:utxo;\n").unwrap();
    writeln!(wit, "interface {iface} {{").unwrap();
    for (name, method) in contract.utxo_methods(&export) {
        let method = method?;
        if implemented.contains(&method_hash(name)) {
            writeln!(wit, "    {}", wit_func(name, method.ty())).unwrap();
        }
    }
    writeln!(wit, "}}").unwrap();
    print!("{wit}");

    if let Some(addr) = serve {
        let wit: Arc<str> = Arc::from(wit);
        let lis = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind `{addr}`"))?;

        let srv = Arc::new(Server::default());
        let ws = tokio_websockets::ServerBuilder::default();
        let accept = tokio::spawn({
            let srv = Arc::clone(&srv);
            async move {
                loop {
                    match lis.accept().await {
                        Ok((mut stream, addr)) => {
                            debug!(?addr, "TCP connection accepted");
                            // Peek at the request head (without consuming it) to
                            // tell a WebSocket upgrade (wRPC) apart from a plain
                            // HTTP request, for which we serve the WIT at `GET /`.
                            let mut head = [0u8; 8192];
                            let n = match stream.peek(&mut head).await {
                                Ok(n) => n,
                                Err(err) => {
                                    error!(?err, "failed to peek connection");
                                    continue;
                                }
                            };
                            if let Peeked::Http { method, path } = peek(&head[..n]) {
                                let res = match (method, path) {
                                    ("GET", "/") => respond(&mut stream, "200 OK", &wit).await,
                                    ("GET", _) => {
                                        respond(&mut stream, "404 Not Found", "not found\n").await
                                    }
                                    _ => {
                                        respond(
                                            &mut stream,
                                            "405 Method Not Allowed",
                                            "method not allowed\n",
                                        )
                                        .await
                                    }
                                };
                                if let Err(err) = res {
                                    error!(?err, "failed to serve HTTP response");
                                }
                                continue;
                            }
                            let (req, (tx, rx)) = match ws.accept(stream).await {
                                Ok((req, ws)) => (req, wrpc_websockets::split(ws)),
                                Err(err) => {
                                    error!(?err, "failed to perform WebSocket handshake");
                                    continue;
                                }
                            };
                            if let Err(err) = srv.accept(req, tx, rx).await {
                                error!(?err, "failed to accept wRPC invocation");
                            }
                        }
                        Err(err) => error!(?err, "failed to accept TCP connection"),
                    }
                }
            }
        });

        let methods = contract
            .utxo_methods(&export)
            .filter(|(name, ..)| implemented.contains(&method_hash(name)));
        let instance: Arc<str> = Arc::from(format!("starstream:utxo/{iface}"));
        let invocations = methods.map(|(name, export)| {
            let srv = Arc::clone(&srv);
            let instance = Arc::clone(&instance);
            async move {
                let export = export?;
                let Some((_, name)) = name.split_once("[method]utxo.") else {
                    bail!("unexpected UTXO method name: {name}");
                };
                let invocations = srv
                    .serve(&instance, name, Arc::default())
                    .await
                    .map_err(wasmtime::Error::from_anyhow)
                    .with_context(|| format!("failed to serve `{instance}#{name}`"))?;
                Ok(((name, export), invocations))
            }
        });
        let invocations = try_join_all(invocations).await?;
        let (exports, mut invocations) = invocations.into_iter().unzip::<_, _, Vec<_>, Vec<_>>();
        let shutdown = signal::ctrl_c();
        let mut shutdown = pin!(shutdown);
        while let Some((name, export, req, mut tx, mut rx)) =
            poll_fn(|cx| match shutdown.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => Poll::Ready(None),
                Poll::Ready(Err(err)) => {
                    error!(?err, "failed to handle ^C");
                    Poll::Ready(None)
                }
                Poll::Pending => {
                    for ((name, export), invocations) in zip(&exports, &mut invocations) {
                        match invocations.poll_next_unpin(cx) {
                            Poll::Ready(Some(Ok((req, tx, rx)))) => {
                                return Poll::Ready(Some((name, export, req, tx, rx)));
                            }
                            Poll::Ready(Some(Err(err))) => {
                                error!(?err, name, "failed to accept method invocation");
                                return Poll::Ready(None);
                            }
                            Poll::Ready(None) => {
                                error!(
                                    name,
                                    "unexpected end of method invocation stream encountered"
                                );
                                return Poll::Ready(None);
                            }
                            Poll::Pending => continue,
                        }
                    }
                    Poll::Pending
                }
            })
            .await
        {
            debug!(?name, ?req, "invocation accepted");

            let params_ty = export.ty().params();

            debug_assert!(params_ty.len() >= 1);

            let mut params = vec![Val::Bool(false); params_ty.len()];
            params[0] = Val::Resource(utxo.resource());
            for (v, (name, ty)) in zip(&mut params[1..], params_ty.skip(1)) {
                debug!(name, "decoding parameter");
                read_value(&mut rx, v, &ty)
                    .await
                    .with_context(|| format!("failed to decode parameter `{name}`"))?;
            }
            let results = utxo.call(export, params)?;

            let mut buf = BytesMut::new();
            for (v, ty) in zip(&results, export.ty().results()) {
                ValEncoder::new(&ty)
                    .encode(v, &mut buf)
                    .context("failed to encode result")?;
            }
            tx.write_all(&buf)
                .await
                .context("failed to transmit results")?;
            tx.shutdown().await.context("failed to shut down stream")?;
        }
        accept.abort();
    }
    utxo.drop()?;
    Ok(())
}
