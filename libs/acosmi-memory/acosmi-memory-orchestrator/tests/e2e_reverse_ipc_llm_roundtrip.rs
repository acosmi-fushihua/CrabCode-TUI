// W-MEMORY-EVOLUTION W11 PR-6 (2026-05-29) — end-to-end reverse-IPC roundtrip.
//
// The existing `smoke_dream_run_now_emits_llm_request.rs` only proves HOP1
// (the orchestrator binary EMITS a `memory/tier/llmCallRequest` frame over the
// events long-connection). It stops there — no TS proxy writes a result back,
// so the dream process eventually times out awaiting the LLM result and nothing
// is written to disk.
//
// This test closes the gap: it runs a "fake TS proxy" that drives HOP2-4 of the
// reverse-IPC contract against the REAL orchestrator binary:
//
//   HOP1  orchestrator emits `memory/tier/llmCallRequest` over the events conn
//   HOP2  fake proxy reads the frame, builds a valid canned LLM result, and
//         writes it back via the `memory.tier.llm_call_result` IPC method
//   HOP3  orchestrator's `deliver_result` matches the `req_id` and resolves the
//         tier processor's awaited oneshot
//   HOP4  the tier processor parses the result and writes it to disk
//         (dream → `dreams/insight_*.md`; extract → typed `*.md` + MEMORY.md)
//         — and (for the extract path) `memory.search` retrieves the written
//         content.
//
// Two test functions:
//   * `dream_roundtrip_writes_insight_to_disk`     — tier3 via `memory.dream.run_now`
//   * `extract_roundtrip_writes_and_is_searchable` — tier2 via `memory.tier2.process`
//
// Both exercise the actual UDS transport + broadcast emitter + pending-oneshot
// delivery, not the in-process unit-test shortcuts (`IpcHandler::new()` +
// in-memory `RecordingEmitter` + direct `proc.deliver_result`).
//
// DOWNGRADE NOTE (dream search): the dream pipeline writes `type: insight`
// markdown, which the SE indexer's adapter (`category_map::map_type`) maps to
// `None` (only user/feedback/project/reference are indexable). So dream insight
// files are NOT retrievable via `memory.search` by design. The dream test
// therefore asserts the strongest verifiable HOP4 for that path — the insight
// file is truly written to disk with the proxy-supplied content — and the
// extract test owns the `memory.search` retrievability assertion (it writes
// `type: project` markdown, which IS indexable).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

// ── shared spawn / ready / one-shot helpers (mirrors the smoke test) ──

async fn ping_socket(socket: &Path) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(br#"{"method":"memory.ping"}"#).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn wait_for_ready(child: &mut tokio::process::Child, socket: &Path) -> Result<()> {
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

/// One-shot request → response → close. The orchestrator frames requests by
/// reading until the first `\n` (`read_ipc_request`), so we terminate the body
/// with a newline; the response is read to EOF after the server shuts down its
/// write half.
async fn one_shot(socket: &Path, body: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).await?;
    let mut bytes = serde_json::to_vec(body)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading one-shot response"))??;
    Ok(serde_json::from_slice(&buf)?)
}

/// Open the events long-connection and consume the subscribe ack. Returns a
/// buffered reader over the read half; the orchestrator pushes notification
/// frames (`{"notification": ..., "payload": ...}`) over it.
async fn subscribe_events(socket: &Path) -> Result<BufReader<UnixStream>> {
    let mut sub = UnixStream::connect(socket).await?;
    sub.write_all(b"{\"method\":\"memory.events.subscribe\"}\n")
        .await?;
    sub.flush().await?;
    let mut reader = BufReader::new(sub);
    let mut ack = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut ack))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for subscribe ack"))??;
    let ack: Value = serde_json::from_str(ack.trim())?;
    if ack["subscribed"] != Value::Bool(true) {
        bail!("unexpected subscribe ack: {ack}");
    }
    Ok(reader)
}

/// Build a canned LLM response string for the dream (tier3) pipeline keyed on
/// the request's `phase` field. The shapes are exactly what the tier3 phase
/// parsers expect; phase2 carries a high-weight evidence ref so phase3 resolves
/// to `confidence: high` (→ written as `insight_*.md`, not `fragment_*.md`).
fn dream_phase_response(phase: &str) -> String {
    match phase {
        "phase1" => json!({
            "themes": [{ "id": "widget-pipeline", "label": "Widget rendering pipeline" }]
        })
        .to_string(),
        "phase2" => json!({
            "evidence_refs": [{
                "source": "SESSION.md",
                "snippet": "the widget pipeline batches frames before flushing",
                "weight": 0.92_f64
            }]
        })
        .to_string(),
        "phase3" => {
            // A frontmatter memory block; `confidence: high` + non-empty evidence
            // → tier3 writes this verbatim as `dreams/insight_<theme>.md`.
            "---\n\
             name: widget-pipeline\n\
             type: insight\n\
             description: how the widget rendering pipeline batches frames\n\
             confidence: high\n\
             ---\n\n\
             The widget pipeline batches frames before flushing to reduce draw calls.\n"
                .to_string()
        }
        // phase0 (reflection) + phase4 (prune) JSON.
        _ => json!({
            "still_valid_ids": [],
            "stale_ids": [],
            "delete_ids": [],
            "notes": ""
        })
        .to_string(),
    }
}

/// Read one notification frame off the events connection (skipping heartbeats),
/// returning `None` on a clean read timeout so callers can poll for completion.
async fn next_llm_request(
    reader: &mut BufReader<UnixStream>,
    read_timeout: Duration,
) -> Result<Option<Value>> {
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout(read_timeout, reader.read_line(&mut line)).await;
        let n = match read {
            Ok(r) => r?,
            // No frame within the window → let the caller poll/decide.
            Err(_) => return Ok(None),
        };
        if n == 0 {
            bail!("events connection closed unexpectedly");
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match frame.get("notification").and_then(Value::as_str) {
            Some("memory/tier/llmCallRequest") => return Ok(Some(frame)),
            // Heartbeat / any other frame → keep reading.
            _ => continue,
        }
    }
}

/// D8 filename contract: dream artifacts include an 8-hex hash suffix so
/// distinct raw theme ids that sanitize to the same stem cannot overwrite one
/// another. Keep this integration test aligned without duplicating the
/// production SHA-256 implementation.
fn find_widget_insight(insight_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(insight_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let Some(hash) = name
                .strip_prefix("insight_widget-pipeline_")
                .and_then(|rest| rest.strip_suffix(".md"))
            else {
                return false;
            };
            hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit())
        })
}

// ── Test 1: dream (tier3) reverse-IPC roundtrip → insight written to disk ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dream_roundtrip_writes_insight_to_disk() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-e2e-dream.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir)?;
    std::fs::write(
        memory_dir.join("SESSION.md"),
        "## Notes\n- the widget pipeline batches frames before flushing\n",
    )?;
    // W-MEMORY-SYNERGY W4 (2026-07-16, RC-7a)：run_now 带空语料门 —— 需要
    // 一份压缩后非空的主会话转写（`<project>/<uuid>.jsonl`）放行。
    std::fs::write(
        dir.path()
            .join("550e8400-e29b-41d4-a716-446655440501.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"type":"user","message":{"role":"user","content":"how does the widget pipeline flush"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":"it batches frames before flushing"}}),
        ),
    )?;

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        .env("CRABCODE_MEMORY_HEARTBEAT_MS", "60000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let insight_dir = memory_dir.join("dreams");

    let result = async {
        wait_for_ready(&mut child, &socket_path).await?;
        let mut reader = subscribe_events(&socket_path).await?;

        // HOP1 trigger: fire a manual dream. run_now spawns the detached
        // tier3 `process()`, which drives the reverse-IPC LLM phases.
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let run = one_shot(
            &socket_path,
            &json!({
                "method": "memory.dream.run_now",
                "payload": {
                    "session_id": session_id,
                    "current_session_id": session_id,
                    "memory_dir": memory_dir.to_string_lossy(),
                    "now_ms": 1_700_200_000_000_u64,
                }
            }),
        )
        .await?;
        assert_eq!(
            run["dream_run"]["started"], true,
            "run_now must report the detached dream started; got {run}"
        );

        // HOP2-4 driver loop: read each emitted llmCallRequest, write back a
        // valid result via `memory.tier.llm_call_result`, until the dream has
        // written its insight file (or we hit the overall budget).
        let overall_deadline = Instant::now() + Duration::from_secs(15);
        let mut saw_request = false;
        let insight_path = loop {
            if let Some(path) = find_widget_insight(&insight_dir) {
                break path;
            }
            if Instant::now() >= overall_deadline {
                bail!(
                    "dream did not write insight within budget (saw_request={saw_request}); \
                     dreams dir exists={}",
                    insight_dir.exists()
                );
            }
            match next_llm_request(&mut reader, Duration::from_secs(2)).await? {
                Some(frame) => {
                    let payload = &frame["payload"];
                    assert_eq!(
                        payload["tier"], "Dream",
                        "dream run_now must emit Dream-tier requests; got {payload}"
                    );
                    let req_id = payload["req_id"]
                        .as_str()
                        .expect("req_id must be a string")
                        .to_string();
                    assert!(
                        req_id.starts_with("tier3-"),
                        "dream req_id must carry the tier3- prefix; got {req_id}"
                    );
                    let phase = payload["phase"].as_str().unwrap_or("");
                    // HOP2: write the LLM result back over the real IPC method.
                    let ack = one_shot(
                        &socket_path,
                        &json!({
                            "method": "memory.tier.llm_call_result",
                            "payload": {
                                "req_id": req_id,
                                "response": dream_phase_response(phase),
                            }
                        }),
                    )
                    .await?;
                    // HOP3: delivery resolved the awaited oneshot for this req.
                    assert_eq!(ack["ok"], true, "llm_call_result must ok; got {ack}");
                    assert_eq!(
                        ack["received"], true,
                        "the orchestrator must MATCH the req_id (HOP3 delivery); got {ack}"
                    );
                    saw_request = true;
                }
                // No request this poll — give the detached dream task time to
                // advance, then re-check the disk + poll again.
                None => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };

        assert!(
            saw_request,
            "the dream must have emitted at least one llmCallRequest"
        );

        // HOP4: the insight file is on disk with the proxy-supplied content.
        let written = std::fs::read_to_string(&insight_path)?;
        assert!(
            written.contains("type: insight") && written.contains("batches frames"),
            "insight file must carry the consolidated body written from the LLM \
             result; got:\n{written}"
        );

        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

// ── Test 2: extract (tier2) reverse-IPC roundtrip → written + searchable ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_roundtrip_writes_and_is_searchable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-e2e-extract.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir)?;

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        .env("CRABCODE_MEMORY_HEARTBEAT_MS", "60000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // The LLM extraction output: ONE memory block (valid frontmatter, indexable
    // `type: project`) + a matching MEMORY.md index line. tier2 parses this into
    // a block, writes `<memory_dir>/widget_pipeline.md`, and appends the index
    // line to MEMORY.md.
    let extraction_output = "---\n\
        type: project\n\
        name: widget_pipeline\n\
        description: the widget rendering pipeline batches frames before flushing\n\
        created_at: 2026-05-29\n\
        ---\n\n\
        The widget pipeline batches frames before flushing to reduce GPU draw calls.\n\n\
        - [Widget pipeline](widget_pipeline.md) — batches frames before flushing\n";

    let result = async {
        wait_for_ready(&mut child, &socket_path).await?;
        let mut reader = subscribe_events(&socket_path).await?;

        // HOP1 trigger: drive `memory.tier2.process` directly with a gate
        // payload (the extraction system+user messages). This emits a single
        // `tier2-` llmCallRequest and awaits its result. We run it on a
        // detached one-shot connection (the request blocks until the proxy
        // writes the result back, so we cannot await it inline on this task).
        let socket_for_proc = socket_path.to_path_buf();
        let memory_dir_str = memory_dir.to_string_lossy().to_string();
        let proc_task = tokio::spawn(async move {
            one_shot(
                &socket_for_proc,
                &json!({
                    "method": "memory.tier2.process",
                    "payload": {
                        "session_key": "session-e2e",
                        "memory_dir": memory_dir_str,
                        "gate_payload": {
                            "messages": [
                                { "role": "system", "content": "Extract durable memories.\n{{existing_manifest}}" },
                                { "role": "user", "content": "Recent messages (compacted):\n\n- The widget pipeline batches frames before flushing." }
                            ],
                            "visible_message_count_at_trigger": 8
                        }
                    }
                }),
            )
            .await
        });

        // HOP2-3: answer the single tier2 llmCallRequest with the extraction
        // output so `process()` can advance to its two-step write.
        let frame = next_llm_request(&mut reader, Duration::from_secs(10))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no tier2 llmCallRequest emitted"))?;
        let payload = &frame["payload"];
        assert_eq!(
            payload["tier"], "Extract",
            "tier2 must emit an Extract-tier request; got {payload}"
        );
        let req_id = payload["req_id"]
            .as_str()
            .expect("req_id must be a string")
            .to_string();
        assert!(
            req_id.starts_with("tier2-"),
            "extract req_id must carry the tier2- prefix; got {req_id}"
        );
        let ack = one_shot(
            &socket_path,
            &json!({
                "method": "memory.tier.llm_call_result",
                "payload": { "req_id": req_id, "response": extraction_output }
            }),
        )
        .await?;
        assert_eq!(
            ack["received"], true,
            "the orchestrator must MATCH the tier2 req_id (HOP3 delivery); got {ack}"
        );

        // HOP4 (write): the process call now completes; assert it reports the
        // typed memory file was written.
        let proc_resp = tokio::time::timeout(Duration::from_secs(10), proc_task)
            .await
            .map_err(|_| anyhow::anyhow!("tier2.process did not return after result delivery"))???;
        assert_eq!(
            proc_resp["blocks_written"], 1,
            "tier2.process must write exactly one block; got {proc_resp}"
        );
        let written_paths = proc_resp["written_paths"]
            .as_array()
            .expect("written_paths array");
        assert_eq!(written_paths.len(), 1, "one written path expected");
        let written_file = memory_dir.join("widget_pipeline.md");
        assert!(
            written_file.exists(),
            "the typed memory markdown must be on disk; got {proc_resp}"
        );
        let body = std::fs::read_to_string(&written_file)?;
        assert!(
            body.contains("type: project") && body.contains("batches frames"),
            "written memory must carry the LLM-extracted block; got:\n{body}"
        );

        // Establish the memory_dir + stand up the SE integration. A real
        // session always runs `memory.turn_end.evaluate` (it is what sets
        // `last_memory_dir` and lazily inits the SE with a full index pass).
        // We run it AFTER the extract write so that pass indexes the
        // just-written `type: project` file — not relying on the async
        // (debounced) fs-event daemon. (`memory.tier2.process` itself does not
        // set `last_memory_dir`, which is why a turn_end is required here,
        // mirroring production.)
        //
        // W-MEMORY-EVOLUTION FIX #13 (2026-06-01): the initial index pass now
        // runs on a background `spawn_blocking` task (so cold-start turn_end
        // returns promptly), so HOP4 polls memory.search until the index
        // lands rather than assuming a synchronous pass.
        let te = one_shot(
            &socket_path,
            &json!({
                "method": "memory.turn_end.evaluate",
                "payload": {
                    "recovery_schema_version": 1,
                    "session_id": "550e8400-e29b-41d4-a716-446655440000",
                    "current_session_id": "550e8400-e29b-41d4-a716-446655440000",
                    "last_assistant_uuid": "770e8400-e29b-41d4-a716-446655440000",
                    "project_cwd": dir.path().to_string_lossy(),
                    "transcript_path": dir.path().join(
                        "550e8400-e29b-41d4-a716-446655440000.jsonl"
                    ).to_string_lossy(),
                    "memory_dir": memory_dir.to_string_lossy(),
                    "message_counts": { "user": 1, "assistant": 1, "total": 2 },
                    "feature_flags": {},
                    "requested_kinds": [],
                    "now_ms": 1_700_200_000_000_u64
                }
            }),
        )
        .await?;
        assert!(
            te.get("triggers").is_some(),
            "turn_end.evaluate returns a triggers block; got {te}"
        );

        // HOP4 (search): `memory.search` resolves the now-attached SE and
        // returns the indexed hit. This proves the result is not only written
        // but retrievable end-to-end. Poll until the background initial index
        // pass (FIX #13) lands.
        let mut search = json!(null);
        for _ in 0..200 {
            search = one_shot(
                &socket_path,
                &json!({
                    "method": "memory.search",
                    "payload": { "query": "widget pipeline frames", "top_k": 10 }
                }),
            )
            .await?;
            if search["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(search["ok"], true, "search must ok; got {search}");
        let results = search["results"]
            .as_array()
            .expect("results array");
        assert!(
            !results.is_empty(),
            "the extracted memory must be retrievable via memory.search; got {search}"
        );
        let top = &results[0];
        assert!(
            top["name"].as_str().unwrap_or("").contains("widget")
                || top["source_path"]
                    .as_str()
                    .unwrap_or("")
                    .contains("widget_pipeline"),
            "top search hit must be the extracted widget_pipeline memory; got {top}"
        );

        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
