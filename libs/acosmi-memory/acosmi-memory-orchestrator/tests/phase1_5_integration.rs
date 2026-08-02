use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use acosmi_memory_orchestrator::daily_log::{
    append_daily_log, SessionEvent, TranscriptMeta as DailyLogTranscriptMeta,
};
use acosmi_memory_orchestrator::scheduler::{run_startup_scan, SchedulerConfig};
use acosmi_memory_orchestrator::status::{
    build_health, build_status, list_dedup_groups, StatusRequest,
};
use serde_json::json;
use tempfile::TempDir;

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn write_file(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn memory_doc(description: &str, body: &str) -> String {
    format!("---\ntype: project\ndescription: {description}\n---\n{body}")
}

fn status_request(dir: &TempDir) -> StatusRequest {
    StatusRequest::new(
        dir.path().join("memory"),
        dir.path().join("workspace"),
        dir.path(),
        dir.path().join("transcripts"),
    )
    .with_now(UNIX_EPOCH + Duration::from_secs(2_000_000_000))
}

#[tokio::test]
async fn phase1_5_status_health_dedup_and_scheduler_are_query_only() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("workspace")).unwrap();

    write_file(
        &dir.path().join("memory/primary.md"),
        &memory_doc("primary", "same body\n"),
    );
    write_file(
        &dir.path().join("memory/duplicate.md"),
        &memory_doc("duplicate", "same body\n"),
    );
    write_file(
        &dir.path().join("memory/MEMORY.md"),
        "- [Primary](primary.md)\n",
    );
    write_file(
        &dir.path()
            .join("transcripts")
            .join(format!("{SESSION_ID}.jsonl")),
        "{}\n",
    );

    append_daily_log(
        dir.path(),
        &DailyLogTranscriptMeta {
            session_id: SESSION_ID.to_owned(),
            path: dir
                .path()
                .join("transcripts")
                .join(format!("{SESSION_ID}.jsonl")),
            mtime_ms: 2_000_000_000_000,
            size_bytes: 3,
            sealed: true,
        },
        &[SessionEvent {
            event_id: "evt-1".to_owned(),
            kind: "turn_end".to_owned(),
            occurred_at_ms: 2_000_000_000_000,
            payload: json!({ "deterministic": true }),
        }],
    )
    .await
    .unwrap();

    let request = status_request(&dir);
    let status = build_status(&request).unwrap();
    let health = build_health(&request).unwrap();
    let dedup = list_dedup_groups(&request.memory_dir).unwrap();
    let scheduler =
        run_startup_scan(&SchedulerConfig::session(request, Duration::from_secs(60))).unwrap();

    assert_eq!(status.dedup.duplicate_group_count, 1);
    assert!(status.daily_log.exists);
    assert_eq!(status.transcript_index.transcript_count, 1);
    assert!(health.ok);
    assert!(health.derived_root.is_sibling_of_memory_dir);
    assert_eq!(dedup.duplicate_group_count, 1);
    assert_eq!(scheduler.status.dedup.duplicate_group_count, 1);
    assert!(scheduler.emitted_llm_trigger_methods.is_empty());
    assert!(!scheduler.pending_triggers_written);
    assert!(!dir
        .path()
        .join(".memory-rust-derived/pending-triggers")
        .exists());
    assert!(!dir.path().join("memory/.rust-derived").exists());
}
