use std::io::Write as _;
use std::net::{Ipv6Addr, SocketAddr, TcpListener};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tempfile::NamedTempFile;
use test_components::EXAMPLE_SCORE;
use tokio::process::Command;
use tokio::time::sleep;
use tokio::{io::AsyncReadExt as _, net::TcpStream};

mod bindings {
    wit_bindgen_wrpc::generate!({
        inline: "
            package starstream:utxo;

            // The flat interface the CLI serves: the `utxo` resource's ABI
            // methods, with the implicit `self` receiver injected host-side.
            interface score-progress {
                plus-chips: func(chips2: u64);
                plus-mult: func(mult2: u64);
                mult-mult: func(mult-pct: u64);
                finish: func();
            }

            world client {
                import score-progress;
            }
        ",
    });
}

use bindings::starstream::utxo::score_progress;

#[tokio::test(flavor = "multi_thread")]
async fn new() -> anyhow::Result<()> {
    let mut wasm = NamedTempFile::new().context("failed to create a temp file")?;
    wasm.write_all(EXAMPLE_SCORE)
        .context("failed to write the score guest to a temp file")?;

    let mut cli = Command::new(env!("CARGO_BIN_EXE_starstream-run"))
        .arg("new")
        .arg(wasm.path())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn the starstream-run CLI")?;
    let mut stdout = cli.stdout.take().context("the CLI stdout was not piped")?;

    cli.wait().await.context("failed to await the CLI")?;

    let mut out = String::new();
    stdout
        .read_to_string(&mut out)
        .await
        .context("failed to read the CLI output")?;
    assert_eq!(
        out,
        r#"package starstream:utxo;

interface score-progress {
    plus-chips: func(chips2: u64);
    plus-mult: func(mult2: u64);
    mult-mult: func(mult-pct: u64);
    finish: func();
}
"#
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn new_serve() -> anyhow::Result<()> {
    let mut wasm = NamedTempFile::new().context("failed to create a temp file")?;
    wasm.write_all(EXAMPLE_SCORE)
        .context("failed to write the score guest to a temp file")?;

    let addr = TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))
        .context("failed to reserve a port")?
        .local_addr()
        .context("failed to read the reserved address")?;

    let mut cli = Command::new(env!("CARGO_BIN_EXE_starstream-run"))
        .arg("--serve")
        .arg(addr.to_string())
        .arg("new")
        .arg(wasm.path())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn the starstream-run CLI")?;
    let mut stdout = cli.stdout.take().context("the CLI stdout was not piped")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).await.is_err() {
        if Instant::now() >= deadline {
            panic!("the CLI never started serving on `{addr}`");
        }
        sleep(Duration::from_millis(50)).await;
    }
    sleep(Duration::from_millis(250)).await;

    let ws = wrpc_websockets::ClientBuilder::new().uri(&format!("ws://{addr}"))?;
    let wrpc = wrpc_websockets::Client::from(ws);

    score_progress::plus_chips(&wrpc, (), 7).await?;
    score_progress::plus_mult(&wrpc, (), 6).await?;
    score_progress::finish(&wrpc, ()).await?;

    cli.kill().await.context("failed to kill the CLI")?;
    cli.wait().await.context("failed to await the CLI")?;

    let mut out = String::new();
    stdout
        .read_to_string(&mut out)
        .await
        .context("failed to read the CLI output")?;
    assert_eq!(
        out,
        r#"package starstream:utxo;

interface score-progress {
    plus-chips: func(chips2: u64);
    plus-mult: func(mult2: u64);
    mult-mult: func(mult-pct: u64);
    finish: func();
}
"#
    );
    Ok(())
}
