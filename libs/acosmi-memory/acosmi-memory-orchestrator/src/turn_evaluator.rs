use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::path::{Component, Path};
use std::sync::Arc;

use acosmi_memory_journal::{Journal, WorkKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::dream_config::{read_dream_config_optional, DreamConfig};
use crate::dream_gate::{
    evaluate_dream_gate, project_state_dir_from_memory_dir, DreamGateDecision, DreamGateInput,
    DreamGateState,
};
use crate::extract_cursor::{
    build_window_meta, evaluate_extract_cursor, load_extract_cursor, save_extract_cursor,
    ExtractCursorConfig, ExtractCursorDecision,
};
use crate::result_listener::{PendingRunner, ResultListener};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual `dream` runner trigger
/// payload. Bypasses the 4/6-gate dream policy: TUI «Run Dream Now» button
/// expresses explicit user intent, so we skip `KAIROS` / `auto_memory_enabled`
/// / `min_hours` / `min_sessions` checks and synthesize a runner trigger
/// unconditionally. `lock_token` is still acquired (so concurrent automatic
/// runs are excluded), but a busy lock surfaces as `gate_skip_reason =
/// "lock_held"` rather than `Run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DreamRunNowRequest {
    pub session_id: String,
    pub current_session_id: String,
    pub memory_dir: PathBuf,
    pub now_ms: u64,
}

/// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual `extract` runner
/// trigger payload. Same bypass semantics as `DreamRunNowRequest` — feature
/// gates (`EXTRACT_MEMORIES` / `auto_memory_enabled` / `remote_mode`) and the
/// extract-cursor sufficiency check are skipped because the user clicked the
/// button explicitly. Cursor state is still advanced on completion via the
/// existing `ResultListener` pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractRunNowRequest {
    pub session_id: String,
    pub last_assistant_uuid: String,
    pub memory_dir: PathBuf,
    pub team_memory_dir: Option<PathBuf>,
    pub message_counts: BTreeMap<String, u64>,
    pub now_ms: u64,
}

/// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual run-now response.
/// `triggers` is `Vec` (not `Option`) for shape parity with the gated
/// `evaluate_turn_end` path; a busy lock / scope failure yields an empty
/// triggers list + a populated `gate_skip_reason` so the caller can render
/// a friendly «already running» tooltip without inferring from log lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunNowResponse {
    pub triggers: Vec<TurnEndTrigger>,
    pub gate_skip_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEndEvaluateRequest {
    pub recovery_schema_version: u64,
    pub session_id: String,
    pub current_session_id: String,
    pub last_assistant_uuid: String,
    pub project_cwd: PathBuf,
    pub transcript_path: PathBuf,
    pub memory_dir: PathBuf,
    pub team_memory_dir: Option<PathBuf>,
    pub message_counts: BTreeMap<String, u64>,
    pub feature_flags: BTreeMap<String, bool>,
    pub requested_kinds: Vec<RunnerKind>,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEndEvaluateResponse {
    pub triggers: Vec<TurnEndTrigger>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TurnEndTrigger {
    pub trigger_id: String,
    pub kind: RunnerKind,
    pub lock_token: Option<String>,
    pub runner_payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerKind {
    Dream,
    Extract,
}

impl RunnerKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dream => "dream",
            Self::Extract => "extract",
        }
    }
}

/// W-MEMORY-EVOLUTION PR-11 — the extract cursor was previously a single
/// in-memory `ExtractCursorState` field here. That caused two bugs:
///
/// * bug1 (restart duplicate extraction): a process restart reset the
///   cursor to default → the next turn re-extracted the whole window.
/// * bug2 (cross-project pollution): one orchestrator serves many projects
///   (each `evaluate_turn_end` carries its own `memory_dir`), but all
///   projects shared this one cursor → interleaved A/B turns clobbered each
///   other's cursor.
///
/// The cursor is now persisted per-project under
/// `<project_state_dir>/.memory-rust-derived/extract-cursor.json` and is
/// loaded / saved per turn (see `evaluate_turn_end` /
/// `evaluate_extract_run_now`) and on completion (see
/// `ResultListener::handle_completed`). No shared in-memory cursor remains.
pub const RUNNER_RECOVERY_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunnerRecoveryLocator {
    pub recovery_schema_version: u64,
    pub trigger_id: String,
    pub kind: RunnerKind,
    pub session_id: String,
    pub current_session_id: String,
    pub context_leaf_uuid: String,
    pub project_cwd: PathBuf,
    pub transcript_path: PathBuf,
    pub memory_dir: PathBuf,
    pub project_state_dir: PathBuf,
}

impl RunnerRecoveryLocator {
    fn from_evaluate_request(
        request: &TurnEndEvaluateRequest,
        trigger: &TurnEndTrigger,
    ) -> Result<Self, BoxError> {
        let locator = Self {
            recovery_schema_version: request.recovery_schema_version,
            trigger_id: trigger.trigger_id.clone(),
            kind: trigger.kind,
            session_id: request.session_id.clone(),
            current_session_id: request.current_session_id.clone(),
            context_leaf_uuid: request.last_assistant_uuid.clone(),
            project_cwd: request.project_cwd.clone(),
            transcript_path: request.transcript_path.clone(),
            memory_dir: request.memory_dir.clone(),
            project_state_dir: project_state_dir_from_memory_dir(&request.memory_dir),
        };
        locator.validate()?;
        Ok(locator)
    }

    pub fn validate(&self) -> Result<(), BoxError> {
        if self.recovery_schema_version != RUNNER_RECOVERY_SCHEMA_VERSION {
            return Err(invalid_recovery(format!(
                "unsupported runner recovery_schema_version {}",
                self.recovery_schema_version
            )));
        }
        validate_trigger_subject(&self.trigger_id, self.kind)?;
        validate_canonical_uuid("session_id", &self.session_id)?;
        validate_canonical_uuid("current_session_id", &self.current_session_id)?;
        validate_canonical_uuid("context_leaf_uuid", &self.context_leaf_uuid)?;
        validate_absolute_clean_path("project_cwd", &self.project_cwd)?;
        validate_absolute_clean_path("transcript_path", &self.transcript_path)?;
        validate_absolute_clean_path("memory_dir", &self.memory_dir)?;
        validate_absolute_clean_path("project_state_dir", &self.project_state_dir)?;
        if project_state_dir_from_memory_dir(&self.memory_dir) != self.project_state_dir {
            return Err(invalid_recovery(
                "project_state_dir must be the direct parent of memory_dir",
            ));
        }
        let expected_transcript_name = format!("{}.jsonl", self.session_id);
        if self
            .transcript_path
            .file_name()
            .and_then(|name| name.to_str())
            != Some(expected_transcript_name.as_str())
        {
            return Err(invalid_recovery(
                "transcript_path filename must match session_id",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DurableRunnerWork {
    pub recovery: RunnerRecoveryLocator,
    pub trigger: TurnEndTrigger,
    pub pending: PendingRunner,
}

impl DurableRunnerWork {
    pub fn validate(&self) -> Result<(), BoxError> {
        self.recovery.validate()?;
        validate_trigger_id(&self.trigger)?;
        if self.recovery.trigger_id != self.trigger.trigger_id
            || self.recovery.kind != self.trigger.kind
        {
            return Err(invalid_recovery(
                "recovery locator subject does not match durable trigger",
            ));
        }
        if self.pending.trigger_id != self.trigger.trigger_id {
            return Err(invalid_recovery(
                "pending trigger_id does not match durable trigger",
            ));
        }
        if self.pending.kind != self.trigger.kind.as_str() {
            return Err(invalid_recovery(
                "pending runner kind does not match durable trigger",
            ));
        }
        if self.pending.session_id != self.recovery.session_id {
            return Err(invalid_recovery(
                "pending session_id does not match recovery locator",
            ));
        }
        if self.pending.memory_dir != self.recovery.memory_dir
            || self.pending.project_state_dir != self.recovery.project_state_dir
        {
            return Err(invalid_recovery(
                "pending paths do not match recovery locator",
            ));
        }

        require_payload_path(
            &self.trigger.runner_payload,
            "memory_dir",
            &self.recovery.memory_dir,
        )?;
        match self.trigger.kind {
            RunnerKind::Dream => {
                require_payload_path(
                    &self.trigger.runner_payload,
                    "project_state_dir",
                    &self.recovery.project_state_dir,
                )?;
                require_payload_path(
                    &self.trigger.runner_payload,
                    "transcript_dir",
                    &self.recovery.project_state_dir,
                )?;
                if self.pending.extract_last_assistant_uuid.is_some() {
                    return Err(invalid_recovery(
                        "dream pending state must not contain extract cursor identity",
                    ));
                }
            }
            RunnerKind::Extract => {
                let last_assistant_uuid = self
                    .trigger
                    .runner_payload
                    .get("last_assistant_uuid")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid_recovery("extract runner payload is missing last_assistant_uuid")
                    })?;
                if last_assistant_uuid != self.recovery.context_leaf_uuid
                    || self.pending.extract_last_assistant_uuid.as_deref()
                        != Some(self.recovery.context_leaf_uuid.as_str())
                {
                    return Err(invalid_recovery(
                        "extract assistant UUID does not match recovery locator",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_canonical_uuid(label: &str, raw: &str) -> Result<(), BoxError> {
    let parsed =
        Uuid::parse_str(raw).map_err(|_| invalid_recovery(format!("{label} must be a UUID")))?;
    if parsed.hyphenated().to_string() != raw {
        return Err(invalid_recovery(format!(
            "{label} must use canonical lowercase hyphenated UUID form"
        )));
    }
    Ok(())
}

fn validate_absolute_clean_path(label: &str, path: &Path) -> Result<(), BoxError> {
    if !path.is_absolute() {
        return Err(invalid_recovery(format!("{label} must be absolute")));
    }
    let Some(raw) = path.to_str() else {
        return Err(invalid_recovery(format!("{label} must be valid UTF-8")));
    };
    if raw.contains('\0') || path.parent().is_none() {
        return Err(invalid_recovery(format!(
            "{label} must not be empty, root-only, or contain NUL"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(invalid_recovery(format!(
            "{label} must not contain '.' or '..' components"
        )));
    }
    Ok(())
}

fn validate_trigger_id(trigger: &TurnEndTrigger) -> Result<(), BoxError> {
    validate_trigger_subject(&trigger.trigger_id, trigger.kind)
}

fn validate_trigger_subject(trigger_id: &str, kind: RunnerKind) -> Result<(), BoxError> {
    let expected_prefix = format!("{}:", kind.as_str());
    let raw_uuid = trigger_id
        .strip_prefix(&expected_prefix)
        .ok_or_else(|| invalid_recovery("trigger_id kind prefix mismatch"))?;
    validate_canonical_uuid("trigger_id UUID", raw_uuid)
}

fn require_payload_path(payload: &Value, key: &str, expected: &Path) -> Result<(), BoxError> {
    let raw = payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_recovery(format!("runner payload is missing {key}")))?;
    if Path::new(raw) != expected {
        return Err(invalid_recovery(format!(
            "runner payload {key} does not match recovery locator"
        )));
    }
    Ok(())
}

fn validate_evaluate_recovery_inputs(
    request: &TurnEndEvaluateRequest,
) -> Result<PathBuf, BoxError> {
    if request.recovery_schema_version != RUNNER_RECOVERY_SCHEMA_VERSION {
        return Err(invalid_recovery(format!(
            "unsupported runner recovery_schema_version {}",
            request.recovery_schema_version
        )));
    }
    validate_canonical_uuid("session_id", &request.session_id)?;
    validate_canonical_uuid("current_session_id", &request.current_session_id)?;
    validate_canonical_uuid("last_assistant_uuid", &request.last_assistant_uuid)?;
    validate_absolute_clean_path("project_cwd", &request.project_cwd)?;
    validate_absolute_clean_path("transcript_path", &request.transcript_path)?;
    validate_absolute_clean_path("memory_dir", &request.memory_dir)?;
    if let Some(team_memory_dir) = request.team_memory_dir.as_ref() {
        validate_absolute_clean_path("team_memory_dir", team_memory_dir)?;
    }
    let expected_transcript_name = format!("{}.jsonl", request.session_id);
    if request
        .transcript_path
        .file_name()
        .and_then(|name| name.to_str())
        != Some(expected_transcript_name.as_str())
    {
        return Err(invalid_recovery(
            "transcript_path filename must match session_id",
        ));
    }
    let project_state_dir = project_state_dir_from_memory_dir(&request.memory_dir);
    validate_absolute_clean_path("project_state_dir", &project_state_dir)?;
    Ok(project_state_dir)
}

fn invalid_recovery(message: impl Into<String>) -> BoxError {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

#[derive(Debug, Default)]
pub struct TurnEvaluator {
    pub dream_gate: DreamGateState,
    pub results: ResultListener,
    journal: Option<Arc<Journal>>,
}

impl TurnEvaluator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_journal(journal: Arc<Journal>) -> Self {
        Self {
            journal: Some(journal),
            ..Self::default()
        }
    }

    fn register_trigger(
        &mut self,
        recovery: RunnerRecoveryLocator,
        pending: PendingRunner,
        trigger: TurnEndTrigger,
        created_at_ms: u64,
    ) -> Result<(), BoxError> {
        if let Some(journal) = self.journal.as_ref() {
            let work = DurableRunnerWork {
                recovery,
                trigger: trigger.clone(),
                pending: pending.clone(),
            };
            work.validate()?;
            journal.enqueue(
                &runner_work_key(&trigger.trigger_id),
                WorkKind::RunnerTrigger,
                &serde_json::to_value(work)?,
                created_at_ms,
            )?;
        }
        self.results.register(pending);
        Ok(())
    }

    pub async fn evaluate_turn_end(
        &mut self,
        request: TurnEndEvaluateRequest,
    ) -> Result<TurnEndEvaluateResponse, BoxError> {
        // Validate the complete restart locator before gate evaluation can
        // acquire a dream lock or mutate the extract cursor.
        let project_state_dir = validate_evaluate_recovery_inputs(&request)?;
        // §25.1 / §27.3-G7：先回收超时在飞 runner。`has_in_flight` 成为去重
        // 真源之后这一步是**必需**的 —— 一条永不回执的 pending 会真的永久
        // 阻塞该项目的抽取。两者必须成对存在。
        self.results.sweep_timeouts(request.now_ms).await;
        let mut triggers = Vec::new();
        let requested = requested_kinds(&request.requested_kinds);

        if requested.dream {
            let stored = match read_dream_config_optional(&project_state_dir) {
                Ok(stored) => stored,
                // A corrupt/unreadable dream-config.json must NOT silently enable
                // dreaming on the turn-end path (which is otherwise TS-flag gated).
                // The A4/D2 default-on belongs to the periodic sweep, not to error
                // recovery here — construct an explicit disabled config so this
                // edge keeps its exact pre-A4 behaviour (disabled on read error).
                Err(_) => Some(DreamConfig {
                    enabled: false,
                    ..DreamConfig::default()
                }),
            };
            let config = DreamConfig::from_feature_flags(stored, &request.feature_flags);
            let input = DreamGateInput {
                memory_dir: request.memory_dir.clone(),
                project_state_dir: project_state_dir.clone(),
                current_session_id: request.current_session_id.clone(),
                now_ms: request.now_ms,
                holder_pid: std::process::id(),
                force: flag(&request.feature_flags, "force_dream"),
                kairos_active: flag(&request.feature_flags, "KAIROS"),
                remote_mode: flag(&request.feature_flags, "remote_mode"),
                auto_memory_enabled: flag_default(
                    &request.feature_flags,
                    "auto_memory_enabled",
                    true,
                ),
            };
            if let DreamGateDecision::Run(trigger) =
                evaluate_dream_gate(&mut self.dream_gate, &config, &input).await?
            {
                let trigger_id = next_trigger_id("dream", &request.session_id, request.now_ms);
                let lock_token = trigger.lock_token.clone();
                let pending = PendingRunner {
                    trigger_id: trigger_id.clone(),
                    kind: "dream".to_owned(),
                    session_id: request.session_id.clone(),
                    memory_dir: request.memory_dir.clone(),
                    project_state_dir: project_state_dir.clone(),
                    lock_token: Some(lock_token.clone()),
                    prior_mtime_ms: Some(trigger.prior_mtime_ms),
                    extract_last_assistant_uuid: None,
                    extract_total_model_visible: None,
                    registered_at_ms: request.now_ms,
                };
                let turn_trigger = TurnEndTrigger {
                    trigger_id,
                    kind: RunnerKind::Dream,
                    lock_token: Some(lock_token),
                    runner_payload: json!({
                        "memory_dir": request.memory_dir.to_string_lossy(),
                        "project_state_dir": project_state_dir.to_string_lossy(),
                        "transcript_dir": project_state_dir.to_string_lossy(),
                        "session_ids": trigger.session_ids,
                        "sessions_since_last_consolidation": trigger.sessions_since_last_consolidation,
                        "last_consolidated_at_ms": trigger.last_consolidated_at_ms,
                        "hours_since_last_consolidation": trigger.hours_since_last_consolidation,
                        "prior_mtime_ms": trigger.prior_mtime_ms,
                        "min_hours": config.min_hours,
                        "min_sessions": config.min_sessions,
                    }),
                };
                let recovery =
                    RunnerRecoveryLocator::from_evaluate_request(&request, &turn_trigger)?;
                if let Err(error) =
                    self.register_trigger(recovery, pending, turn_trigger.clone(), request.now_ms)
                {
                    crate::lock::rollback(&request.memory_dir, trigger.prior_mtime_ms).await?;
                    return Err(error);
                }
                triggers.push(turn_trigger);
            }
        }

        if requested.extract {
            let enabled = flag(&request.feature_flags, "EXTRACT_MEMORIES");
            let auto_memory_enabled =
                flag_default(&request.feature_flags, "auto_memory_enabled", true);
            let remote_mode = flag(&request.feature_flags, "remote_mode");
            let mut window =
                build_window_meta(request.last_assistant_uuid.clone(), &request.message_counts);
            window.has_memory_writes_since_cursor =
                flag(&request.feature_flags, "memory_write_since_cursor");

            // W-MEMORY-EVOLUTION PR-11 — load the persisted per-project cursor,
            // evaluate against it, then save it back if any branch mutated the
            // state (Run sets in_progress=true; direct-memory-write / throttle
            // advance counters). This is what makes the cursor survive a
            // restart and stay isolated per project.
            let mut cursor = load_extract_cursor(&project_state_dir);
            let before = cursor.clone();
            // §27.3-G8：在飞去重的真源是**进程内的 pending 表**，不是磁盘
            // 标记。磁盘标记那条路（写 `in_progress` + 每轮 load 时无条件
            // 重置）使守卫永远命中不了，实测导致 7/9 项目每个 turn 都重复
            // 触发抽取且一次都没完成过。
            let extract_in_flight = self.results.has_in_flight("extract", &project_state_dir);
            let decision = evaluate_extract_cursor(
                &mut cursor,
                &ExtractCursorConfig::default(),
                enabled,
                auto_memory_enabled,
                remote_mode,
                extract_in_flight,
                &window,
            );
            if cursor != before {
                save_extract_cursor(&project_state_dir, &cursor).await?;
            }

            if let ExtractCursorDecision::Run(trigger) = decision {
                let trigger_id = next_trigger_id("extract", &request.session_id, request.now_ms);
                let pending = PendingRunner {
                    trigger_id: trigger_id.clone(),
                    kind: "extract".to_owned(),
                    session_id: request.session_id.clone(),
                    memory_dir: request.memory_dir.clone(),
                    project_state_dir: project_state_dir.clone(),
                    lock_token: None,
                    prior_mtime_ms: None,
                    extract_last_assistant_uuid: Some(trigger.last_assistant_uuid.clone()),
                    extract_total_model_visible: window.total_model_visible,
                    registered_at_ms: request.now_ms,
                };
                let turn_trigger = TurnEndTrigger {
                    trigger_id,
                    kind: RunnerKind::Extract,
                    lock_token: None,
                    runner_payload: json!({
                        "memory_dir": request.memory_dir.to_string_lossy(),
                        "team_memory_dir": request.team_memory_dir.as_ref().map(|path| path.to_string_lossy().to_string()),
                        "previous_cursor_uuid": trigger.previous_cursor_uuid,
                        "last_assistant_uuid": trigger.last_assistant_uuid,
                        "new_message_count": trigger.new_message_count,
                        "team_memory_enabled": flag(&request.feature_flags, "team_memory_enabled"),
                        "skip_index": flag(&request.feature_flags, "skip_index"),
                    }),
                };
                let recovery =
                    RunnerRecoveryLocator::from_evaluate_request(&request, &turn_trigger)?;
                // A failed registration occurs before a pending runner becomes
                // authoritative. Keep the already-persisted observation count
                // so the same unconsumed window remains eligible; only a
                // successful runner completion advances/resets the extract
                // cursor (see ResultListener).
                self.register_trigger(recovery, pending, turn_trigger.clone(), request.now_ms)?;
                triggers.push(turn_trigger);
            }
        }

        Ok(TurnEndEvaluateResponse { triggers })
    }

    /// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual dream trigger
    /// invoked by the TUI «Run Dream Now» button. Bypasses the dream-gate
    /// feature flags (`auto_memory_enabled` / `KAIROS` / `remote_mode` /
    /// `config.enabled`) and time/scan/min-sessions gates: the user clicked
    /// the button explicitly, so we forge a trigger regardless. The
    /// consolidation lock is still respected so concurrent automatic and
    /// manual dream runs are mutually excluded — a busy lock surfaces as
    /// `gate_skip_reason = "lock_held"` (empty triggers list) so the TUI
    /// can render a friendly «already running» tooltip without inferring
    /// from log lines.
    ///
    /// The synthesized trigger payload mirrors the automatic-path
    /// `runner_payload` schema (same keys, defensive defaults when the
    /// project-state dir is empty / never consolidated). `min_hours` /
    /// `min_sessions` are surfaced as `0` to signal «no policy gate».
    pub async fn evaluate_dream_run_now(
        &mut self,
        request: DreamRunNowRequest,
    ) -> Result<RunNowResponse, BoxError> {
        let project_state_dir = project_state_dir_from_memory_dir(&request.memory_dir);
        let last_consolidated_at_ms =
            crate::lock::last_consolidated_at(&request.memory_dir).await?;
        let elapsed_ms = request.now_ms.saturating_sub(last_consolidated_at_ms);
        let hours_since = elapsed_ms / 3_600_000;

        let owner = crate::lock::LockOwner {
            holder_pid: std::process::id(),
        };
        let prior_mtime_ms = match crate::lock::try_acquire_for(
            &request.memory_dir,
            &owner,
            &crate::lock::LockOptions::default(),
        )
        .await?
        {
            Some(prior) => prior,
            None => {
                return Ok(RunNowResponse {
                    triggers: Vec::new(),
                    gate_skip_reason: Some("lock_held".to_owned()),
                });
            }
        };

        // List sessions defensively: surface 0 if scan fails (e.g. project
        // state dir not yet provisioned). The dream runner itself does the
        // real session scan against the memory dir contents.
        let session_ids = crate::dream_gate::list_sessions_touched_since(
            &project_state_dir,
            last_consolidated_at_ms,
            &request.current_session_id,
        )
        .unwrap_or_default();

        let lock_token = crate::dream_gate::build_lock_token(
            &request.memory_dir,
            request.now_ms,
            owner.holder_pid,
        );
        let trigger_id = next_trigger_id("dream", &request.session_id, request.now_ms);

        let pending = PendingRunner {
            trigger_id: trigger_id.clone(),
            kind: "dream".to_owned(),
            session_id: request.session_id.clone(),
            memory_dir: request.memory_dir.clone(),
            project_state_dir: project_state_dir.clone(),
            lock_token: Some(lock_token.clone()),
            prior_mtime_ms: Some(prior_mtime_ms),
            extract_last_assistant_uuid: None,
            extract_total_model_visible: None,
            registered_at_ms: crate::extract_archive::now_ms(),
        };

        let sessions_count = session_ids.len();
        let turn_trigger = TurnEndTrigger {
            trigger_id,
            kind: RunnerKind::Dream,
            lock_token: Some(lock_token),
            runner_payload: json!({
                "memory_dir": request.memory_dir.to_string_lossy(),
                "project_state_dir": project_state_dir.to_string_lossy(),
                "transcript_dir": project_state_dir.to_string_lossy(),
                "session_ids": session_ids,
                "sessions_since_last_consolidation": sessions_count,
                "last_consolidated_at_ms": last_consolidated_at_ms,
                "hours_since_last_consolidation": hours_since,
                "prior_mtime_ms": prior_mtime_ms,
                "min_hours": 0_u64,
                "min_sessions": 0_u64,
                "force": true,
            }),
        };
        // Manual dream is executed inside the orchestrator by
        // `spawn_dream_now`; it is not handed to the TS runner consumer and
        // therefore must not enter the external runner-delivery journal.
        self.results.register(pending);
        Ok(RunNowResponse {
            triggers: vec![turn_trigger],
            gate_skip_reason: None,
        })
    }

    /// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual extract trigger.
    /// Bypasses the extract-cursor sufficiency check (`new_message_count`
    /// vs `min_messages`) and the feature gates (`EXTRACT_MEMORIES` /
    /// `auto_memory_enabled` / `remote_mode`); the user clicked the button
    /// explicitly, so we forge a trigger regardless. The previous cursor
    /// (`state.last_assistant_uuid`) is still surfaced for runner context.
    /// When `last_assistant_uuid` is empty the request yields an empty
    /// trigger list with `gate_skip_reason = "no_assistant_uuid"` so the
    /// caller can short-circuit without throwing.
    pub async fn evaluate_extract_run_now(
        &mut self,
        request: ExtractRunNowRequest,
    ) -> Result<RunNowResponse, BoxError> {
        if request.last_assistant_uuid.is_empty() {
            return Ok(RunNowResponse {
                triggers: Vec::new(),
                gate_skip_reason: Some("no_assistant_uuid".to_owned()),
            });
        }

        let project_state_dir = project_state_dir_from_memory_dir(&request.memory_dir);
        // W-MEMORY-EVOLUTION PR-11 — surface the persisted per-project cursor's
        // last UUID for runner context. The cursor itself is advanced on
        // completion (`ResultListener::handle_completed`), not here, so we only
        // read it (no save needed in this manual bypass path).
        let previous_cursor_uuid = load_extract_cursor(&project_state_dir)
            .last_assistant_uuid
            .clone();
        let new_message_count = request
            .message_counts
            .get("total")
            .copied()
            .or_else(|| request.message_counts.get("assistant").copied())
            .unwrap_or(0);
        let total_model_visible = request.message_counts.get("total").copied();

        let trigger_id = next_trigger_id("extract", &request.session_id, request.now_ms);
        let pending = PendingRunner {
            trigger_id: trigger_id.clone(),
            kind: "extract".to_owned(),
            session_id: request.session_id.clone(),
            memory_dir: request.memory_dir.clone(),
            project_state_dir: project_state_dir.clone(),
            lock_token: None,
            prior_mtime_ms: None,
            extract_last_assistant_uuid: Some(request.last_assistant_uuid.clone()),
            extract_total_model_visible: total_model_visible,
            registered_at_ms: crate::extract_archive::now_ms(),
        };

        let turn_trigger = TurnEndTrigger {
            trigger_id,
            kind: RunnerKind::Extract,
            lock_token: None,
            runner_payload: json!({
                "memory_dir": request.memory_dir.to_string_lossy(),
                "team_memory_dir": request.team_memory_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                "previous_cursor_uuid": previous_cursor_uuid,
                "last_assistant_uuid": request.last_assistant_uuid,
                "new_message_count": new_message_count,
                "team_memory_enabled": false,
                "skip_index": false,
                "force": true,
            }),
        };
        // The retained compatibility endpoint has no executable TS consumer
        // (see `ipc_handler.rs` semantic-seal comment), so this wire-only
        // trigger is not durable runner work.
        self.results.register(pending);
        Ok(RunNowResponse {
            triggers: vec![turn_trigger],
            gate_skip_reason: None,
        })
    }
}

#[derive(Clone, Copy)]
struct RequestedKinds {
    dream: bool,
    extract: bool,
}

fn requested_kinds(kinds: &[RunnerKind]) -> RequestedKinds {
    if kinds.is_empty() {
        return RequestedKinds {
            dream: true,
            extract: true,
        };
    }
    RequestedKinds {
        dream: kinds.contains(&RunnerKind::Dream),
        extract: kinds.contains(&RunnerKind::Extract),
    }
}

fn flag(flags: &BTreeMap<String, bool>, name: &str) -> bool {
    flags.get(name).copied().unwrap_or(false)
}

fn flag_default(flags: &BTreeMap<String, bool>, name: &str, default: bool) -> bool {
    flags.get(name).copied().unwrap_or(default)
}

fn next_trigger_id(kind: &str, _session_id: &str, _now_ms: u64) -> String {
    format!("{kind}:{}", Uuid::new_v4())
}

#[must_use]
pub fn runner_work_key(trigger_id: &str) -> String {
    format!("runner:{trigger_id}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use crate::dream_config::{dream_config_path, write_dream_config, DreamConfig};

    use super::*;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const OTHER_SESSION: &str = "660e8400-e29b-41d4-a716-446655440000";
    const LAST_ASSISTANT_ID: &str = "770e8400-e29b-41d4-a716-446655440000";

    fn request(dir: &TempDir, kinds: Vec<RunnerKind>) -> TurnEndEvaluateRequest {
        let mut counts = BTreeMap::new();
        counts.insert("user".to_owned(), 1);
        counts.insert("assistant".to_owned(), 1);
        let mut flags = BTreeMap::new();
        flags.insert("EXTRACT_MEMORIES".to_owned(), true);
        flags.insert("auto_memory_enabled".to_owned(), true);
        flags.insert("auto_dream_enabled".to_owned(), true);
        TurnEndEvaluateRequest {
            recovery_schema_version: RUNNER_RECOVERY_SCHEMA_VERSION,
            session_id: SESSION_ID.to_owned(),
            current_session_id: SESSION_ID.to_owned(),
            last_assistant_uuid: LAST_ASSISTANT_ID.to_owned(),
            project_cwd: dir.path().to_path_buf(),
            transcript_path: dir.path().join(format!("{SESSION_ID}.jsonl")),
            memory_dir: dir.path().join("memory"),
            team_memory_dir: None,
            message_counts: counts,
            feature_flags: flags,
            requested_kinds: kinds,
            now_ms: 1_700_200_000_000,
        }
    }

    fn write_session(dir: &TempDir, session_id: &str, mtime_s: i64) {
        let path = dir.path().join(format!("{session_id}.jsonl"));
        fs::write(&path, "{}\n").unwrap();
        set_file_mtime(path, FileTime::from_unix_time(mtime_s, 0)).unwrap();
    }

    #[tokio::test]
    async fn turn_evaluator_returns_only_requested_extract_trigger() {
        let dir = TempDir::new().unwrap();
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator
            .evaluate_turn_end(request(&dir, vec![RunnerKind::Extract]))
            .await
            .unwrap();

        assert_eq!(response.triggers.len(), 1);
        assert_eq!(response.triggers[0].kind, RunnerKind::Extract);
        assert_eq!(
            response.triggers[0].runner_payload["last_assistant_uuid"],
            LAST_ASSISTANT_ID
        );
    }

    #[tokio::test]
    async fn turn_evaluator_returns_dream_trigger_with_lock_when_gate_passes() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, OTHER_SESSION, 1_700_100_000);
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
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator
            .evaluate_turn_end(request(&dir, vec![RunnerKind::Dream]))
            .await
            .unwrap();

        assert_eq!(response.triggers.len(), 1);
        assert_eq!(response.triggers[0].kind, RunnerKind::Dream);
        assert!(response.triggers[0].lock_token.is_some());
        assert_eq!(
            response.triggers[0].runner_payload["sessions_since_last_consolidation"],
            1
        );
    }

    #[tokio::test]
    async fn turn_evaluator_stored_dream_disabled_wins_over_stale_ts_flag() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, OTHER_SESSION, 1_700_100_000);
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: false,
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
        let mut req = request(&dir, vec![RunnerKind::Dream]);
        req.feature_flags
            .insert("auto_dream_enabled".to_owned(), true);
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator.evaluate_turn_end(req).await.unwrap();

        assert!(response.triggers.is_empty());
    }

    #[tokio::test]
    async fn turn_evaluator_invalid_stored_dream_config_does_not_enable_stale_ts_flag() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, OTHER_SESSION, 1_700_100_000);
        let path = dream_config_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not-json").unwrap();
        let mut req = request(&dir, vec![RunnerKind::Dream]);
        req.feature_flags
            .insert("auto_dream_enabled".to_owned(), true);
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator.evaluate_turn_end(req).await.unwrap();

        assert!(response.triggers.is_empty());
    }

    #[tokio::test]
    async fn turn_evaluator_does_not_return_dream_when_kairos_is_active() {
        let dir = TempDir::new().unwrap();
        write_session(&dir, OTHER_SESSION, 1_700_100_000);
        let mut req = request(&dir, vec![RunnerKind::Dream]);
        req.feature_flags.insert("KAIROS".to_owned(), true);
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator.evaluate_turn_end(req).await.unwrap();

        assert!(response.triggers.is_empty());
    }

    #[tokio::test]
    async fn turn_evaluator_registers_pending_trigger_for_completion() {
        let dir = TempDir::new().unwrap();
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator
            .evaluate_turn_end(request(&dir, vec![RunnerKind::Extract]))
            .await
            .unwrap();

        assert_eq!(response.triggers.len(), 1);
        assert_eq!(evaluator.results.pending_len(), 1);
    }

    #[tokio::test]
    async fn durable_runner_work_persists_exact_versioned_recovery_locator() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let mut evaluator = TurnEvaluator::with_journal(Arc::clone(&journal));
        let request = request(&dir, vec![RunnerKind::Extract]);
        let expected_project_cwd = request.project_cwd.clone();
        let expected_transcript_path = request.transcript_path.clone();
        let expected_memory_dir = request.memory_dir.clone();

        let response = evaluator.evaluate_turn_end(request).await.unwrap();
        let trigger = &response.triggers[0];
        let row = journal
            .get(&runner_work_key(&trigger.trigger_id))
            .unwrap()
            .unwrap();
        let work: DurableRunnerWork = serde_json::from_value(row.payload).unwrap();
        work.validate().unwrap();
        assert_eq!(
            work.recovery,
            RunnerRecoveryLocator {
                recovery_schema_version: RUNNER_RECOVERY_SCHEMA_VERSION,
                trigger_id: trigger.trigger_id.clone(),
                kind: RunnerKind::Extract,
                session_id: SESSION_ID.to_owned(),
                current_session_id: SESSION_ID.to_owned(),
                context_leaf_uuid: LAST_ASSISTANT_ID.to_owned(),
                project_cwd: expected_project_cwd,
                transcript_path: expected_transcript_path,
                project_state_dir: dir.path().to_path_buf(),
                memory_dir: expected_memory_dir,
            }
        );
    }

    #[tokio::test]
    async fn invalid_recovery_authority_is_rejected_before_gate_or_cursor_mutation() {
        let dir = TempDir::new().unwrap();

        let mut wrong_schema = request(&dir, vec![RunnerKind::Extract]);
        wrong_schema.recovery_schema_version = RUNNER_RECOVERY_SCHEMA_VERSION + 1;
        assert!(TurnEvaluator::new()
            .evaluate_turn_end(wrong_schema)
            .await
            .is_err());

        let mut invalid_uuid = request(&dir, vec![RunnerKind::Extract]);
        invalid_uuid.last_assistant_uuid = "not-a-uuid".to_owned();
        assert!(TurnEvaluator::new()
            .evaluate_turn_end(invalid_uuid)
            .await
            .is_err());

        let mut relative_cwd = request(&dir, vec![RunnerKind::Extract]);
        relative_cwd.project_cwd = PathBuf::from("relative/project");
        assert!(TurnEvaluator::new()
            .evaluate_turn_end(relative_cwd)
            .await
            .is_err());

        let mut wrong_transcript = request(&dir, vec![RunnerKind::Extract]);
        wrong_transcript.transcript_path = dir.path().join("different-session.jsonl");
        assert!(TurnEvaluator::new()
            .evaluate_turn_end(wrong_transcript)
            .await
            .is_err());

        let mut traversing_memory = request(&dir, vec![RunnerKind::Extract]);
        traversing_memory.memory_dir = dir.path().join("nested/../memory");
        assert!(TurnEvaluator::new()
            .evaluate_turn_end(traversing_memory)
            .await
            .is_err());

        assert!(
            !dir.path().join("memory/.consolidate-lock").exists(),
            "invalid recovery input must be rejected before a dream lock"
        );
        let cursor_path = crate::extract_cursor::extract_cursor_path(dir.path());
        assert!(
            !cursor_path.exists(),
            "invalid recovery input must be rejected before cursor persistence"
        );
    }

    #[tokio::test]
    async fn durable_work_validation_detects_cross_field_locator_tampering() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let mut evaluator = TurnEvaluator::with_journal(Arc::clone(&journal));
        let response = evaluator
            .evaluate_turn_end(request(&dir, vec![RunnerKind::Extract]))
            .await
            .unwrap();
        let row = journal
            .get(&runner_work_key(&response.triggers[0].trigger_id))
            .unwrap()
            .unwrap();
        let mut work: DurableRunnerWork = serde_json::from_value(row.payload).unwrap();

        work.recovery.context_leaf_uuid = OTHER_SESSION.to_owned();
        assert!(
            work.validate().is_err(),
            "a valid but different UUID must not pass cross-field consistency"
        );
    }

    #[tokio::test]
    async fn turn_evaluator_direct_memory_write_advances_cursor_without_trigger() {
        let dir = TempDir::new().unwrap();
        let mut req = request(&dir, vec![RunnerKind::Extract]);
        req.feature_flags
            .insert("memory_write_since_cursor".to_owned(), true);
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator.evaluate_turn_end(req).await.unwrap();

        assert!(response.triggers.is_empty());
        // W-MEMORY-EVOLUTION PR-11 — cursor advance is now persisted to disk
        // under the per-project derived root, not held in an in-memory field.
        let psd = project_state_dir_from_memory_dir(&dir.path().join("memory"));
        let cursor = crate::extract_cursor::load_extract_cursor(&psd);
        assert_eq!(
            cursor.last_assistant_uuid.as_deref(),
            Some(LAST_ASSISTANT_ID)
        );
    }

    /// W-MEMORY-EVOLUTION PR-11 — restart-duplicate-extraction fix (bug1).
    /// A Run decision persists the cursor window and the matching
    /// `memory.runner.completed` advances `last_total_model_visible`. A fresh
    /// `TurnEvaluator` (simulating a process restart) must load that persisted
    /// cursor and *not* re-extract the same already-consumed window.
    ///
    /// 2026-07-27：在飞标记已退役（§27.3-G8），所以这里只断言**窗口位置**
    /// 被持久化 —— 进程重启后放行由"新进程的 pending 表是空的"天然保证，
    /// 不再需要任何磁盘标记与它的重置逻辑。
    #[tokio::test]
    async fn turn_evaluator_persisted_cursor_prevents_restart_duplicate_extraction() {
        use crate::result_listener::{PendingRunner, RunnerCompleted};

        let dir = TempDir::new().unwrap();
        let psd = project_state_dir_from_memory_dir(&dir.path().join("memory"));

        // Turn 1: evaluate → Run (cursor persisted with in_progress=true).
        let mut evaluator = TurnEvaluator::new();
        let response = evaluator
            .evaluate_turn_end(request(&dir, vec![RunnerKind::Extract]))
            .await
            .unwrap();
        assert_eq!(response.triggers.len(), 1, "first turn should Run");
        let trigger_id = response.triggers[0].trigger_id.clone();
        // Run 决策把窗口计数落盘；在飞占位只存在于进程内的 pending 表。
        let persisted = crate::extract_cursor::load_extract_cursor(&psd);
        assert_eq!(persisted.turns_since_last_extraction, 1);
        assert!(
            evaluator.results.has_in_flight("extract", &psd),
            "Run 之后同项目必须处于在飞态"
        );

        // Settle the runner: completion advances last_total_model_visible to
        // the window total (user=1 + assistant=1 = 2).
        evaluator.results.register(PendingRunner {
            trigger_id: trigger_id.clone(),
            kind: "extract".to_owned(),
            session_id: SESSION_ID.to_owned(),
            memory_dir: dir.path().join("memory"),
            project_state_dir: psd.clone(),
            lock_token: None,
            prior_mtime_ms: None,
            extract_last_assistant_uuid: Some(LAST_ASSISTANT_ID.to_owned()),
            extract_total_model_visible: Some(2),
            registered_at_ms: 0,
        });
        evaluator
            .results
            .handle_completed(RunnerCompleted {
                trigger_id,
                kind: "extract".to_owned(),
                written_paths: vec![],
                usage: None,
                error: None,
                completed_at_ms: None,
            })
            .await
            .unwrap();
        let after_complete = crate::extract_cursor::load_extract_cursor(&psd);
        assert_eq!(after_complete.last_total_model_visible, 2);
        assert_eq!(
            after_complete.last_assistant_uuid.as_deref(),
            Some(LAST_ASSISTANT_ID)
        );

        // RESTART: brand-new evaluator (no in-memory cursor) replays the same
        // window. The persisted cursor means there are no new messages, so it
        // must NOT re-extract.
        let mut restarted = TurnEvaluator::new();
        let response2 = restarted
            .evaluate_turn_end(request(&dir, vec![RunnerKind::Extract]))
            .await
            .unwrap();
        assert!(
            response2.triggers.is_empty(),
            "after restart the persisted cursor must suppress duplicate extraction",
        );
    }

    #[tokio::test]
    async fn turn_evaluator_never_mentions_rust_to_ts_dream_run_method() {
        let source = include_str!("turn_evaluator.rs");
        // P1.4 intentionally introduces `dream.run_now` / `extract.run_now`
        // method names. The original v6 gate banned the bare token
        // `memory.<kind>.run` (TS→Rust direct invoke, removed by the
        // rebuild). Build the banned token via concat so this assertion's
        // own source line does not trip the gate. Match the bare token
        // followed by `"` (JSON value boundary) — that is the wire-shape
        // the rebuild explicitly removed.
        let dream_banned = format!("memory.{}.run{}", "dream", "\"");
        let extract_banned = format!("memory.{}.run{}", "extract", "\"");
        assert!(!source.contains(&dream_banned));
        assert!(!source.contains(&extract_banned));
    }

    // ──────────────────────────────────────────────────────────────────
    // W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual «Run Now» tests.
    // ──────────────────────────────────────────────────────────────────

    fn dream_run_now_request(dir: &TempDir) -> DreamRunNowRequest {
        DreamRunNowRequest {
            session_id: SESSION_ID.to_owned(),
            current_session_id: SESSION_ID.to_owned(),
            memory_dir: dir.path().join("memory"),
            now_ms: 1_700_200_000_000,
        }
    }

    fn extract_run_now_request(dir: &TempDir) -> ExtractRunNowRequest {
        let mut counts = BTreeMap::new();
        counts.insert("user".to_owned(), 1);
        counts.insert("assistant".to_owned(), 1);
        counts.insert("total".to_owned(), 2);
        ExtractRunNowRequest {
            session_id: SESSION_ID.to_owned(),
            last_assistant_uuid: LAST_ASSISTANT_ID.to_owned(),
            memory_dir: dir.path().join("memory"),
            team_memory_dir: None,
            message_counts: counts,
            now_ms: 1_700_200_000_000,
        }
    }

    /// P1.4 dream run_now bypasses KAIROS / auto_memory_enabled / min_hours
    /// / min_sessions gates: the request always returns a trigger when the
    /// consolidation lock can be acquired.
    #[tokio::test]
    async fn evaluate_dream_run_now_bypasses_gates_and_returns_trigger() {
        let dir = TempDir::new().unwrap();
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator
            .evaluate_dream_run_now(dream_run_now_request(&dir))
            .await
            .unwrap();

        assert_eq!(response.triggers.len(), 1);
        assert_eq!(response.triggers[0].kind, RunnerKind::Dream);
        assert!(response.triggers[0].lock_token.is_some());
        assert_eq!(response.gate_skip_reason, None);
        // payload should mark force=true so the runner knows it's manual
        assert_eq!(response.triggers[0].runner_payload["force"], true);
        // pending runner registered (so the matching memory.runner.completed
        // settles the lock + advances cursor)
        assert_eq!(evaluator.results.pending_len(), 1);
    }

    /// P1.4 dream run_now reports lock_held when an automatic / manual run
    /// is already holding the consolidation lock. Subsequent call surfaces
    /// `gate_skip_reason = "lock_held"` rather than throwing.
    #[tokio::test]
    async fn evaluate_dream_run_now_returns_lock_held_when_lock_busy() {
        let dir = TempDir::new().unwrap();
        // Pre-acquire the lock with a different evaluator instance to
        // simulate a concurrent run in progress.
        let mut blocker = TurnEvaluator::new();
        let first = blocker
            .evaluate_dream_run_now(dream_run_now_request(&dir))
            .await
            .unwrap();
        assert_eq!(first.triggers.len(), 1);

        let mut evaluator = TurnEvaluator::new();
        let response = evaluator
            .evaluate_dream_run_now(dream_run_now_request(&dir))
            .await
            .unwrap();
        assert!(response.triggers.is_empty());
        assert_eq!(response.gate_skip_reason.as_deref(), Some("lock_held"));
    }

    /// P1.4 extract run_now bypasses cursor sufficiency + feature gates.
    /// A non-empty `last_assistant_uuid` always yields a single trigger.
    #[tokio::test]
    async fn evaluate_extract_run_now_bypasses_gates_and_returns_trigger() {
        let dir = TempDir::new().unwrap();
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator
            .evaluate_extract_run_now(extract_run_now_request(&dir))
            .await
            .unwrap();

        assert_eq!(response.triggers.len(), 1);
        assert_eq!(response.triggers[0].kind, RunnerKind::Extract);
        assert!(response.triggers[0].lock_token.is_none());
        assert_eq!(response.gate_skip_reason, None);
        assert_eq!(
            response.triggers[0].runner_payload["last_assistant_uuid"],
            LAST_ASSISTANT_ID
        );
        assert_eq!(response.triggers[0].runner_payload["force"], true);
        assert_eq!(evaluator.results.pending_len(), 1);
    }

    /// P1.4 extract run_now short-circuits with `gate_skip_reason =
    /// "no_assistant_uuid"` when the caller supplied an empty UUID. No
    /// trigger registered, no pending runner accumulated.
    #[tokio::test]
    async fn evaluate_extract_run_now_returns_no_assistant_uuid_short_circuit() {
        let dir = TempDir::new().unwrap();
        let mut request = extract_run_now_request(&dir);
        request.last_assistant_uuid = String::new();
        let mut evaluator = TurnEvaluator::new();

        let response = evaluator.evaluate_extract_run_now(request).await.unwrap();

        assert!(response.triggers.is_empty());
        assert_eq!(
            response.gate_skip_reason.as_deref(),
            Some("no_assistant_uuid")
        );
        assert_eq!(evaluator.results.pending_len(), 0);
    }
}
