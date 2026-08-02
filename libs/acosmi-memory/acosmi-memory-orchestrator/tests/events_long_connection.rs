// W-MEMORY-EVOLUTION PR-1 (2026-05-29) — events 长连传输地基集成测试。
//
// Exercises the full real-binary path: orchestrator process → UDS socket →
// `memory.events.subscribe` request → connection kept open → heartbeat frame
// pushed over the same connection. Complements `event_sink.rs` unit tests
// (in-process push/prune) by validating the wire + accept-loop integration.
//
// Heartbeat interval is driven down to 50ms via `CRABCODE_MEMORY_HEARTBEAT_MS`
// so the test does not wait the 30s production default.

#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

async fn ping_socket(socket: &std::path::Path) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(br#"{"method":"memory.ping"}"#).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn wait_for_ready(child: &mut tokio::process::Child, socket: &std::path::Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("orchestrator exited before ping: {status}");
        }
        if ping_socket(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("orchestrator failed to become ready within 10s")
}

#[tokio::test]
async fn events_subscribe_keeps_connection_open_and_pushes_heartbeat() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-events.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        // Drive heartbeat down so we don't wait the 30s production default.
        .env("CRABCODE_MEMORY_HEARTBEAT_MS", "50")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let result = async {
        wait_for_ready(&mut child, &socket_path).await?;

        let mut stream = UnixStream::connect(&socket_path).await?;
        stream
            .write_all(b"{\"method\":\"memory.events.subscribe\"}\n")
            .await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);

        // 1. Ack frame.
        let mut ack_line = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut ack_line))
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for ack"))??;
        let ack: Value = serde_json::from_str(ack_line.trim())?;
        assert_eq!(ack["ok"], Value::Bool(true), "ack ok must be true");
        assert_eq!(
            ack["subscribed"],
            Value::Bool(true),
            "ack subscribed must be true"
        );

        // 2. At least one heartbeat frame within a generous bound (interval
        //    is 50ms; first heartbeat lands after one interval).
        let mut hb_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut hb_line))
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for heartbeat"))??;
        let hb: Value = serde_json::from_str(hb_line.trim())?;
        assert_eq!(
            hb["notification"], "memory/events/heartbeat",
            "frame must be a heartbeat notification"
        );
        assert!(
            hb["ts_ms"].as_u64().is_some(),
            "heartbeat must carry a ts_ms"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

#[tokio::test]
async fn non_subscribe_request_still_one_shot() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-oneshot.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        .env("CRABCODE_MEMORY_HEARTBEAT_MS", "50")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let result = async {
        wait_for_ready(&mut child, &socket_path).await?;

        // A plain ping must still get a one-shot response and the connection
        // closes (read_to_end returns EOF).
        let mut stream = UnixStream::connect(&socket_path).await?;
        stream.write_all(br#"{"method":"memory.ping"}"#).await?;
        stream.shutdown().await?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("timed out reading one-shot response"))??;
        let resp: Value = serde_json::from_slice(&buf)?;
        assert_eq!(
            resp["ok"],
            Value::Bool(true),
            "memory.ping one-shot response must be ok"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
