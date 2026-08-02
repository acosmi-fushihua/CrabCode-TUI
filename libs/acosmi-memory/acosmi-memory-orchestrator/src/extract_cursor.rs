use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_write::{atomic_write, BoxError};
use crate::daily_log::rust_derived_root;

/// 触发抽取所需的最小"有新消息的 turn"数。
///
/// **1 = 首个有新消息的 turn 即触发**（`turns_since_last_extraction += 1`
/// 之后判 `1 < 1` 为假）。这是既有默认值，由
/// `extract_cursor_runs_on_first_eligible_turn_by_default` 钉住；2026-07-27
/// 的在飞守卫修复**刻意不改动它** —— 是否收紧节流是一个独立的产品裁决，
/// 不该搭车在一个正确性修复里翻转一条已命名的契约测试。
pub const DEFAULT_EXTRACT_MIN_ELIGIBLE_TURNS: u64 = 1;

/// W-MEMORY-EVOLUTION PR-11 — on-disk filename for the persisted per-project
/// extract cursor. Lives next to `dream-config.json` under the sibling
/// `.memory-rust-derived/` derived root so a process restart resumes the
/// extraction window rather than re-extracting from scratch (bug1), and so
/// distinct projects keep independent cursors (bug2).
const CURSOR_FILE: &str = "extract-cursor.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractCursorConfig {
    pub min_eligible_turns: u64,
}

impl Default for ExtractCursorConfig {
    fn default() -> Self {
        Self {
            min_eligible_turns: DEFAULT_EXTRACT_MIN_ELIGIBLE_TURNS,
        }
    }
}

/// 持久化的抽取游标。
///
/// 2026-07-27（§27.3-G8）：**`in_progress` 字段已退役**。它曾经既是"在飞
/// 去重"的判据、又必须在进程重启后被清掉，于是 `load_extract_cursor` 在加载
/// 时**无条件**重置它 —— 而 `turn_evaluator` 每个 turn 都重新 load，两者相乘
/// 使 `ExtractCursorSkip::InProgress` 成为**生产路径上不可达的分支**（实测
/// 9 个有派生层的项目里 7 个 `in_progress` 恒 true 且从未完成过一次抽取）。
///
/// 在飞真源改为进程内的 [`crate::result_listener::ResultListener`]：重启 =
/// 空表 = 天然放行，不需要任何"进程换代"判定，也就不会新造一份判活口径
/// （往状态文件写 PID 再判它活没活，正是本仓 cron 僵尸 PID 楔死的同族缺陷）。
/// 旧文件里残留的 `in_progress` 键会被 serde 忽略（未开 `deny_unknown_fields`）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractCursorState {
    #[serde(default)]
    pub last_assistant_uuid: Option<String>,
    #[serde(default)]
    pub last_total_model_visible: u64,
    #[serde(default)]
    pub turns_since_last_extraction: u64,
}

/// W-MEMORY-EVOLUTION PR-11 — disk path for the persisted extract cursor.
/// Mirrors `dream_config::dream_config_path` (sibling `.memory-rust-derived/`
/// derived root, keyed by `project_state_dir`).
#[must_use]
pub fn extract_cursor_path(project_state_dir: &Path) -> PathBuf {
    rust_derived_root(project_state_dir).join(CURSOR_FILE)
}

/// W-MEMORY-EVOLUTION PR-11 — load the persisted per-project extract cursor.
///
/// Fail-soft: a missing file or any parse error yields `ExtractCursorState`
/// default (so a corrupt cursor degrades to "re-evaluate from scratch" rather
/// than panicking).
#[must_use]
pub fn load_extract_cursor(project_state_dir: &Path) -> ExtractCursorState {
    let path = extract_cursor_path(project_state_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ExtractCursorState::default(),
        Err(e) => {
            log::warn!(
                "extract-cursor read failed ({}); falling back to default cursor: {e}",
                path.display(),
            );
            return ExtractCursorState::default();
        }
    };
    match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(e) => {
            log::warn!(
                "extract-cursor parse failed ({}); falling back to default cursor: {e}",
                path.display(),
            );
            ExtractCursorState::default()
        }
    }
}

/// W-MEMORY-EVOLUTION PR-11 — atomically persist the per-project extract
/// cursor to `<project_state_dir>/.memory-rust-derived/extract-cursor.json`.
/// Mirrors `dream_config::write_dream_config` (serde_json + `atomic_write`).
pub async fn save_extract_cursor(
    project_state_dir: &Path,
    state: &ExtractCursorState,
) -> Result<(), BoxError> {
    let path = extract_cursor_path(project_state_dir);
    let bytes = serde_json::to_vec_pretty(state)?;
    atomic_write(&path, &bytes).await
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractWindowMeta {
    pub last_assistant_uuid: Option<String>,
    pub model_visible_count_since_cursor: Option<u64>,
    pub total_model_visible: Option<u64>,
    pub has_memory_writes_since_cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractTrigger {
    pub previous_cursor_uuid: Option<String>,
    pub last_assistant_uuid: String,
    pub new_message_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractCursorDecision {
    Run(ExtractTrigger),
    Skip(ExtractCursorSkip),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractCursorSkip {
    Disabled,
    AutoMemoryDisabled,
    RemoteMode,
    MissingAssistantUuid,
    InProgress,
    DirectMemoryWriteAdvancedCursor { cursor_uuid: String },
    Throttled { turns: u64, min_turns: u64 },
    NoNewMessages,
}

/// 判定本 turn 是否触发一次抽取。
///
/// `in_flight` 由调用方从
/// [`crate::result_listener::ResultListener::has_in_flight`] 取得 ——
/// **在飞去重的唯一真源**，见 [`ExtractCursorState`] 的说明。
#[allow(clippy::too_many_arguments)]
pub fn evaluate_extract_cursor(
    state: &mut ExtractCursorState,
    config: &ExtractCursorConfig,
    enabled: bool,
    auto_memory_enabled: bool,
    remote_mode: bool,
    in_flight: bool,
    window: &ExtractWindowMeta,
) -> ExtractCursorDecision {
    if !enabled {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::Disabled);
    }
    if !auto_memory_enabled {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::AutoMemoryDisabled);
    }
    if remote_mode {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::RemoteMode);
    }
    if in_flight {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::InProgress);
    }

    let Some(last_assistant_uuid) = window.last_assistant_uuid.clone() else {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::MissingAssistantUuid);
    };

    if window.has_memory_writes_since_cursor {
        state.last_assistant_uuid = Some(last_assistant_uuid.clone());
        if let Some(total) = window.total_model_visible {
            state.last_total_model_visible = total;
        }
        state.turns_since_last_extraction = 0;
        return ExtractCursorDecision::Skip(ExtractCursorSkip::DirectMemoryWriteAdvancedCursor {
            cursor_uuid: last_assistant_uuid,
        });
    }

    let new_message_count = new_message_count_since_cursor(state, window);
    if new_message_count == 0 {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::NoNewMessages);
    }

    state.turns_since_last_extraction += 1;
    if state.turns_since_last_extraction < config.min_eligible_turns {
        return ExtractCursorDecision::Skip(ExtractCursorSkip::Throttled {
            turns: state.turns_since_last_extraction,
            min_turns: config.min_eligible_turns,
        });
    }

    ExtractCursorDecision::Run(ExtractTrigger {
        previous_cursor_uuid: state.last_assistant_uuid.clone(),
        last_assistant_uuid,
        new_message_count,
    })
}

/// 抽取成功回执：推进游标窗口。
///
/// 失败回执**没有对应函数** —— 失败时游标必须保持原样（不推进），而在飞
/// 标记已不在磁盘上，所以失败路径没有任何要落盘的状态变更。
pub fn complete_extract_success(
    state: &mut ExtractCursorState,
    last_assistant_uuid: &str,
    total_model_visible: Option<u64>,
) {
    state.last_assistant_uuid = Some(last_assistant_uuid.to_owned());
    if let Some(total) = total_model_visible {
        state.last_total_model_visible = total;
    }
    state.turns_since_last_extraction = 0;
}

pub fn build_window_meta(
    last_assistant_uuid: String,
    message_counts: &BTreeMap<String, u64>,
) -> ExtractWindowMeta {
    let user = message_counts.get("user").copied().unwrap_or(0);
    let assistant = message_counts.get("assistant").copied().unwrap_or(0);
    ExtractWindowMeta {
        last_assistant_uuid: if last_assistant_uuid.is_empty() {
            None
        } else {
            Some(last_assistant_uuid)
        },
        model_visible_count_since_cursor: None,
        total_model_visible: Some(user + assistant),
        has_memory_writes_since_cursor: false,
    }
}

fn new_message_count_since_cursor(state: &ExtractCursorState, window: &ExtractWindowMeta) -> u64 {
    if let Some(count) = window.model_visible_count_since_cursor {
        return count;
    }
    if let Some(total) = window.total_model_visible {
        return total.saturating_sub(state.last_total_model_visible);
    }
    1
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn window(uuid: &str, total: u64) -> ExtractWindowMeta {
        ExtractWindowMeta {
            last_assistant_uuid: Some(uuid.to_owned()),
            model_visible_count_since_cursor: None,
            total_model_visible: Some(total),
            has_memory_writes_since_cursor: false,
        }
    }

    #[test]
    fn extract_cursor_skips_when_feature_disabled() {
        let mut state = ExtractCursorState::default();

        let decision = evaluate_extract_cursor(
            &mut state,
            &ExtractCursorConfig::default(),
            false,
            true,
            false,
            false,
            &window("a", 2),
        );

        assert_eq!(
            decision,
            ExtractCursorDecision::Skip(ExtractCursorSkip::Disabled)
        );
    }

    #[test]
    fn extract_cursor_runs_on_first_eligible_turn_by_default() {
        let mut state = ExtractCursorState::default();

        let decision = evaluate_extract_cursor(
            &mut state,
            &ExtractCursorConfig::default(),
            true,
            true,
            false,
            false,
            &window("assistant-1", 2),
        );

        assert_eq!(
            decision,
            ExtractCursorDecision::Run(ExtractTrigger {
                previous_cursor_uuid: None,
                last_assistant_uuid: "assistant-1".to_owned(),
                new_message_count: 2,
            })
        );
    }

    /// §27.3-G8 的核心回归：**同一项目已有 runner 在飞时必须跳过**。
    /// 这正是旧实现（磁盘 `in_progress` + 每轮 load 时无条件重置）永远走不
    /// 到的分支 —— 于是每个 turn 都重复触发抽取，且没有任何一次能完成。
    #[test]
    fn extract_cursor_skips_while_a_runner_is_in_flight() {
        let mut state = ExtractCursorState::default();

        let decision = evaluate_extract_cursor(
            &mut state,
            &ExtractCursorConfig::default(),
            true,
            true,
            false,
            true, // in_flight
            &window("assistant-1", 2),
        );

        assert_eq!(
            decision,
            ExtractCursorDecision::Skip(ExtractCursorSkip::InProgress)
        );
        assert_eq!(
            state.turns_since_last_extraction, 0,
            "在飞时不推进任何计数器"
        );
    }

    #[test]
    fn extract_cursor_advances_without_running_when_main_agent_wrote_memory() {
        let mut state = ExtractCursorState {
            last_assistant_uuid: Some("old".to_owned()),
            last_total_model_visible: 4,
            turns_since_last_extraction: 0,
        };
        let mut meta = window("assistant-2", 6);
        meta.has_memory_writes_since_cursor = true;

        let decision = evaluate_extract_cursor(
            &mut state,
            &ExtractCursorConfig::default(),
            true,
            true,
            false,
            false,
            &meta,
        );

        assert_eq!(
            decision,
            ExtractCursorDecision::Skip(ExtractCursorSkip::DirectMemoryWriteAdvancedCursor {
                cursor_uuid: "assistant-2".to_owned()
            })
        );
        assert_eq!(state.last_assistant_uuid.as_deref(), Some("assistant-2"));
        assert_eq!(state.last_total_model_visible, 6);
    }

    #[test]
    fn extract_cursor_throttles_by_eligible_turn_count() {
        let mut state = ExtractCursorState::default();
        let cfg = ExtractCursorConfig {
            min_eligible_turns: 2,
        };

        let first =
            evaluate_extract_cursor(&mut state, &cfg, true, true, false, false, &window("a", 2));
        let second =
            evaluate_extract_cursor(&mut state, &cfg, true, true, false, false, &window("b", 4));

        assert_eq!(
            first,
            ExtractCursorDecision::Skip(ExtractCursorSkip::Throttled {
                turns: 1,
                min_turns: 2
            })
        );
        assert!(matches!(second, ExtractCursorDecision::Run(_)));
    }

    #[test]
    fn extract_cursor_completion_advances_cursor_only_on_success() {
        let mut state = ExtractCursorState::default();
        assert!(matches!(
            evaluate_extract_cursor(
                &mut state,
                &ExtractCursorConfig::default(),
                true,
                true,
                false,
                false,
                &window("a", 2),
            ),
            ExtractCursorDecision::Run(_)
        ));

        // 失败回执：游标保持原样（不推进）—— 没有"失败结算"函数要调。
        assert_eq!(state.last_assistant_uuid, None);

        assert!(matches!(
            evaluate_extract_cursor(
                &mut state,
                &ExtractCursorConfig::default(),
                true,
                true,
                false,
                false,
                &window("a", 2),
            ),
            ExtractCursorDecision::Run(_)
        ));
        complete_extract_success(&mut state, "a", Some(2));
        assert_eq!(state.last_assistant_uuid.as_deref(), Some("a"));
        assert_eq!(state.last_total_model_visible, 2);
        assert_eq!(state.turns_since_last_extraction, 0);
    }

    #[test]
    fn extract_cursor_uses_ts_counts_to_compute_model_visible_delta() {
        let mut counts = BTreeMap::new();
        counts.insert("user".to_owned(), 3);
        counts.insert("assistant".to_owned(), 2);
        let mut state = ExtractCursorState {
            last_assistant_uuid: Some("old".to_owned()),
            last_total_model_visible: 3,
            turns_since_last_extraction: 0,
        };
        let meta = build_window_meta("new".to_owned(), &counts);

        let decision = evaluate_extract_cursor(
            &mut state,
            &ExtractCursorConfig::default(),
            true,
            true,
            false,
            false,
            &meta,
        );

        let ExtractCursorDecision::Run(trigger) = decision else {
            panic!("expected run");
        };
        assert_eq!(trigger.new_message_count, 2);
        assert_eq!(trigger.previous_cursor_uuid.as_deref(), Some("old"));
    }

    // ──────────────────────────────────────────────────────────────────
    // W-MEMORY-EVOLUTION PR-11 — persisted per-project extract cursor.
    // ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn extract_cursor_save_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let state = ExtractCursorState {
            last_assistant_uuid: Some("assistant-7".to_owned()),
            last_total_model_visible: 42,
            turns_since_last_extraction: 3,
        };

        save_extract_cursor(dir.path(), &state).await.unwrap();
        let loaded = load_extract_cursor(dir.path());

        assert_eq!(loaded, state);
        // landed under the sibling derived root, not inside memory/.
        assert!(
            extract_cursor_path(dir.path()).starts_with(dir.path().join(".memory-rust-derived"))
        );
    }

    #[test]
    fn extract_cursor_load_missing_file_is_default() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            load_extract_cursor(dir.path()),
            ExtractCursorState::default()
        );
    }

    #[test]
    fn extract_cursor_load_corrupt_file_is_default() {
        let dir = TempDir::new().unwrap();
        let path = extract_cursor_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not-json").unwrap();

        assert_eq!(
            load_extract_cursor(dir.path()),
            ExtractCursorState::default()
        );
    }

    /// 既有磁盘游标（本机 9 个项目全都停在 `in_progress: true`）必须能原样
    /// 读入，窗口位置保留、退役字段被忽略。
    ///
    /// 这条**取代**了旧的 `extract_cursor_load_recovers_in_progress_to_false`
    /// —— 那条测试钉住的正是本次要拆掉的缺陷（加载时无条件重置在飞标记），
    /// 属有意的契约重指向，不是回归。
    #[test]
    fn legacy_cursor_with_in_progress_key_is_readable_and_field_is_ignored() {
        let dir = TempDir::new().unwrap();
        let path = extract_cursor_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"last_assistant_uuid":"assistant-9","last_total_model_visible":10,
                "turns_since_last_extraction":47,"in_progress":true}"#,
        )
        .unwrap();

        let loaded = load_extract_cursor(dir.path());

        assert_eq!(loaded.last_assistant_uuid.as_deref(), Some("assistant-9"));
        assert_eq!(loaded.last_total_model_visible, 10);
        assert_eq!(loaded.turns_since_last_extraction, 47);
    }

    /// bug2 (cross-project pollution) fix: two distinct project_state_dirs
    /// keep independent cursors — saving A does not affect loading B.
    #[tokio::test]
    async fn extract_cursor_is_isolated_per_project() {
        let root = TempDir::new().unwrap();
        let project_a = root.path().join("project-a");
        let project_b = root.path().join("project-b");
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();

        let cursor_a = ExtractCursorState {
            last_assistant_uuid: Some("a-cursor".to_owned()),
            last_total_model_visible: 100,
            turns_since_last_extraction: 5,
        };
        save_extract_cursor(&project_a, &cursor_a).await.unwrap();

        // B never saved → still default; A unchanged.
        assert_eq!(
            load_extract_cursor(&project_b),
            ExtractCursorState::default()
        );
        assert_eq!(load_extract_cursor(&project_a), cursor_a);

        // saving B does not bleed into A.
        let cursor_b = ExtractCursorState {
            last_assistant_uuid: Some("b-cursor".to_owned()),
            last_total_model_visible: 7,
            turns_since_last_extraction: 1,
        };
        save_extract_cursor(&project_b, &cursor_b).await.unwrap();

        assert_eq!(load_extract_cursor(&project_a), cursor_a);
        assert_eq!(load_extract_cursor(&project_b), cursor_b);
    }
}
