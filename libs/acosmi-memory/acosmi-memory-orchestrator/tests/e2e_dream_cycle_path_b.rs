use std::fs;

use acosmi_memory_orchestrator::dream_config::{write_dream_config, DreamConfig};
use acosmi_memory_orchestrator::ipc_handler::IpcHandler;
use acosmi_memory_orchestrator::lock::last_consolidated_at;
use filetime::{set_file_mtime, FileTime};
use serde_json::{json, Value};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const OTHER_SESSION: &str = "660e8400-e29b-41d4-a716-446655440000";
const LAST_ASSISTANT_ID: &str = "770e8400-e29b-41d4-a716-446655440000";

fn request(method: &str, payload: Value) -> Value {
    json!({ "method": method, "payload": payload })
}

#[tokio::test]
async fn e2e_dream_cycle_path_b_returns_decision_then_completes_without_rust_llm_call() {
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = dir.path().join("memory");
    fs::create_dir_all(&memory_dir).unwrap();
    let transcript = dir.path().join(format!("{OTHER_SESSION}.jsonl"));
    fs::write(&transcript, "{}\n").unwrap();
    set_file_mtime(&transcript, FileTime::from_unix_time(1_700_100_000, 0)).unwrap();
    write_dream_config(
        dir.path(),
        &DreamConfig {
            enabled: true,
            min_hours: 24,
            min_sessions: 1,
            session_scan_interval_ms: 600_000,
            auto_promote: Default::default(),
            imagination_min_hours: 48,
            ..DreamConfig::default()
        },
    )
    .await
    .unwrap();

    let handler = IpcHandler::new();
    handler.set_base_dir(dir.path().to_path_buf());
    let leader = handler
        .handle_value(request(
            "memory.leader.claim",
            json!({
                "memory_dir": dir.path().join(".memory-rust-derived").to_string_lossy(),
                "owner_pid": std::process::id(),
                "ttl_ms": 60_000,
            }),
        ))
        .await;
    assert_eq!(leader["granted"], true);
    let evaluate = handler
        .handle_value(request(
            "memory.turn_end.evaluate",
            json!({
                "recovery_schema_version": 1,
                "session_id": SESSION_ID,
                "current_session_id": SESSION_ID,
                "last_assistant_uuid": LAST_ASSISTANT_ID,
                "project_cwd": dir.path().to_string_lossy(),
                "transcript_path": dir.path().join(format!("{SESSION_ID}.jsonl")).to_string_lossy(),
                "memory_dir": memory_dir.to_string_lossy(),
                "message_counts": { "user": 1, "assistant": 1, "total": 2 },
                "feature_flags": {
                    "auto_memory_enabled": true,
                    "auto_dream_enabled": true
                },
                "requested_kinds": ["dream"],
                "now_ms": 1_700_200_000_000_u64
            }),
        ))
        .await;

    let triggers = evaluate["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0]["kind"], "dream");
    assert!(triggers[0]["lock_token"].is_string());
    assert_eq!(
        triggers[0]["runner_payload"]["sessions_since_last_consolidation"],
        1
    );
    assert!(!serde_json::to_string(&evaluate)
        .unwrap()
        .contains(&format!("memory.{}.run", "dream")));

    let memory_file = memory_dir.join("dreamed.md");
    fs::write(&memory_file, "---\ntype: project\n---\nupdated").unwrap();
    let completed = handler
        .handle_value(request(
            "memory.runner.completed",
            json!({
                "trigger_id": triggers[0]["trigger_id"],
                "kind": "dream",
                "written_paths": [memory_file.to_string_lossy()],
                "usage": { "output_tokens": 21 },
                "leader_token": leader["leader_token"],
                "leader_epoch": leader["leader_epoch"]
            }),
        ))
        .await;

    assert_eq!(completed["ok"], true);
    assert_eq!(completed["lock_released"], true);
    assert_eq!(completed["indexed_path_count"], 1);
    assert_eq!(
        fs::read_to_string(memory_dir.join(".consolidate-lock")).unwrap(),
        ""
    );
    assert!(last_consolidated_at(&memory_dir).await.unwrap() > 0);
    assert!(dir
        .path()
        .join(".memory-rust-derived/archives/runner-completed.jsonl")
        .exists());
    assert!(!memory_dir.join(".rust-derived").exists());
}
