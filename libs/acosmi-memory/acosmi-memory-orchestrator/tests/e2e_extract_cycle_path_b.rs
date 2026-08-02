use std::fs;

use acosmi_memory_orchestrator::ipc_handler::IpcHandler;
use serde_json::{json, Value};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const LAST_ASSISTANT_ID: &str = "770e8400-e29b-41d4-a716-446655440000";

fn request(method: &str, payload: Value) -> Value {
    json!({ "method": method, "payload": payload })
}

#[tokio::test]
async fn e2e_extract_cycle_path_b_returns_cursor_decision_then_indexes_written_paths() {
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = dir.path().join("memory");
    fs::create_dir_all(&memory_dir).unwrap();

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
                    "EXTRACT_MEMORIES": true,
                    "auto_memory_enabled": true
                },
                "requested_kinds": ["extract"],
                "now_ms": 1_700_200_000_000_u64
            }),
        ))
        .await;

    let triggers = evaluate["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0]["kind"], "extract");
    assert_eq!(
        triggers[0]["runner_payload"]["last_assistant_uuid"],
        LAST_ASSISTANT_ID
    );
    assert_eq!(triggers[0]["runner_payload"]["new_message_count"], 2);
    assert!(triggers[0].get("lock_token").is_none());

    let memory_file = memory_dir.join("topic.md");
    fs::write(&memory_file, "---\ntype: project\n---\nremembered").unwrap();
    let completed = handler
        .handle_value(request(
            "memory.runner.completed",
            json!({
                "trigger_id": triggers[0]["trigger_id"],
                "kind": "extract",
                "written_paths": [memory_file.to_string_lossy()],
                "usage": { "output_tokens": 7 },
                "leader_token": leader["leader_token"],
                "leader_epoch": leader["leader_epoch"]
            }),
        ))
        .await;

    assert_eq!(completed["ok"], true);
    assert_eq!(completed["cursor_updated"], true);
    assert_eq!(completed["indexed_path_count"], 1);
    assert!(dir
        .path()
        .join(".memory-rust-derived/indexes/written-paths.jsonl")
        .exists());

    let next = handler
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
                    "EXTRACT_MEMORIES": true,
                    "auto_memory_enabled": true
                },
                "requested_kinds": ["extract"],
                "now_ms": 1_700_200_001_000_u64
            }),
        ))
        .await;

    assert!(next["triggers"].as_array().unwrap().is_empty());
    assert!(!memory_dir.join(".rust-derived").exists());
}
