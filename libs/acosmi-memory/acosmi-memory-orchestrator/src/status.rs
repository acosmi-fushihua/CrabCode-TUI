use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::daily_log::{daily_log_path, rust_derived_root};
use crate::dedup_hash::{find_dedup_groups, scan_body_hashes};
use crate::lock::{is_process_running, lock_path, DEFAULT_HOLDER_STALE_MS};
use crate::memory_md_analyze::analyze_memory_md;
use crate::stale_detector::{detect_stale_at, StaleReason};
use crate::transcript_index::{scan_transcript_dir, TASK_TYPE_AGENT, TASK_TYPE_MAIN_SESSION};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub const DEFAULT_STALE_DAYS: u32 = 90;
pub const MEMORY_STALE_DAYS_ENV: &str = "CRABCODE_MEMORY_STALE_DAYS";
pub const LEGACY_RUST_DERIVED_DIRNAME: &str = ".rust-derived";

#[derive(Clone, Debug)]
pub struct StatusRequest {
    pub memory_dir: PathBuf,
    pub cwd: PathBuf,
    pub project_state_dir: PathBuf,
    pub transcript_dir: PathBuf,
    pub stale_days: u32,
    pub now: SystemTime,
}

impl StatusRequest {
    #[must_use]
    pub fn new(
        memory_dir: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        project_state_dir: impl Into<PathBuf>,
        transcript_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            memory_dir: memory_dir.into(),
            cwd: cwd.into(),
            project_state_dir: project_state_dir.into(),
            transcript_dir: transcript_dir.into(),
            stale_days: stale_days_from_env(),
            now: SystemTime::now(),
        }
    }

    #[must_use]
    pub fn with_stale_days(mut self, stale_days: u32) -> Self {
        self.stale_days = stale_days;
        self
    }

    #[must_use]
    pub fn with_now(mut self, now: SystemTime) -> Self {
        self.now = now;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MemoryOrchestratorStatus {
    pub generated_at_ms: u64,
    pub paths: StatusPaths,
    pub dedup: DedupSummary,
    pub stale: StaleSummary,
    pub memory_md: MemoryMdSummary,
    pub daily_log: DailyLogStatus,
    pub transcript_index: TranscriptIndexSummary,
    pub lock: LockStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusPaths {
    pub memory_dir: PathBuf,
    pub cwd: PathBuf,
    pub project_state_dir: PathBuf,
    pub rust_derived_root: PathBuf,
    pub legacy_rust_derived_root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DedupSummary {
    pub scanned_files: usize,
    pub duplicate_group_count: usize,
    pub duplicate_file_count: usize,
    pub duplicate_body_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StaleSummary {
    pub scanned_with_findings: usize,
    pub stale_file_count: usize,
    pub reason_counts: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MemoryMdSummary {
    pub path: PathBuf,
    pub exists: bool,
    pub line_count: usize,
    pub byte_size: u64,
    pub overflow_ratio: f32,
    pub link_count: usize,
    pub long_entry_count: usize,
    pub dangling_ref_count: usize,
    pub missing_index_count: usize,
    pub duplicate_target_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DailyLogStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub parent_exists: bool,
    pub size_bytes: u64,
    pub line_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TranscriptIndexSummary {
    pub path: PathBuf,
    pub transcript_count: usize,
    pub main_session_count: usize,
    pub agent_task_count: usize,
    pub unknown_task_count: usize,
    pub total_size_bytes: u64,
    pub latest_mtime_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LockStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub last_consolidated_at_ms: u64,
    pub holder_pid: Option<u32>,
    pub holder_running: Option<bool>,
    pub stale_by_mtime: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupListReport {
    pub groups: Vec<DedupGroupEntry>,
    pub duplicate_group_count: usize,
    pub duplicate_file_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupGroupEntry {
    pub hash_hex: String,
    pub primary: DedupFileEntry,
    pub duplicates: Vec<DedupFileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DedupFileEntry {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub body_bytes: u64,
    pub mtime_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryOrchestratorHealth {
    pub generated_at_ms: u64,
    pub ok: bool,
    pub path_writability: Vec<PathWritability>,
    pub derived_root: DerivedRootHealth,
    pub legacy_rust_derived: LegacyRustDerivedHealth,
    pub recent_analysis: RecentAnalysisSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathWritability {
    pub label: String,
    pub path: PathBuf,
    pub exists: bool,
    pub nearest_existing_ancestor: Option<PathBuf>,
    pub writable_by_metadata: bool,
    pub check: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedRootHealth {
    pub path: PathBuf,
    pub expected_parent: Option<PathBuf>,
    pub actual_parent: Option<PathBuf>,
    pub is_sibling_of_memory_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRustDerivedHealth {
    pub path: PathBuf,
    pub exists: bool,
    pub recognized: bool,
    pub skipped_by_scanners: bool,
    pub new_writes_allowed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentAnalysisSummary {
    pub analyzed_at_ms: u64,
    pub dedup_group_count: usize,
    pub dedup_duplicate_file_count: usize,
    pub stale_file_count: usize,
    pub memory_md_issue_count: usize,
    pub daily_log_exists: bool,
    pub transcript_count: usize,
    pub lock_exists: bool,
}

pub fn build_status(request: &StatusRequest) -> Result<MemoryOrchestratorStatus, BoxError> {
    let generated_at_ms = system_time_to_unix_ms(request.now)?;
    let rust_derived = rust_derived_root(&request.project_state_dir);
    let legacy_rust_derived = request.memory_dir.join(LEGACY_RUST_DERIVED_DIRNAME);

    let body_records = scan_body_hashes(&request.memory_dir)?;
    let dedup_list = list_dedup_groups(&request.memory_dir)?;
    let duplicate_body_bytes = dedup_list
        .groups
        .iter()
        .flat_map(|group| group.duplicates.iter())
        .map(|entry| entry.body_bytes)
        .sum();

    let stale_reports = detect_stale_at(
        &request.memory_dir,
        &request.cwd,
        request.stale_days,
        request.now,
    )?;
    let mut reason_counts = BTreeMap::new();
    for report in &stale_reports {
        for reason in &report.reasons {
            *reason_counts.entry(stale_reason_key(reason)).or_insert(0) += 1;
        }
    }

    let memory_md_report = analyze_memory_md(&request.memory_dir)?;
    let daily_log = daily_log_status(&request.project_state_dir, generated_at_ms)?;
    let transcript_index = transcript_index_summary(&request.transcript_dir)?;
    let lock = lock_status(&request.memory_dir, generated_at_ms)?;

    Ok(MemoryOrchestratorStatus {
        generated_at_ms,
        paths: StatusPaths {
            memory_dir: request.memory_dir.clone(),
            cwd: request.cwd.clone(),
            project_state_dir: request.project_state_dir.clone(),
            rust_derived_root: rust_derived,
            legacy_rust_derived_root: legacy_rust_derived,
        },
        dedup: DedupSummary {
            scanned_files: body_records.len(),
            duplicate_group_count: dedup_list.duplicate_group_count,
            duplicate_file_count: dedup_list.duplicate_file_count,
            duplicate_body_bytes,
        },
        stale: StaleSummary {
            scanned_with_findings: stale_reports.len(),
            stale_file_count: stale_reports.iter().filter(|report| report.stale).count(),
            reason_counts,
        },
        memory_md: MemoryMdSummary {
            path: memory_md_report.path,
            exists: memory_md_report.exists,
            line_count: memory_md_report.line_count,
            byte_size: memory_md_report.byte_size,
            overflow_ratio: memory_md_report.overflow_ratio,
            link_count: memory_md_report.links.len(),
            long_entry_count: memory_md_report.long_entries.len(),
            dangling_ref_count: memory_md_report.dangling_refs.len(),
            missing_index_count: memory_md_report.missing_index.len(),
            duplicate_target_count: memory_md_report.duplicates.len(),
        },
        daily_log,
        transcript_index,
        lock,
    })
}

pub fn build_health(request: &StatusRequest) -> Result<MemoryOrchestratorHealth, BoxError> {
    let status = build_status(request)?;
    let derived_root = derived_root_health(&request.project_state_dir, &request.memory_dir);
    let legacy_rust_derived = legacy_rust_derived_health(&request.memory_dir);
    let path_writability = vec![
        path_writability("memory_dir", &request.memory_dir),
        path_writability("project_state_dir", &request.project_state_dir),
        path_writability("rust_derived_root", &status.paths.rust_derived_root),
        path_writability("transcript_dir", &request.transcript_dir),
    ];
    let paths_ok = path_writability
        .iter()
        .all(|path| path.writable_by_metadata);
    let recent_analysis = recent_analysis_summary(&status);

    Ok(MemoryOrchestratorHealth {
        generated_at_ms: status.generated_at_ms,
        ok: paths_ok && derived_root.is_sibling_of_memory_dir,
        path_writability,
        derived_root,
        legacy_rust_derived,
        recent_analysis,
    })
}

pub fn list_dedup_groups(memory_dir: &Path) -> Result<DedupListReport, BoxError> {
    let groups = find_dedup_groups(memory_dir)?
        .into_iter()
        .map(|group| DedupGroupEntry {
            hash_hex: group.hash_hex,
            primary: DedupFileEntry {
                path: group.primary.path,
                relative_path: group.primary.relative_path,
                body_bytes: group.primary.body_bytes,
                mtime_ms: group.primary.mtime_ms,
            },
            duplicates: group
                .duplicates
                .into_iter()
                .map(|duplicate| DedupFileEntry {
                    path: duplicate.path,
                    relative_path: duplicate.relative_path,
                    body_bytes: duplicate.body_bytes,
                    mtime_ms: duplicate.mtime_ms,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let duplicate_file_count = groups
        .iter()
        .map(|group| group.duplicates.len())
        .sum::<usize>();

    Ok(DedupListReport {
        duplicate_group_count: groups.len(),
        duplicate_file_count,
        groups,
    })
}

fn daily_log_status(
    project_state_dir: &Path,
    generated_at_ms: u64,
) -> Result<DailyLogStatus, BoxError> {
    let path = daily_log_path(project_state_dir, generated_at_ms);
    let parent_exists = path.parent().is_some_and(Path::exists);
    match fs::metadata(&path) {
        Ok(metadata) => {
            let body = fs::read_to_string(&path)?;
            Ok(DailyLogStatus {
                path,
                exists: true,
                parent_exists,
                size_bytes: metadata.len(),
                line_count: body.lines().count(),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DailyLogStatus {
            path,
            exists: false,
            parent_exists,
            size_bytes: 0,
            line_count: 0,
        }),
        Err(e) => Err(e.into()),
    }
}

fn transcript_index_summary(transcript_dir: &Path) -> Result<TranscriptIndexSummary, BoxError> {
    let metas = scan_transcript_dir(transcript_dir)?;
    let mut main_session_count = 0;
    let mut agent_task_count = 0;
    let mut unknown_task_count = 0;
    let mut total_size_bytes = 0;
    let mut latest_mtime_ms: Option<u64> = None;

    for meta in &metas {
        match meta.task_type.as_deref() {
            Some(TASK_TYPE_MAIN_SESSION) => main_session_count += 1,
            Some(TASK_TYPE_AGENT) => agent_task_count += 1,
            _ => unknown_task_count += 1,
        }
        total_size_bytes += meta.size_bytes;
        latest_mtime_ms =
            Some(latest_mtime_ms.map_or(meta.mtime_ms, |latest| latest.max(meta.mtime_ms)));
    }

    Ok(TranscriptIndexSummary {
        path: transcript_dir.to_path_buf(),
        transcript_count: metas.len(),
        main_session_count,
        agent_task_count,
        unknown_task_count,
        total_size_bytes,
        latest_mtime_ms,
    })
}

fn lock_status(memory_dir: &Path, generated_at_ms: u64) -> Result<LockStatus, BoxError> {
    let path = lock_path(memory_dir);
    match fs::metadata(&path) {
        Ok(metadata) => {
            let last_consolidated_at_ms = system_time_to_unix_ms(metadata.modified()?)?;
            let holder_pid = fs::read_to_string(&path)
                .ok()
                .and_then(|body| body.trim().parse::<u32>().ok());
            let holder_running = holder_pid.map(is_process_running);
            Ok(LockStatus {
                path,
                exists: true,
                last_consolidated_at_ms,
                holder_pid,
                holder_running,
                stale_by_mtime: Some(
                    generated_at_ms.saturating_sub(last_consolidated_at_ms)
                        >= DEFAULT_HOLDER_STALE_MS,
                ),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(LockStatus {
            path,
            exists: false,
            last_consolidated_at_ms: 0,
            holder_pid: None,
            holder_running: None,
            stale_by_mtime: None,
        }),
        Err(e) => Err(e.into()),
    }
}

fn derived_root_health(project_state_dir: &Path, memory_dir: &Path) -> DerivedRootHealth {
    let path = rust_derived_root(project_state_dir);
    let expected_parent = memory_dir.parent().map(Path::to_path_buf);
    let actual_parent = path.parent().map(Path::to_path_buf);
    let is_sibling_of_memory_dir = expected_parent.is_some() && expected_parent == actual_parent;

    DerivedRootHealth {
        path,
        expected_parent,
        actual_parent,
        is_sibling_of_memory_dir,
    }
}

fn legacy_rust_derived_health(memory_dir: &Path) -> LegacyRustDerivedHealth {
    let path = memory_dir.join(LEGACY_RUST_DERIVED_DIRNAME);
    LegacyRustDerivedHealth {
        exists: path.exists(),
        path,
        recognized: true,
        skipped_by_scanners: true,
        new_writes_allowed: false,
    }
}

fn path_writability(label: &str, path: &Path) -> PathWritability {
    let nearest_existing_ancestor = nearest_existing_ancestor(path);
    let writable_by_metadata = nearest_existing_ancestor
        .as_deref()
        .and_then(|ancestor| fs::metadata(ancestor).ok())
        .is_some_and(|metadata| !metadata.permissions().readonly());

    PathWritability {
        label: label.to_owned(),
        path: path.to_path_buf(),
        exists: path.exists(),
        nearest_existing_ancestor,
        writable_by_metadata,
        check: "nearest_existing_ancestor_metadata_readonly",
    }
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return Some(path.to_path_buf());
    }

    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.exists())
        .map(Path::to_path_buf)
}

fn recent_analysis_summary(status: &MemoryOrchestratorStatus) -> RecentAnalysisSummary {
    RecentAnalysisSummary {
        analyzed_at_ms: status.generated_at_ms,
        dedup_group_count: status.dedup.duplicate_group_count,
        dedup_duplicate_file_count: status.dedup.duplicate_file_count,
        stale_file_count: status.stale.stale_file_count,
        memory_md_issue_count: status.memory_md.long_entry_count
            + status.memory_md.dangling_ref_count
            + status.memory_md.missing_index_count
            + status.memory_md.duplicate_target_count,
        daily_log_exists: status.daily_log.exists,
        transcript_count: status.transcript_index.transcript_count,
        lock_exists: status.lock.exists,
    }
}

fn stale_reason_key(reason: &StaleReason) -> String {
    match reason {
        StaleReason::OldMtime { .. } => "old_mtime".to_owned(),
        StaleReason::DanglingRef { target } => target
            .split_once(':')
            .map(|(kind, _)| format!("dangling_{kind}"))
            .unwrap_or_else(|| "dangling_ref".to_owned()),
    }
}

fn stale_days_from_env() -> u32 {
    std::env::var(MEMORY_STALE_DAYS_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(DEFAULT_STALE_DAYS)
}

fn system_time_to_unix_ms(time: SystemTime) -> Result<u64, BoxError> {
    Ok(time.duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn memory_doc(description: &str, body: &str) -> String {
        format!("---\ntype: project\ndescription: {description}\n---\n{body}")
    }

    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    fn request(dir: &TempDir) -> StatusRequest {
        StatusRequest::new(
            dir.path().join("memory"),
            dir.path().join("workspace"),
            dir.path(),
            dir.path().join("transcripts"),
        )
        .with_stale_days(90)
        .with_now(fixed_now())
    }

    #[test]
    fn status_summarizes_phase1_5_deterministic_modules() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        let cwd = dir.path().join("workspace");
        let transcripts = dir.path().join("transcripts");
        fs::create_dir_all(&cwd).unwrap();

        let older = memory_dir.join("older.md");
        let newer = memory_dir.join("newer.md");
        write_file(&older, &memory_doc("older", "shared body\n"));
        write_file(&newer, &memory_doc("newer", "shared body\n"));
        set_file_mtime(&older, FileTime::from_unix_time(1_999_000_000, 0)).unwrap();
        set_file_mtime(&newer, FileTime::from_unix_time(1_999_000_100, 0)).unwrap();

        let stale = memory_dir.join("stale.md");
        write_file(&stale, &memory_doc("stale", "See path: src/missing.rs\n"));
        set_file_mtime(&stale, FileTime::from_unix_time(1_990_000_000, 0)).unwrap();

        write_file(
            &memory_dir.join("MEMORY.md"),
            "- [Older](older.md)\n- [Missing](missing.md)\n",
        );
        let daily_path = daily_log_path(dir.path(), system_time_to_unix_ms(fixed_now()).unwrap());
        write_file(&daily_path, "{\"event_id\":\"e1\"}\n");
        write_file(&transcripts.join(format!("{SESSION_ID}.jsonl")), "{}\n");
        write_file(&lock_path(&memory_dir), "999999");

        let status = build_status(&request(&dir)).unwrap();

        assert_eq!(status.dedup.scanned_files, 3);
        assert_eq!(status.dedup.duplicate_group_count, 1);
        assert_eq!(status.dedup.duplicate_file_count, 1);
        assert_eq!(status.stale.stale_file_count, 1);
        assert_eq!(status.stale.reason_counts.get("dangling_path"), Some(&1));
        assert!(status.memory_md.exists);
        assert_eq!(status.memory_md.dangling_ref_count, 1);
        assert!(status.daily_log.exists);
        assert_eq!(status.daily_log.line_count, 1);
        assert_eq!(status.transcript_index.transcript_count, 1);
        assert!(status.lock.exists);
        assert_eq!(status.lock.holder_pid, Some(999_999));
    }

    #[test]
    fn health_reports_writability_sibling_derived_root_and_legacy_skip() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory/.rust-derived")).unwrap();
        fs::create_dir_all(dir.path().join("transcripts")).unwrap();

        let health = build_health(&request(&dir)).unwrap();

        assert!(health.ok);
        assert!(health
            .path_writability
            .iter()
            .all(|entry| entry.writable_by_metadata));
        assert!(health.derived_root.is_sibling_of_memory_dir);
        assert!(health.legacy_rust_derived.exists);
        assert!(health.legacy_rust_derived.recognized);
        assert!(health.legacy_rust_derived.skipped_by_scanners);
        assert!(!health.legacy_rust_derived.new_writes_allowed);
        assert_eq!(health.recent_analysis.dedup_group_count, 0);
    }

    #[test]
    fn dedup_list_reports_groups_without_modifying_files() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        let primary = memory_dir.join("a.md");
        let duplicate = memory_dir.join("b.md");
        write_file(&primary, &memory_doc("a", "same body\n"));
        write_file(&duplicate, &memory_doc("b", "same body\n"));
        set_file_mtime(&primary, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        set_file_mtime(&duplicate, FileTime::from_unix_time(1_700_000_100, 0)).unwrap();
        let before = fs::read_to_string(&duplicate).unwrap();

        let report = list_dedup_groups(&memory_dir).unwrap();

        assert_eq!(report.duplicate_group_count, 1);
        assert_eq!(report.duplicate_file_count, 1);
        assert_eq!(
            report.groups[0].primary.relative_path,
            PathBuf::from("a.md")
        );
        assert_eq!(
            report.groups[0].duplicates[0].relative_path,
            PathBuf::from("b.md")
        );
        assert_eq!(fs::read_to_string(&duplicate).unwrap(), before);
    }
}
