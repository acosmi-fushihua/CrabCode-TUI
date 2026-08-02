//! Unix-only 集成测试：全程走 `UnixStream`。
//!
//! cfg 必须打在 **crate 根**而不是逐个 item 上 —— 逐 item 时非 unix 平台会把
//! 所有 item 移除、头部 import 全部变成 `unused_imports`，让整个集成测试目标
//! 在 `cargo clippy --all-targets -- -D warnings` 下编不过（Windows 本机实证）。
#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use acosmi_memory_orchestrator::ping_endpoint;
use anyhow::{bail, Context, Result};
use tokio::process::Command;

#[tokio::test]
async fn binary_serves_memory_ping_over_unix_socket() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-test.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let result = async {
        let pong = wait_for_ping(&mut child, &endpoint).await?;
        let newline_pong = ping_unix_socket_with_newline_without_eof(&socket_path).await?;
        Ok::<_, anyhow::Error>((pong, newline_pong))
    }
    .await;

    if let Err(e) = child.kill().await {
        if e.kind() != std::io::ErrorKind::InvalidInput {
            return Err(e.into());
        }
    }
    let _status = child.wait().await?;

    let (pong, newline_pong) = result?;
    assert_eq!(pong["ok"], true);
    assert_eq!(pong["service"], "acosmi-memory-orchestrator");

    assert_eq!(newline_pong["ok"], true);
    assert_eq!(newline_pong["service"], "acosmi-memory-orchestrator");

    Ok(())
}

async fn ping_unix_socket_with_newline_without_eof(
    socket_path: &std::path::Path,
) -> Result<serde_json::Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(br#"{"method":"memory.ping"}"#).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn wait_for_ping(
    child: &mut tokio::process::Child,
    endpoint: &str,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("memory-orchestrator exited before ping succeeded: {status}");
        }

        match ping_endpoint(endpoint).await {
            Ok(pong) => return Ok(pong),
            Err(e) => last_error = Some(e),
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    match last_error {
        Some(e) => Err(e).context("memory.ping did not succeed before timeout"),
        None => bail!("memory.ping did not run before timeout"),
    }
}
