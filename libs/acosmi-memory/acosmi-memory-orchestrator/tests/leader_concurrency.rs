// Integration stress test for the leader-election IPC surface. Complements
// in-process unit coverage by exercising the full real binary path:
// orchestrator process → UDS socket → JSON wire frame → ipc_handler →
// leader_lock filesystem CAS.
//
// Scenarios covered here:
// 1. N concurrent claim clients → exactly one wins. Closes the concurrent
//    bootstrap race at the IPC layer (the unit-test
//    `competing_claim_grants_one_owner` already covers the in-process file
//    CAS; this adds the wire layer).
// 2. Release-then-immediate-claim → another client wins without waiting for
//    TTL. Confirms graceful-exit handoff works end-to-end.
//
// Other scenarios (dead PID takeover / stale lease takeover / fail-soft when
// the bridge is unavailable / follower watchdog) are covered by library and
// TypeScript unit tests — those exercise the same code
// paths through different entry points; adding the same coverage at
// integration level would only flake without adding signal.

#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

async fn send_claim(socket: &std::path::Path, memory_dir: &str, owner_pid: u32) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).await?;
    let body = json!({
        "method": "memory.leader.claim",
        "payload": {
            "memory_dir": memory_dir,
            "owner_pid": owner_pid,
            "ttl_ms": 60_000_u64,
        },
    });
    stream.write_all(&serde_json::to_vec(&body)?).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn send_release(
    socket: &std::path::Path,
    memory_dir: &str,
    owner_pid: u32,
    leader_token: &str,
    leader_epoch: u64,
) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).await?;
    let body = json!({
        "method": "memory.leader.release",
        "payload": {
            "memory_dir": memory_dir,
            "owner_pid": owner_pid,
            "leader_token": leader_token,
            "leader_epoch": leader_epoch,
        },
    });
    stream.write_all(&serde_json::to_vec(&body)?).await?;
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
async fn three_concurrent_claims_grant_exactly_one_owner_over_ipc() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-test.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");
    let memory_dir = dir.path().join("memdir");
    std::fs::create_dir_all(&memory_dir)?;
    let memory_dir_str = memory_dir.to_string_lossy().to_string();

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
        wait_for_ready(&mut child, &socket_path).await?;

        // Spawn 3 client tasks concurrently. All use the same OS PID
        // (this test process) — that mimics production where multiple
        // Bun workers bootstrap as distinct OS processes but all are
        // "alive" from the orchestrator's PID-check vantage. Using
        // distinct fake PIDs would make `is_process_running` return false
        // for each and trigger dead-PID takeover, defeating the test.
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let mut handles = Vec::new();
        let owner_pid = std::process::id();
        for _ in 0..3 {
            let socket_path = socket_path.clone();
            let memory_dir_str = memory_dir_str.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                send_claim(&socket_path, &memory_dir_str, owner_pid).await
            }));
        }

        let mut granted = 0usize;
        for h in handles {
            let res = h.await??;
            if res.get("granted").and_then(Value::as_bool) == Some(true) {
                granted += 1;
            }
        }

        assert_eq!(
            granted, 1,
            "exactly one of three concurrent claims must win, got {granted}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

#[tokio::test]
async fn release_lets_next_claim_succeed_immediately_over_ipc() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-test.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");
    let memory_dir = dir.path().join("memdir");
    std::fs::create_dir_all(&memory_dir)?;
    let memory_dir_str = memory_dir.to_string_lossy().to_string();

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
        wait_for_ready(&mut child, &socket_path).await?;

        // Use the current test PID for both clients (see neighbour test
        // for rationale — distinct fake PIDs trigger dead-PID takeover).
        let pid_a = std::process::id();
        let pid_b = std::process::id();

        // A claims.
        let a = send_claim(&socket_path, &memory_dir_str, pid_a).await?;
        assert_eq!(a["granted"], Value::Bool(true), "A's claim must grant");

        // B tries — must be denied since A holds the lease (same live PID,
        // unexpired TTL).
        let b_denied = send_claim(&socket_path, &memory_dir_str, pid_b).await?;
        assert_eq!(
            b_denied["granted"],
            Value::Bool(false),
            "B's claim must be denied while A holds fresh lease"
        );

        // A releases.
        let release = send_release(
            &socket_path,
            &memory_dir_str,
            pid_a,
            a["leader_token"].as_str().expect("leader token"),
            a["leader_epoch"].as_u64().expect("leader epoch"),
        )
        .await?;
        assert_eq!(
            release["ok"],
            Value::Bool(true),
            "A's release must succeed (idempotent ok)"
        );
        assert_eq!(release["released"], Value::Bool(true));

        // B tries again — must succeed without waiting for TTL.
        let b_succ = send_claim(&socket_path, &memory_dir_str, pid_b).await?;
        assert_eq!(
            b_succ["granted"],
            Value::Bool(true),
            "B's post-release claim must succeed immediately (no TTL wait)"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
