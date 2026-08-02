use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::daily_log::rust_derived_root;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub const ARCHIVE_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct RunnerArchiveRecord {
    pub trigger_id: String,
    pub kind: String,
    pub completed_at_ms: u64,
    pub written_paths: Vec<PathBuf>,
    pub usage: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenPathIndexRecord {
    pub trigger_id: String,
    pub kind: String,
    pub path: PathBuf,
    pub exists: bool,
    pub mtime_ms: Option<u64>,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractArchiveReport {
    pub archive_path: PathBuf,
    pub index_path: PathBuf,
    pub written_path_records: Vec<WrittenPathIndexRecord>,
    pub archive_fsynced: bool,
    pub index_fsynced: bool,
}

pub async fn archive_runner_completed(
    project_state_dir: &Path,
    record: &RunnerArchiveRecord,
) -> Result<ExtractArchiveReport, BoxError> {
    let archive_path = runner_archive_path(project_state_dir);
    let index_path = written_paths_index_path(project_state_dir);
    ensure_not_legacy_path(&archive_path)?;
    ensure_not_legacy_path(&index_path)?;

    let existing_archive_ids = existing_jsonl_values(&archive_path)
        .await?
        .into_iter()
        .filter_map(|value| {
            value
                .get("trigger_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let archive_values = (!existing_archive_ids.contains(&record.trigger_id))
        .then(|| runner_record_json(record))
        .into_iter()
        .collect::<Vec<_>>();
    let archive_fsynced = append_jsonl(&archive_path, &archive_values).await?;
    let written_path_records = reindex_written_paths(record);
    let existing_index_keys = existing_jsonl_values(&index_path)
        .await?
        .into_iter()
        .filter_map(|value| {
            let trigger_id = value.get("trigger_id")?.as_str()?;
            let path = value.get("path")?.as_str()?;
            Some(format!("{trigger_id}\0{path}"))
        })
        .collect::<BTreeSet<_>>();
    let index_values = written_path_records
        .iter()
        .filter(|record| {
            !existing_index_keys.contains(&format!(
                "{}\0{}",
                record.trigger_id,
                record.path.to_string_lossy()
            ))
        })
        .map(written_path_record_json)
        .collect::<Vec<_>>();
    let index_fsynced = append_jsonl(&index_path, &index_values).await?;

    Ok(ExtractArchiveReport {
        archive_path,
        index_path,
        written_path_records,
        archive_fsynced,
        index_fsynced,
    })
}

async fn existing_jsonl_values(path: &Path) -> Result<Vec<Value>, io::Error> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect())
}

pub fn runner_archive_path(project_state_dir: &Path) -> PathBuf {
    rust_derived_root(project_state_dir)
        .join("archives")
        .join("runner-completed.jsonl")
}

pub fn written_paths_index_path(project_state_dir: &Path) -> PathBuf {
    rust_derived_root(project_state_dir)
        .join("indexes")
        .join("written-paths.jsonl")
}

/// W-MEMORY-SELF-EVOLUTION W-C (2026-06-11) — read the persistent archive
/// ledger newest-first, capped at `limit`. Tolerant line-by-line parse: a
/// corrupt JSONL line is skipped, never fails the whole read. Missing file =
/// empty Vec (project simply has no archive history yet).
pub async fn read_recent_archive_records(project_state_dir: &Path, limit: usize) -> Vec<Value> {
    let path = runner_archive_path(project_state_dir);
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let mut records: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    records.reverse(); // append-only file → reverse = newest first
    records.truncate(limit);
    records
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn reindex_written_paths(record: &RunnerArchiveRecord) -> Vec<WrittenPathIndexRecord> {
    record
        .written_paths
        .iter()
        .map(|path| {
            let metadata = fs::metadata(path);
            match metadata {
                Ok(metadata) if metadata.is_file() => WrittenPathIndexRecord {
                    trigger_id: record.trigger_id.clone(),
                    kind: record.kind.clone(),
                    path: path.clone(),
                    exists: true,
                    mtime_ms: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_millis() as u64),
                    size_bytes: Some(metadata.len()),
                },
                _ => WrittenPathIndexRecord {
                    trigger_id: record.trigger_id.clone(),
                    kind: record.kind.clone(),
                    path: path.clone(),
                    exists: false,
                    mtime_ms: None,
                    size_bytes: None,
                },
            }
        })
        .collect()
}

fn runner_record_json(record: &RunnerArchiveRecord) -> Value {
    json!({
        "schema_version": ARCHIVE_SCHEMA_VERSION,
        "trigger_id": record.trigger_id,
        "kind": record.kind,
        "completed_at_ms": record.completed_at_ms,
        "written_paths": record
            .written_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        "usage": record.usage,
        "error": record.error,
    })
}

fn written_path_record_json(record: &WrittenPathIndexRecord) -> Value {
    json!({
        "schema_version": ARCHIVE_SCHEMA_VERSION,
        "trigger_id": record.trigger_id,
        "kind": record.kind,
        "path": record.path.to_string_lossy(),
        "exists": record.exists,
        "mtime_ms": record.mtime_ms,
        "size_bytes": record.size_bytes,
    })
}

async fn append_jsonl(path: &Path, values: &[Value]) -> Result<bool, BoxError> {
    if values.is_empty() {
        return Ok(false);
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("archive target has no parent: {}", path.display()),
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;

    let mut bytes = Vec::new();
    for value in values {
        bytes.extend(serde_json::to_vec(value)?);
        bytes.push(b'\n');
    }

    rotate_if_oversized(path).await;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(true)
}

/// 2026-07-27 §22.2-1 —— 单个 JSONL 归档的尺寸上限。
///
/// `runner-completed.jsonl` / `written-paths.jsonl` 是纯 append 的归档，此前
/// 没有任何轮转：只增不减。超限即滚动为 `<name>.1`（**只保留一代**）——
/// 派生层数据，丢一代不影响任何真源。
pub const ARCHIVE_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// 超限滚动。**fail-soft 到"不滚动"而不是"不写入"**：宁可文件偏大，
/// 也绝不能因为一个回收动作把一条真实回执丢掉。
async fn rotate_if_oversized(path: &Path) {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return;
    };
    if meta.len() < ARCHIVE_ROTATE_BYTES {
        return;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    let rotated = PathBuf::from(rotated);
    // Windows 的 rename 不覆盖已存在目标，先摘掉上一代。
    let _ = tokio::fs::remove_file(&rotated).await;
    match tokio::fs::rename(path, &rotated).await {
        Ok(()) => log::info!(
            "[archive] rotated oversized jsonl: {} → {}",
            path.display(),
            rotated.display()
        ),
        Err(e) => log::warn!(
            "[archive] rotate failed (fail-soft, continuing to append) {}: {e}",
            path.display()
        ),
    }
}

fn ensure_not_legacy_path(path: &Path) -> Result<(), BoxError> {
    if path
        .components()
        .any(|component| component.as_os_str() == ".rust-derived")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to archive under legacy Rust-derived path: {}",
                path.display()
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    fn record(_dir: &TempDir, written_paths: Vec<PathBuf>) -> RunnerArchiveRecord {
        RunnerArchiveRecord {
            trigger_id: "extract-1".to_owned(),
            kind: "extract".to_owned(),
            completed_at_ms: 1_700_000_000_000,
            written_paths,
            usage: Some(json!({ "output_tokens": 11 })),
            error: None,
        }
    }

    #[tokio::test]
    async fn extract_archive_records_runner_completion_and_reindexes_written_paths() {
        let dir = TempDir::new().unwrap();
        let memory_file = dir.path().join("memory/topic.md");
        fs::create_dir_all(memory_file.parent().unwrap()).unwrap();
        fs::write(&memory_file, "---\ntype: project\n---\nbody").unwrap();

        let report = archive_runner_completed(dir.path(), &record(&dir, vec![memory_file.clone()]))
            .await
            .unwrap();

        assert!(report.archive_path.ends_with("runner-completed.jsonl"));
        assert!(report.index_path.ends_with("written-paths.jsonl"));
        assert!(report.archive_fsynced);
        assert!(report.index_fsynced);
        assert_eq!(report.written_path_records.len(), 1);
        assert!(report.written_path_records[0].exists);
        assert_eq!(report.written_path_records[0].path, memory_file);

        let archive_body = fs::read_to_string(report.archive_path).unwrap();
        let archive: Value = serde_json::from_str(archive_body.lines().next().unwrap()).unwrap();
        assert_eq!(archive["trigger_id"], "extract-1");
        assert_eq!(archive["usage"]["output_tokens"], 11);
    }

    #[tokio::test]
    async fn extract_archive_keeps_missing_written_paths_as_index_tombstones() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("memory/missing.md");

        let report = archive_runner_completed(dir.path(), &record(&dir, vec![missing.clone()]))
            .await
            .unwrap();

        assert_eq!(
            report.written_path_records,
            vec![WrittenPathIndexRecord {
                trigger_id: "extract-1".to_owned(),
                kind: "extract".to_owned(),
                path: missing,
                exists: false,
                mtime_ms: None,
                size_bytes: None,
            }]
        );
    }

    #[tokio::test]
    async fn extract_archive_deduplicates_retried_completion_by_trigger_id() {
        let dir = TempDir::new().unwrap();

        let first = archive_runner_completed(dir.path(), &record(&dir, Vec::new()))
            .await
            .unwrap();
        let retry = archive_runner_completed(dir.path(), &record(&dir, Vec::new()))
            .await
            .unwrap();

        let body = fs::read_to_string(runner_archive_path(dir.path())).unwrap();
        assert_eq!(body.lines().count(), 1);
        assert!(first.archive_fsynced);
        assert!(!retry.archive_fsynced);
    }

    #[tokio::test]
    async fn extract_archive_appends_distinct_completion_records() {
        let dir = TempDir::new().unwrap();
        let first = record(&dir, Vec::new());
        let mut second = record(&dir, Vec::new());
        second.trigger_id = "extract-2".to_owned();

        archive_runner_completed(dir.path(), &first).await.unwrap();
        archive_runner_completed(dir.path(), &second).await.unwrap();

        let body = fs::read_to_string(runner_archive_path(dir.path())).unwrap();
        assert_eq!(body.lines().count(), 2);
    }

    #[tokio::test]
    async fn extract_archive_uses_sibling_derived_root_not_memory_legacy_root() {
        let dir = TempDir::new().unwrap();

        let report = archive_runner_completed(dir.path(), &record(&dir, Vec::new()))
            .await
            .unwrap();

        assert!(report
            .archive_path
            .starts_with(dir.path().join(".memory-rust-derived")));
        assert!(!dir.path().join("memory/.rust-derived").exists());
    }

    #[tokio::test]
    async fn extract_archive_includes_error_payload_for_failed_runner() {
        let dir = TempDir::new().unwrap();
        let mut failed = record(&dir, Vec::new());
        failed.error = Some(json!({ "message": "fork failed" }));

        let report = archive_runner_completed(dir.path(), &failed).await.unwrap();

        let body = fs::read_to_string(report.archive_path).unwrap();
        let archive: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(archive["error"]["message"], "fork failed");
    }

    #[tokio::test]
    async fn extract_archive_no_written_paths_skips_index_append() {
        let dir = TempDir::new().unwrap();

        let report = archive_runner_completed(dir.path(), &record(&dir, Vec::new()))
            .await
            .unwrap();

        assert!(!report.index_fsynced);
        assert!(!report.index_path.exists());
    }
}
