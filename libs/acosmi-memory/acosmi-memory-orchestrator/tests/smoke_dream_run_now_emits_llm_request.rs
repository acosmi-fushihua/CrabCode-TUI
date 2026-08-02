// W-MEMORY-EVOLUTION PR-12 (2026-05-29) — end-to-end smoke test.
//
// Validates the FULL reverse-IPC emit path through the real orchestrator
// binary, exercising the wiring landed across PR-1/PR-3/PR-5/PR-10:
//
//   memory.events.subscribe (PR-1 long-conn transport)
//     → memory.dream.run_now (PR-10 run_now real-execute: spawn tier3
//       processor.process())
//     → tier3 processor emits memory/tier/llmCallRequest via the
//       UdsBroadcastEmitter (PR-3) over the EventSink
//     → the subscriber connection receives the pushed frame.
//
// This is the saga's "the dead scaffolding is now alive" proof at the
// real-binary level: a manual dream trigger actually drives the orchestrator
// to emit an LLM call request to the (would-be) TS proxy. No TS proxy runs in
// this test, so the dream process will eventually time out awaiting the
// result — but the EMIT (what this test asserts) happens immediately, proving
// the transport + emitter + run_now-execute chain is end-to-end live.
//
// A3 fix (P0-3, 2026-06-05) note: `DreamProcessor::process` now SETTLES the
// real `.consolidate-lock` at every exit (success → fresh-mtime release;
// failure → rollback). This smoke test deliberately never delivers an LLM
// result, so `process()` only settles after the full 60s LLM timeout (well
// past the 10s assertion window + child kill here) — so lock re-acquirability
// is NOT asserted at this level. It IS covered by the focused unit tests in
// `tier3_auto_dream.rs`
// (`process_success_releases_lock_with_fresh_mtime_and_is_reacquirable`,
// `process_failure_rolls_lock_back_to_prior_mtime_and_is_reacquirable`,
// `process_does_not_clobber_lock_when_we_are_not_the_holder`), which drive the
// pipeline to terminal both ways and assert `lock::try_acquire` succeeds after.

#![cfg(unix)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

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
async fn dream_run_now_emits_llm_call_request_over_events_connection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-smoke.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    // A memory_dir for the dream to consolidate. W-MEMORY-SYNERGY W4
    // (2026-07-16, RC-7a)：run_now 带空语料门 —— 夹具必须有一份压缩后
    // 非空的主会话转写（`<project>/<uuid>.jsonl`），否则 run_now 以
    // corpus_empty 拒绝、不再 emit。
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir)?;
    std::fs::write(
        memory_dir.join("SESSION.md"),
        "## Notes\n- smoke test session\n",
    )?;
    std::fs::write(
        dir.path()
            .join("550e8400-e29b-41d4-a716-446655440500.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"type":"user","message":{"role":"user","content":"smoke corpus user line"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":"smoke corpus assistant line"}}),
        ),
    )?;

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        // Long heartbeat so heartbeats don't interleave the assertion window.
        .env("CRABCODE_MEMORY_HEARTBEAT_MS", "60000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let result = async {
        wait_for_ready(&mut child, &socket_path).await?;

        // 1. Open the events long-connection + consume the ack.
        let mut sub = UnixStream::connect(&socket_path).await?;
        sub.write_all(b"{\"method\":\"memory.events.subscribe\"}\n")
            .await?;
        sub.flush().await?;
        let mut reader = BufReader::new(sub);
        let mut ack = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut ack))
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for subscribe ack"))??;
        assert_eq!(
            serde_json::from_str::<Value>(ack.trim())?["subscribed"],
            Value::Bool(true)
        );

        // 2. Fire a manual dream run_now on a separate one-shot connection.
        //    PR-10 makes this spawn tier3_processor.process(), which emits the
        //    reverse-IPC LLM call request over the EventSink.
        let mut runner = UnixStream::connect(&socket_path).await?;
        let body = json!({
            "method": "memory.dream.run_now",
            "payload": {
                "session_id": SESSION_ID,
                "current_session_id": SESSION_ID,
                "memory_dir": memory_dir.to_string_lossy(),
                "now_ms": 1_700_200_000_000_u64,
            }
        });
        runner.write_all(&serde_json::to_vec(&body)?).await?;
        runner.shutdown().await?;
        let mut run_resp = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), runner.read_to_end(&mut run_resp))
            .await
            .map_err(|_| anyhow::anyhow!("timed out reading run_now response"))??;
        // run_now returns immediately (detached process); response is the
        // trigger envelope (+ dream_run started field).
        let _resp: Value = serde_json::from_slice(&run_resp)?;

        // 3. The subscriber connection must receive a llmCallRequest frame
        //    (tier=Dream) — the emit driven by the spawned dream process.
        //    Read frames until we find it (skip any heartbeat).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() >= deadline {
                bail!("timed out waiting for memory/tier/llmCallRequest emit");
            }
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .map_err(|_| anyhow::anyhow!("read timed out"))??;
            if n == 0 {
                bail!("events connection closed before llmCallRequest");
            }
            let frame: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match frame.get("notification").and_then(Value::as_str) {
                Some("memory/tier/llmCallRequest") => {
                    let payload = &frame["payload"];
                    assert_eq!(
                        payload["tier"], "Dream",
                        "dream run_now must emit a Dream-tier LLM call request"
                    );
                    assert!(
                        payload["req_id"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("tier3-")),
                        "req_id must carry the tier3 prefix; got {:?}",
                        payload["req_id"]
                    );
                    assert!(
                        payload["messages"]
                            .as_array()
                            .is_some_and(|m| !m.is_empty()),
                        "LLM call request must carry assembled messages"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
                // Heartbeat or any other frame → keep reading.
                _ => continue,
            }
        }
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
