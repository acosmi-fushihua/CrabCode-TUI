use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::atomic_write::BoxError;

pub const RUST_DERIVED_DIRNAME: &str = ".memory-rust-derived";
pub const DAILY_LOG_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptMeta {
    pub session_id: String,
    pub path: PathBuf,
    pub mtime_ms: u64,
    pub size_bytes: u64,
    pub sealed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionEvent {
    pub event_id: String,
    pub kind: String,
    pub occurred_at_ms: u64,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyLogAppendReport {
    pub path: PathBuf,
    pub events_written: usize,
    pub bytes_written: usize,
    pub append_fsynced: bool,
}

pub async fn append_daily_log(
    project_state_dir: &Path,
    transcript_meta: &TranscriptMeta,
    events: &[SessionEvent],
) -> Result<DailyLogAppendReport, BoxError> {
    ensure_no_legacy_component(project_state_dir)?;

    if events.is_empty() {
        return Ok(DailyLogAppendReport {
            path: daily_log_path(project_state_dir, transcript_meta.mtime_ms),
            events_written: 0,
            bytes_written: 0,
            append_fsynced: false,
        });
    }

    let path = daily_log_path(project_state_dir, events[0].occurred_at_ms);
    ensure_no_legacy_component(&path)?;

    let existing = match tokio::fs::read(&path).await {
        Ok(existing) => existing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let existing_event_ids = String::from_utf8_lossy(&existing)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            value
                .get("event_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let new_events = events
        .iter()
        .filter(|event| !existing_event_ids.contains(&event.event_id))
        .collect::<Vec<_>>();
    if new_events.is_empty() {
        return Ok(DailyLogAppendReport {
            path,
            events_written: 0,
            bytes_written: 0,
            append_fsynced: false,
        });
    }

    let mut bytes = Vec::new();
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        bytes.push(b'\n');
    }

    for event in &new_events {
        let record = audit_record(transcript_meta, event);
        bytes.extend(serde_json::to_vec(&record)?);
        bytes.push(b'\n');
    }

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "daily log target has no parent directory: {}",
                path.display()
            ),
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;

    Ok(DailyLogAppendReport {
        path,
        events_written: new_events.len(),
        bytes_written: bytes.len(),
        append_fsynced: true,
    })
}

pub fn rust_derived_root(project_state_dir: &Path) -> PathBuf {
    project_state_dir.join(RUST_DERIVED_DIRNAME)
}

pub fn daily_log_path(project_state_dir: &Path, unix_ms: u64) -> PathBuf {
    let (year, month, day) = utc_ymd_from_unix_ms(unix_ms);
    let date = format!("{year:04}-{month:02}-{day:02}");

    rust_derived_root(project_state_dir)
        .join("logs")
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(format!("{date}.jsonl"))
}

fn audit_record(transcript_meta: &TranscriptMeta, event: &SessionEvent) -> Value {
    json!({
        "schema_version": DAILY_LOG_SCHEMA_VERSION,
        "event_id": event.event_id,
        "kind": event.kind,
        "occurred_at_ms": event.occurred_at_ms,
        "source": {
            "transcript": {
                "session_id": transcript_meta.session_id,
                "path": transcript_meta.path.to_string_lossy(),
                "mtime_ms": transcript_meta.mtime_ms,
                "size_bytes": transcript_meta.size_bytes,
                "sealed": transcript_meta.sealed,
            },
        },
        "payload": event.payload,
    })
}

fn ensure_no_legacy_component(path: &Path) -> Result<(), BoxError> {
    if path
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name == OsStr::new(".rust-derived")))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to write Rust-derived data under legacy path: {}", path.display()),
        )
        .into());
    }

    Ok(())
}

pub(crate) fn utc_ymd_from_unix_ms(unix_ms: u64) -> (i32, u32, u32) {
    let days = Duration::from_millis(unix_ms).as_secs() as i64 / 86_400;
    civil_from_days(days)
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };

    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    fn meta(dir: &Path) -> TranscriptMeta {
        TranscriptMeta {
            session_id: "session-a".to_owned(),
            path: dir.join("transcripts").join("session-a.jsonl"),
            mtime_ms: 0,
            size_bytes: 42,
            sealed: true,
        }
    }

    fn event(id: &str, kind: &str, occurred_at_ms: u64) -> SessionEvent {
        SessionEvent {
            event_id: id.to_owned(),
            kind: kind.to_owned(),
            occurred_at_ms,
            payload: json!({ "ok": true }),
        }
    }

    #[tokio::test]
    async fn daily_log_writes_jsonl_to_sibling_root() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        let report = append_daily_log(dir.path(), &meta(dir.path()), &[event("e1", "turn_end", 0)])
            .await
            .unwrap();

        assert_eq!(
            report.path,
            dir.path()
                .join(".memory-rust-derived/logs/1970/01/1970-01-01.jsonl")
        );
        assert!(report.path.exists());
        assert!(!memory_dir.join("logs/1970/01/1970-01-01.md").exists());
        assert!(!memory_dir.join(".rust-derived/logs").exists());
        assert!(report.append_fsynced);

        let lines = fs::read_to_string(&report.path).unwrap();
        let record: Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(record["schema_version"], DAILY_LOG_SCHEMA_VERSION);
        assert_eq!(record["event_id"], "e1");
        assert_eq!(record["source"]["transcript"]["session_id"], "session-a");
        assert_eq!(record["payload"]["ok"], true);
    }

    #[tokio::test]
    async fn daily_log_appends_with_fsync_and_leaves_no_tmp_files() {
        let dir = TempDir::new().unwrap();
        let transcript_meta = meta(dir.path());

        append_daily_log(dir.path(), &transcript_meta, &[event("e1", "first", 0)])
            .await
            .unwrap();
        let report = append_daily_log(dir.path(), &transcript_meta, &[event("e2", "second", 1)])
            .await
            .unwrap();

        let body = fs::read_to_string(&report.path).unwrap();
        let lines = body.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""event_id":"e1""#));
        assert!(lines[1].contains(r#""event_id":"e2""#));

        let leftovers = fs::read_dir(report.path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[tokio::test]
    async fn daily_log_deduplicates_retried_event_id() {
        let dir = TempDir::new().unwrap();
        let transcript_meta = meta(dir.path());
        let first = append_daily_log(
            dir.path(),
            &transcript_meta,
            &[event("runner-1:completed", "memory.runner.completed", 0)],
        )
        .await
        .unwrap();
        let retry = append_daily_log(
            dir.path(),
            &transcript_meta,
            &[event("runner-1:completed", "memory.runner.completed", 0)],
        )
        .await
        .unwrap();

        let body = fs::read_to_string(first.path).unwrap();
        assert_eq!(body.lines().count(), 1);
        assert_eq!(retry.events_written, 0);
        assert!(!retry.append_fsynced);
    }

    #[tokio::test]
    async fn daily_log_rejects_legacy_rust_derived_project_state_dir() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join("memory").join(".rust-derived");

        let err = append_daily_log(&legacy, &meta(dir.path()), &[event("e1", "turn_end", 0)])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("legacy path"));
        assert!(!legacy.join("logs").exists());
    }

    #[tokio::test]
    async fn daily_log_path_uses_utc_calendar_day() {
        assert_eq!(utc_ymd_from_unix_ms(0), (1970, 1, 1));
        assert_eq!(
            daily_log_path(Path::new("/state"), 0),
            Path::new("/state/.memory-rust-derived/logs/1970/01/1970-01-01.jsonl")
        );
    }
}
