use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::daily_log::rust_derived_root;
use crate::dream_config::DreamConfig;
use crate::lock::{self, LockOptions, LockOwner};
use crate::transcript_index::{scan_transcript_dir, TASK_TYPE_MAIN_SESSION};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DreamGateState {
    pub last_session_scan_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DreamGateInput {
    pub memory_dir: PathBuf,
    pub project_state_dir: PathBuf,
    pub current_session_id: String,
    pub now_ms: u64,
    pub holder_pid: u32,
    pub force: bool,
    pub kairos_active: bool,
    pub remote_mode: bool,
    pub auto_memory_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DreamGateTrigger {
    pub session_ids: Vec<String>,
    pub sessions_since_last_consolidation: usize,
    pub last_consolidated_at_ms: u64,
    pub hours_since_last_consolidation: u64,
    pub prior_mtime_ms: u64,
    pub lock_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DreamGateDecision {
    Run(DreamGateTrigger),
    Skip(DreamGateSkip),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DreamGateSkip {
    Disabled,
    KairosActive,
    RemoteMode,
    AutoMemoryDisabled,
    TimeGate {
        hours_since: u64,
        min_hours: u64,
    },
    ScanThrottled {
        ms_since_last_scan: u64,
        interval_ms: u64,
    },
    NotEnoughSessions {
        sessions: usize,
        min_sessions: usize,
    },
    LockHeld,
}

pub async fn evaluate_dream_gate(
    state: &mut DreamGateState,
    config: &DreamConfig,
    input: &DreamGateInput,
) -> Result<DreamGateDecision, BoxError> {
    if !input.force {
        if !input.auto_memory_enabled {
            return Ok(DreamGateDecision::Skip(DreamGateSkip::AutoMemoryDisabled));
        }
        if input.kairos_active {
            return Ok(DreamGateDecision::Skip(DreamGateSkip::KairosActive));
        }
        if input.remote_mode {
            return Ok(DreamGateDecision::Skip(DreamGateSkip::RemoteMode));
        }
        if !config.enabled {
            return Ok(DreamGateDecision::Skip(DreamGateSkip::Disabled));
        }
    }

    let last_consolidated_at_ms = lock::last_consolidated_at(&input.memory_dir).await?;
    let elapsed_ms = input.now_ms.saturating_sub(last_consolidated_at_ms);
    let hours_since = elapsed_ms / 3_600_000;
    if !input.force && hours_since < config.min_hours {
        return Ok(DreamGateDecision::Skip(DreamGateSkip::TimeGate {
            hours_since,
            min_hours: config.min_hours,
        }));
    }

    let since_scan = input.now_ms.saturating_sub(state.last_session_scan_at_ms);
    if !input.force
        && state.last_session_scan_at_ms > 0
        && since_scan < config.session_scan_interval_ms
    {
        return Ok(DreamGateDecision::Skip(DreamGateSkip::ScanThrottled {
            ms_since_last_scan: since_scan,
            interval_ms: config.session_scan_interval_ms,
        }));
    }
    state.last_session_scan_at_ms = input.now_ms;

    let session_ids = list_sessions_touched_since(
        &input.project_state_dir,
        last_consolidated_at_ms,
        &input.current_session_id,
    )?;
    if !input.force && session_ids.len() < config.min_sessions {
        return Ok(DreamGateDecision::Skip(DreamGateSkip::NotEnoughSessions {
            sessions: session_ids.len(),
            min_sessions: config.min_sessions,
        }));
    }

    let prior_mtime_ms = if input.force {
        last_consolidated_at_ms
    } else {
        let owner = LockOwner {
            holder_pid: input.holder_pid,
        };
        match lock::try_acquire_for(&input.memory_dir, &owner, &LockOptions::default()).await? {
            Some(prior) => prior,
            None => return Ok(DreamGateDecision::Skip(DreamGateSkip::LockHeld)),
        }
    };

    Ok(DreamGateDecision::Run(DreamGateTrigger {
        session_ids,
        sessions_since_last_consolidation: list_sessions_touched_since(
            &input.project_state_dir,
            last_consolidated_at_ms,
            &input.current_session_id,
        )?
        .len(),
        last_consolidated_at_ms,
        hours_since_last_consolidation: hours_since,
        prior_mtime_ms,
        lock_token: build_lock_token(&input.memory_dir, input.now_ms, input.holder_pid),
    }))
}

pub fn project_state_dir_from_memory_dir(memory_dir: &Path) -> PathBuf {
    memory_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn derived_root_from_memory_dir(memory_dir: &Path) -> PathBuf {
    rust_derived_root(&project_state_dir_from_memory_dir(memory_dir))
}

pub fn system_time_to_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

pub(crate) fn list_sessions_touched_since(
    project_state_dir: &Path,
    since_ms: u64,
    current_session_id: &str,
) -> Result<Vec<String>, BoxError> {
    let mut sessions = scan_transcript_dir(project_state_dir)?
        .into_iter()
        .filter(|meta| meta.task_type.as_deref() == Some(TASK_TYPE_MAIN_SESSION))
        .filter(|meta| meta.mtime_ms > since_ms)
        .filter_map(|meta| meta.session_id)
        .filter(|session_id| session_id != current_session_id)
        .collect::<Vec<_>>();
    // G2 (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16)：canonical 项目的会话计数
    // 并入成员（worktree）项目的转写 —— 与语料装配同口径，否则 gate 报
    // session_count_unmet 而语料实际有料。成员目录读错 fail-soft 跳过。
    for member_dir in crate::identity_members::member_project_state_dirs(project_state_dir) {
        let Ok(metas) = scan_transcript_dir(&member_dir) else {
            continue;
        };
        sessions.extend(
            metas
                .into_iter()
                .filter(|meta| meta.task_type.as_deref() == Some(TASK_TYPE_MAIN_SESSION))
                .filter(|meta| meta.mtime_ms > since_ms)
                .filter_map(|meta| meta.session_id)
                .filter(|session_id| session_id != current_session_id),
        );
    }
    sessions.sort();
    sessions.dedup();
    Ok(sessions)
}

pub(crate) fn build_lock_token(memory_dir: &Path, now_ms: u64, holder_pid: u32) -> String {
    format!(
        "dream:{}:{now_ms}:{holder_pid}",
        memory_dir.to_string_lossy()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::UNIX_EPOCH;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    const CURRENT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const OTHER_A: &str = "660e8400-e29b-41d4-a716-446655440000";
    const OTHER_B: &str = "770e8400-e29b-41d4-a716-446655440000";

    fn input(dir: &TempDir, now_ms: u64) -> DreamGateInput {
        DreamGateInput {
            memory_dir: dir.path().join("memory"),
            project_state_dir: dir.path().to_path_buf(),
            current_session_id: CURRENT.to_owned(),
            now_ms,
            holder_pid: 51_001,
            force: false,
            kairos_active: false,
            remote_mode: false,
            auto_memory_enabled: true,
        }
    }

    fn enabled_config(min_sessions: usize) -> DreamConfig {
        DreamConfig {
            enabled: true,
            min_hours: 24,
            min_sessions,
            session_scan_interval_ms: 600_000,
            auto_promote: Default::default(),
            imagination_min_hours: 48,
            ..DreamConfig::default()
        }
    }

    fn write_session(dir: &TempDir, session_id: &str, mtime_s: i64) {
        let path = dir.path().join(format!("{session_id}.jsonl"));
        fs::write(&path, "{}\n").unwrap();
        set_file_mtime(path, FileTime::from_unix_time(mtime_s, 0)).unwrap();
    }

    #[tokio::test]
    async fn dream_gate_skips_when_disabled() {
        let dir = TempDir::new().unwrap();
        let mut state = DreamGateState::default();

        // Explicit disabled config — the default is now enabled (A4/D2), so this
        // test must construct the disabled case it asserts on.
        let disabled = DreamConfig {
            enabled: false,
            ..DreamConfig::default()
        };
        let decision = evaluate_dream_gate(&mut state, &disabled, &input(&dir, 0))
            .await
            .unwrap();

        assert_eq!(decision, DreamGateDecision::Skip(DreamGateSkip::Disabled));
    }

    #[tokio::test]
    async fn dream_gate_enforces_time_gate() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(dir.path().join("memory/.consolidate-lock"), "").unwrap();
        set_file_mtime(
            dir.path().join("memory/.consolidate-lock"),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let now_ms = 1_700_000_000_000 + 23 * 3_600_000;
        let mut state = DreamGateState::default();

        let decision = evaluate_dream_gate(&mut state, &enabled_config(1), &input(&dir, now_ms))
            .await
            .unwrap();

        assert!(matches!(
            decision,
            DreamGateDecision::Skip(DreamGateSkip::TimeGate { .. })
        ));
    }

    #[tokio::test]
    async fn dream_gate_counts_sessions_and_excludes_current_session() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, CURRENT, 1_700_100_000);
        write_session(&dir, OTHER_A, 1_700_100_001);
        write_session(&dir, OTHER_B, 1_700_100_002);
        let mut state = DreamGateState::default();

        let decision = evaluate_dream_gate(
            &mut state,
            &enabled_config(2),
            &input(&dir, 1_700_200_000_000),
        )
        .await
        .unwrap();

        let DreamGateDecision::Run(trigger) = decision else {
            panic!("expected run decision");
        };
        assert_eq!(
            trigger.session_ids,
            vec![OTHER_A.to_owned(), OTHER_B.to_owned()]
        );
        assert_eq!(trigger.sessions_since_last_consolidation, 2);
    }

    #[tokio::test]
    async fn dream_gate_skips_when_not_enough_sessions() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, OTHER_A, 1_700_100_001);
        let mut state = DreamGateState::default();

        let decision = evaluate_dream_gate(
            &mut state,
            &enabled_config(2),
            &input(&dir, 1_700_200_000_000),
        )
        .await
        .unwrap();

        assert_eq!(
            decision,
            DreamGateDecision::Skip(DreamGateSkip::NotEnoughSessions {
                sessions: 1,
                min_sessions: 2
            })
        );
    }

    #[tokio::test]
    async fn dream_gate_throttles_repeated_session_scans_after_session_miss() {
        let dir = TempDir::new().unwrap();
        let mut state = DreamGateState::default();
        let cfg = enabled_config(2);

        let first = evaluate_dream_gate(&mut state, &cfg, &input(&dir, 1_700_200_000_000))
            .await
            .unwrap();
        let second = evaluate_dream_gate(&mut state, &cfg, &input(&dir, 1_700_200_001_000))
            .await
            .unwrap();

        assert!(matches!(
            first,
            DreamGateDecision::Skip(DreamGateSkip::NotEnoughSessions { .. })
        ));
        assert!(matches!(
            second,
            DreamGateDecision::Skip(DreamGateSkip::ScanThrottled { .. })
        ));
    }

    #[tokio::test]
    async fn dream_gate_acquires_lock_with_syscall_mtime_precision() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, OTHER_A, 1_700_100_001);
        let mut state = DreamGateState::default();

        let decision = evaluate_dream_gate(
            &mut state,
            &enabled_config(1),
            &input(&dir, system_time_to_ms(UNIX_EPOCH) + 1_700_200_000_000),
        )
        .await
        .unwrap();

        assert!(matches!(decision, DreamGateDecision::Run(_)));
        assert_eq!(
            fs::read_to_string(dir.path().join("memory/.consolidate-lock")).unwrap(),
            "51001"
        );
    }
}
