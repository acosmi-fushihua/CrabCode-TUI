use std::sync::{Arc, Barrier};
use std::thread;

use serde_json::json;
use tempfile::TempDir;

use super::*;

fn journal(dir: &TempDir) -> Journal {
    Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap()
}

fn enqueue(journal: &Journal, key: &str, kind: WorkKind) {
    assert_eq!(
        journal
            .enqueue(key, kind, &json!({"request": key}), 1_000)
            .unwrap(),
        EnqueueOutcome::Inserted
    );
}

fn fence(item: &WorkItem) -> DeliveryFence {
    DeliveryFence::new(
        item.lease_owner.clone().expect("claimed owner"),
        item.delivery_epoch,
    )
}

fn sqlite_user_version(path: &std::path::Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn sqlite_table_exists(path: &std::path::Path, table: &str) -> bool {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = ?1
         )",
        [table],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn empty_v0_initializes_once_and_v1_reopens_without_data_loss() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state/memory-journal.sqlite3");
    let initialized = Journal::open(&path).unwrap();
    assert_eq!(sqlite_user_version(&path), SCHEMA_VERSION);
    enqueue(&initialized, "runner:v1-reopen", WorkKind::RunnerTrigger);
    drop(initialized);

    let reopened = Journal::open(&path).unwrap();
    assert_eq!(sqlite_user_version(&path), SCHEMA_VERSION);
    assert_eq!(
        reopened.get("runner:v1-reopen").unwrap().unwrap().state,
        WorkState::Pending
    );
}

#[test]
fn unknown_schema_version_is_rejected_without_downgrade_or_schema_mutation() {
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let path = state_dir.join("memory-journal.sqlite3");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sentinel (value TEXT NOT NULL);
             INSERT INTO sentinel(value) VALUES ('preserve-me');
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }

    let error = Journal::open(&path).unwrap_err();
    assert!(matches!(
        error,
        JournalError::UnsupportedSchemaVersion {
            found: 2,
            supported: SCHEMA_VERSION,
        }
    ));
    assert_eq!(sqlite_user_version(&path), 2);
    assert!(sqlite_table_exists(&path, "sentinel"));
    assert!(!sqlite_table_exists(&path, "work_items"));
    let conn = Connection::open(&path).unwrap();
    let sentinel: String = conn
        .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sentinel, "preserve-me");
}

#[test]
fn unversioned_database_with_user_schema_is_not_adopted() {
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let path = state_dir.join("memory-journal.sqlite3");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE foreign_owner (id INTEGER PRIMARY KEY, value TEXT);
             INSERT INTO foreign_owner(value) VALUES ('preserve-me');",
        )
        .unwrap();
    }
    assert_eq!(sqlite_user_version(&path), 0);

    let error = Journal::open(&path).unwrap_err();
    assert!(matches!(
        error,
        JournalError::UnversionedNonEmptyDatabase { ref object }
            if object == "foreign_owner"
    ));
    assert_eq!(sqlite_user_version(&path), 0);
    assert!(sqlite_table_exists(&path, "foreign_owner"));
    assert!(!sqlite_table_exists(&path, "work_items"));
    let conn = Connection::open(&path).unwrap();
    let preserved: String = conn
        .query_row("SELECT value FROM foreign_owner", [], |row| row.get(0))
        .unwrap();
    assert_eq!(preserved, "preserve-me");
}

#[test]
fn forged_v1_schema_is_rejected_before_it_can_be_repaired_or_used() {
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let path = state_dir.join("memory-journal.sqlite3");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE work_items (
                 key TEXT PRIMARY KEY NOT NULL,
                 kind TEXT NOT NULL,
                 state TEXT NOT NULL,
                 next_attempt_at_ms INTEGER NOT NULL,
                 lease_expires_at_ms INTEGER,
                 created_at_ms INTEGER NOT NULL,
                 result_key TEXT,
                 result_recorded_at_ms INTEGER
             );
             CREATE UNIQUE INDEX idx_memory_work_result_key
                 ON work_items(result_key) WHERE result_key IS NOT NULL;
             CREATE INDEX idx_memory_work_delivery
                 ON work_items(
                     kind, state, next_attempt_at_ms, lease_expires_at_ms, created_at_ms
                 );
             CREATE INDEX idx_memory_work_settlement
                 ON work_items(state, lease_expires_at_ms, result_recorded_at_ms);
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }

    let before_sql: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'work_items'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let error = Journal::open(&path).unwrap_err();
    assert!(matches!(
        error,
        JournalError::SchemaMismatch {
            version: SCHEMA_VERSION,
            ..
        }
    ));
    assert_eq!(sqlite_user_version(&path), SCHEMA_VERSION);
    let conn = Connection::open(&path).unwrap();
    let after_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'work_items'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after_sql, before_sql);
    let updated_at_present: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_xinfo('work_items')
                 WHERE name = 'updated_at_ms'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!updated_at_present);
}

#[test]
fn enqueue_is_idempotent_but_rejects_key_reuse_for_different_work() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);

    enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);
    assert_eq!(
        journal
            .enqueue(
                "runner:t1",
                WorkKind::RunnerTrigger,
                &json!({"request": "runner:t1"}),
                9_999,
            )
            .unwrap(),
        EnqueueOutcome::Existing {
            state: WorkState::Pending
        }
    );

    let error = journal
        .enqueue(
            "runner:t1",
            WorkKind::RunnerTrigger,
            &json!({"request": "different"}),
            1_001,
        )
        .unwrap_err();
    assert!(matches!(error, JournalError::IdempotencyConflict { .. }));
}

#[test]
fn committed_pending_work_survives_reopen_and_is_claimable() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state/memory-journal.sqlite3");
    {
        let journal = Journal::open(&path).unwrap();
        enqueue(&journal, "reverse:r1", WorkKind::ReverseRequest);
    }

    let reopened = Journal::open(&path).unwrap();
    let claimed = reopened
        .claim_delivery(WorkKind::ReverseRequest, "worker-a", 2_000, 500, 8)
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].key, "reverse:r1");
    assert_eq!(claimed[0].state, WorkState::Leased);
    assert_eq!(claimed[0].delivery_epoch, 1);
    assert_eq!(claimed[0].attempts, 1);
}

#[test]
fn expired_delivery_is_reclaimed_and_old_epoch_is_fenced() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "reverse:r1", WorkKind::ReverseRequest);

    let first = journal
        .claim_delivery(WorkKind::ReverseRequest, "worker-a", 1_000, 100, 1)
        .unwrap()
        .remove(0);
    let first_fence = fence(&first);
    assert_eq!(
        journal
            .ack_delivery("reverse:r1", &first_fence, 1_050, 100)
            .unwrap(),
        AckOutcome::Acked
    );

    let second = journal
        .claim_delivery(WorkKind::ReverseRequest, "worker-b", 1_151, 100, 1)
        .unwrap()
        .remove(0);
    let second_fence = fence(&second);
    assert_eq!(second.delivery_epoch, first.delivery_epoch + 1);
    assert_eq!(
        journal
            .ack_delivery("reverse:r1", &first_fence, 1_152, 100)
            .unwrap(),
        AckOutcome::Stale
    );
    assert_eq!(
        journal
            .record_result(
                "reverse:r1",
                "result:r1-old",
                &json!({"response": "stale"}),
                &first_fence,
                1_152,
            )
            .unwrap(),
        RecordResultOutcome::Stale
    );
    assert_eq!(
        journal
            .ack_delivery("reverse:r1", &second_fence, 1_152, 100)
            .unwrap(),
        AckOutcome::Acked
    );
}

#[test]
fn keyed_claim_is_idempotent_for_owner_and_reclaims_after_expiry() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);

    let first = journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-a", 1_000, 100)
        .unwrap()
        .unwrap();
    let retry = journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-a", 1_050, 100)
        .unwrap()
        .unwrap();
    assert_eq!(retry.delivery_epoch, first.delivery_epoch);
    assert_eq!(retry.attempts, first.attempts);
    assert!(journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-b", 1_050, 100,)
        .unwrap()
        .is_none());

    let reclaimed = journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-b", 1_101, 100)
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.delivery_epoch, first.delivery_epoch + 1);
    assert_eq!(reclaimed.attempts, first.attempts + 1);
}

#[test]
fn delivery_candidate_snapshot_is_bounded_ordered_and_read_only() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    for key in ["runner:t1", "runner:t2", "runner:t3"] {
        enqueue(&journal, key, WorkKind::RunnerTrigger);
    }
    enqueue(&journal, "reverse:r1", WorkKind::ReverseRequest);

    let leased = journal
        .claim_delivery_by_key(
            "runner:t1",
            WorkKind::RunnerTrigger,
            "crashed-worker",
            1_000,
            100,
        )
        .unwrap()
        .unwrap();
    assert_eq!(leased.state, WorkState::Leased);
    let live = journal
        .claim_delivery_by_key(
            "runner:t2",
            WorkKind::RunnerTrigger,
            "live-worker",
            1_000,
            500,
        )
        .unwrap()
        .unwrap();
    assert_eq!(live.state, WorkState::Leased);

    assert_eq!(
        journal
            .delivery_candidate_keys(WorkKind::RunnerTrigger, 1_101, 2)
            .unwrap(),
        vec!["runner:t1", "runner:t3"],
        "expired delivery and pending row are returned in journal order"
    );
    assert_eq!(
        journal
            .delivery_candidate_keys(WorkKind::RunnerTrigger, 1_101, 1)
            .unwrap(),
        vec!["runner:t1"],
        "snapshot is bounded"
    );
    assert!(journal
        .delivery_candidate_keys(WorkKind::RunnerTrigger, 1_101, 0)
        .unwrap()
        .is_empty());
    assert_eq!(
        journal
            .delivery_candidate_keys(WorkKind::ReverseRequest, 1_101, 8)
            .unwrap(),
        vec!["reverse:r1"],
        "work kinds remain isolated"
    );
    assert_eq!(
        journal.get("runner:t1").unwrap().unwrap().state,
        WorkState::Leased,
        "candidate enumeration grants no lease and changes no state"
    );
    assert_eq!(
        journal.get("runner:t3").unwrap().unwrap().state,
        WorkState::Pending
    );
}

#[test]
fn renew_requires_the_current_live_fence() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);
    let claimed = journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-a", 1_000, 100)
        .unwrap()
        .unwrap();
    let live = fence(&claimed);

    assert_eq!(
        journal
            .renew_delivery("runner:t1", &live, 1_050, 200)
            .unwrap(),
        RenewOutcome::Renewed
    );
    assert_eq!(
        journal
            .renew_delivery(
                "runner:t1",
                &DeliveryFence::new("worker-b", live.epoch),
                1_051,
                200,
            )
            .unwrap(),
        RenewOutcome::Stale
    );
    assert_eq!(
        journal
            .renew_delivery("runner:t1", &live, 1_251, 200)
            .unwrap(),
        RenewOutcome::Stale
    );
}

#[test]
fn result_and_settlement_survive_crashes_and_duplicate_retries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state/memory-journal.sqlite3");
    let delivery_fence;
    {
        let journal = Journal::open(&path).unwrap();
        enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);
        let claimed = journal
            .claim_delivery(WorkKind::RunnerTrigger, "worker-a", 1_000, 500, 1)
            .unwrap()
            .remove(0);
        delivery_fence = fence(&claimed);
        assert_eq!(
            journal
                .record_result(
                    "runner:t1",
                    "completion:t1",
                    &json!({"written_paths": ["memory/a.md"]}),
                    &delivery_fence,
                    1_100,
                )
                .unwrap(),
            RecordResultOutcome::Recorded
        );
        // Simulated crash: no settlement call before dropping the handle.
    }

    let reopened = Journal::open(&path).unwrap();
    assert_eq!(
        reopened
            .record_result(
                "runner:t1",
                "completion:t1",
                &json!({"written_paths": ["memory/a.md"]}),
                &delivery_fence,
                1_101,
            )
            .unwrap(),
        RecordResultOutcome::Duplicate
    );
    let settling = reopened
        .claim_settlement("settler-a", 1_200, 100, 1)
        .unwrap()
        .remove(0);
    let stale_settlement_fence = fence(&settling);
    assert_eq!(settling.state, WorkState::Settling);

    // Crash while settling. The expired claim is recovered with a new epoch.
    drop(reopened);
    let recovered = Journal::open(&path).unwrap();
    let settling_again = recovered
        .claim_settlement("settler-b", 1_301, 100, 1)
        .unwrap()
        .remove(0);
    let live_fence = fence(&settling_again);
    assert!(live_fence.epoch > stale_settlement_fence.epoch);
    assert_eq!(
        recovered
            .mark_settled("runner:t1", &stale_settlement_fence, 1_302)
            .unwrap(),
        SettleOutcome::Stale
    );
    assert_eq!(
        recovered
            .mark_settled("runner:t1", &live_fence, 1_302)
            .unwrap(),
        SettleOutcome::Settled
    );
    assert_eq!(
        recovered
            .mark_settled("runner:t1", &live_fence, 1_303)
            .unwrap(),
        SettleOutcome::AlreadySettled
    );
    assert_eq!(
        recovered.get("runner:t1").unwrap().unwrap().state,
        WorkState::Settled
    );
}

#[test]
fn settlement_candidate_snapshot_lists_ready_and_expired_rows_without_claiming() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    for (key, recorded_at) in [("runner:t1", 1_100), ("runner:t2", 1_200)] {
        enqueue(&journal, key, WorkKind::RunnerTrigger);
        let claimed = journal
            .claim_delivery_by_key(key, WorkKind::RunnerTrigger, "worker-a", 1_000, 500)
            .unwrap()
            .unwrap();
        journal
            .record_result(
                key,
                &format!("completion:{key}"),
                &json!({"key": key}),
                &fence(&claimed),
                recorded_at,
            )
            .unwrap();
    }
    enqueue(&journal, "reverse:r1", WorkKind::ReverseRequest);
    let reverse = journal
        .claim_delivery_by_key(
            "reverse:r1",
            WorkKind::ReverseRequest,
            "worker-reverse",
            1_000,
            500,
        )
        .unwrap()
        .unwrap();
    journal
        .record_result(
            "reverse:r1",
            "completion:reverse:r1",
            &json!({"key": "reverse:r1"}),
            &fence(&reverse),
            1_150,
        )
        .unwrap();

    let settling = journal
        .claim_settlement_by_key("runner:t1", "settler-a", 1_250, 100)
        .unwrap()
        .unwrap();
    assert_eq!(settling.state, WorkState::Settling);
    assert_eq!(
        journal
            .settlement_candidate_keys(WorkKind::RunnerTrigger, 1_300)
            .unwrap(),
        vec!["runner:t2"],
        "a live settlement fence is not recoverable"
    );
    assert_eq!(
        journal
            .settlement_candidate_keys(WorkKind::RunnerTrigger, 1_351)
            .unwrap(),
        vec!["runner:t1", "runner:t2"],
        "expired settlement and result-ready rows preserve result order"
    );
    assert_eq!(
        journal
            .settlement_candidate_keys(WorkKind::ReverseRequest, 1_351)
            .unwrap(),
        vec!["reverse:r1"],
        "settlement recovery is isolated by durable work kind"
    );
    assert_eq!(
        journal.get("runner:t1").unwrap().unwrap().state,
        WorkState::Settling,
        "snapshot is read-only"
    );
    assert_eq!(
        journal.get("runner:t2").unwrap().unwrap().state,
        WorkState::ResultReady
    );
}

#[test]
fn keyed_settlement_claim_is_idempotent_then_fences_expired_owner() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);
    let claimed = journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-a", 1_000, 200)
        .unwrap()
        .unwrap();
    let delivery = fence(&claimed);
    journal
        .record_result(
            "runner:t1",
            "completion:t1",
            &json!({"ok": true}),
            &delivery,
            1_050,
        )
        .unwrap();

    let first = journal
        .claim_settlement_by_key("runner:t1", "settler-a", 1_060, 100)
        .unwrap()
        .unwrap();
    let retry = journal
        .claim_settlement_by_key("runner:t1", "settler-a", 1_100, 100)
        .unwrap()
        .unwrap();
    assert_eq!(retry.delivery_epoch, first.delivery_epoch);
    assert!(journal
        .claim_settlement_by_key("runner:t1", "settler-b", 1_100, 100)
        .unwrap()
        .is_none());

    let reclaimed = journal
        .claim_settlement_by_key("runner:t1", "settler-b", 1_161, 100)
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed.delivery_epoch, first.delivery_epoch + 1);
}

#[test]
fn settlement_renewal_extends_the_live_fence() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);
    let claimed = journal
        .claim_delivery_by_key("runner:t1", WorkKind::RunnerTrigger, "worker-a", 1_000, 100)
        .unwrap()
        .unwrap();
    journal
        .record_result(
            "runner:t1",
            "completion:t1",
            &json!({"ok": true}),
            &fence(&claimed),
            1_050,
        )
        .unwrap();
    let settling = journal
        .claim_settlement_by_key("runner:t1", "settler-a", 1_060, 100)
        .unwrap()
        .unwrap();
    let settlement_fence = fence(&settling);

    assert_eq!(
        journal
            .renew_settlement("runner:t1", &settlement_fence, 1_100, 200)
            .unwrap(),
        RenewOutcome::Renewed
    );
    assert_eq!(
        journal
            .mark_settled("runner:t1", &settlement_fence, 1_201)
            .unwrap(),
        SettleOutcome::Settled
    );
}

#[test]
fn unfenced_result_only_consumes_never_leased_pending_work() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "runner:t1", WorkKind::RunnerTrigger);
    enqueue(&journal, "runner:t2", WorkKind::RunnerTrigger);

    assert_eq!(
        journal
            .record_unfenced_pending_result(
                "runner:t1",
                "completion:t1",
                &json!({"ok": true}),
                1_100,
            )
            .unwrap(),
        RecordResultOutcome::Recorded
    );

    let _claimed = journal
        .claim_delivery(WorkKind::RunnerTrigger, "worker-a", 1_100, 100, 8)
        .unwrap();
    assert_eq!(
        journal
            .record_unfenced_pending_result(
                "runner:t2",
                "completion:t2",
                &json!({"ok": true}),
                1_101,
            )
            .unwrap(),
        RecordResultOutcome::Stale
    );
}

#[test]
fn dead_letter_requires_live_fence_and_preserves_diagnostic_payload() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "reverse:r1", WorkKind::ReverseRequest);
    let claimed = journal
        .claim_delivery(WorkKind::ReverseRequest, "worker-a", 1_000, 100, 1)
        .unwrap()
        .remove(0);
    let live_fence = fence(&claimed);
    let stale_fence = DeliveryFence::new("worker-b", claimed.delivery_epoch);

    assert_eq!(
        journal
            .mark_dead_letter("reverse:r1", &stale_fence, 1_001, "wrong owner")
            .unwrap(),
        DeadLetterOutcome::Stale
    );
    assert_eq!(
        journal
            .mark_dead_letter("reverse:r1", &live_fence, 1_001, "retry budget exhausted")
            .unwrap(),
        DeadLetterOutcome::DeadLettered
    );
    let item = journal.get("reverse:r1").unwrap().unwrap();
    assert_eq!(item.state, WorkState::DeadLetter);
    assert_eq!(item.payload, json!({"request": "reverse:r1"}));
    assert_eq!(item.last_error.as_deref(), Some("retry budget exhausted"));
    assert_eq!(
        journal
            .mark_dead_letter("reverse:r1", &live_fence, 1_002, "duplicate")
            .unwrap(),
        DeadLetterOutcome::AlreadyDeadLettered
    );
}

#[test]
fn two_process_like_handles_cannot_claim_the_same_item() {
    let dir = TempDir::new().unwrap();
    let journal = journal(&dir);
    enqueue(&journal, "reverse:r1", WorkKind::ReverseRequest);

    let barrier = Arc::new(Barrier::new(3));
    let path = journal.path().to_path_buf();
    let mut joins = Vec::new();
    for owner in ["worker-a", "worker-b"] {
        let barrier = Arc::clone(&barrier);
        let path = path.clone();
        joins.push(thread::spawn(move || {
            let journal = Journal::open(path).unwrap();
            barrier.wait();
            journal
                .claim_delivery(WorkKind::ReverseRequest, owner, 1_000, 500, 1)
                .unwrap()
        }));
    }
    barrier.wait();
    let total_claims: usize = joins
        .into_iter()
        .map(|join| join.join().unwrap().len())
        .sum();
    assert_eq!(total_claims, 1);
}

#[cfg(unix)]
#[test]
fn journal_rejects_symlink_target_and_uses_private_file_mode() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real.sqlite3");
    let journal = Journal::open(&real).unwrap();
    assert_eq!(
        std::fs::metadata(journal.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let link = dir.path().join("link.sqlite3");
    symlink(&real, &link).unwrap();
    assert!(matches!(
        Journal::open(&link).unwrap_err(),
        JournalError::SymlinkPath(_)
    ));
}
