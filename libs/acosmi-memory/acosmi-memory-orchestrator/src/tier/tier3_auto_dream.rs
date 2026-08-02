//! W-MEMORY-DREAM-REBUILD v7 P3.4 — Tier-3 AutoDream policy（Rust 重写）。
//!
//! 设计借鉴 CrabClaw `backend/internal/memory/uhms/tier3_policy.go` +
//! `tier3_store.go`（只读参考归档，不 git copy；重写为 Rust）。配合 P3.1 立
//! 的反向 IPC LLM 调用契约：orchestrator 通过 `memory/tier/llmCallRequest`
//! notification 广播请求 → TS 端跑 SDK → `memory/tier/llmCallResult` request
//! 回写 → orchestrator 内部 pending oneshot 按 `req_id` 匹配（详
//! `tier/mod.rs` 反向 IPC 时序图）。
//!
//! # 4 重门控（Tier-3 deterministic policy）
//!
//! 1. **时间门控** —— 自上次 dream consolidation 起 >= 24h。读 lock 文件
//!    mtime（`acosmi_memory_orchestrator::lock::last_consolidated_at`）作真源。
//! 2. **扫描节流** —— 自上次会话扫描起 >= 10min（per-instance in-memory
//!    state）；防 turn-end 高频触发抢占 readdir + stat。
//! 3. **会话门控** —— 自上次 dream 后被 touch 的会话数 >= 5（默认；
//!    DreamConfig.min_sessions 可覆写）。
//! 4. **锁门控** —— PID 锁（acquire `.consolidate-lock` via
//!    `acosmi_memory_orchestrator::lock::try_acquire_for`）。
//!
//! 触发条件最终式：4 重门控全部通过 → trigger；任一失败 → skip + reason。
//!
//! # 5 phase pipeline
//!
//! 本 PR 实施 Phase 0-4（**不含 Phase 5 Imagination**，留 P3.5 范围）：
//!
//! - **Phase 0 反思（Self-RAG）** —— LLM 自我评估近期 dreams/insight_*.md
//!   是否仍 valid（hallucination check）。LLM 标记为陈旧的入参文件被列出，
//!   交由 Phase 4 Prune 清理。
//! - **Phase 1 Orient** —— LLM 扫描近期会话 + memdir 主区，定位 theme
//!   （recent topics 聚类）。
//! - **Phase 2 Gather** —— LLM 基于 theme 聚合 evidence_refs（具体的 session
//!   text / memory snippet）。
//! - **Phase 3 Consolidate** —— LLM 综合 evidence → 生成 insight markdown
//!   （含 frontmatter type/name/description）→ 写入 `dreams/insight_*.md`。
//!   弱信号片段（如未达 evidence_refs 阈值的 sub-insight）写入
//!   `dreams/fragment_*.md`。
//! - **Phase 4 Prune** —— 清理 dreams/ 下 fragment_*.md 中明确被 Phase 0
//!   标为陈旧的 fragment；并对老旧 fragment (mtime > 30d) 兜底清理。
//!   **不实施** promotion fragment → memdir 主区（留 P5.3 范围）。
//!
//! # 与 Tier-1/Tier-2 的共用 stack
//!
//! Tier-3 复用 P3.1 反向 IPC LLM 调用契约 + `LlmCallEmitter` trait +
//! per-processor `pending` HashMap 模式。`req_id` 前缀 `tier3-`（与
//! `tier1-` / `tier2-` 隔离，dispatcher 投递 `memory.tier.llm_call_result`
//! 时 triple-deliver，按前缀匹配 pending oneshot）。
//!
//! # 写盘
//!
//! 走 `atomic_write::atomic_write`（与 Tier-1/Tier-2 一致）。dreams/ 子目录
//! 在 memory_dir 下创建（`memory_dir/dreams/`）；isolation 体现在路径而非
//! VFS 隔离（vfs FileSystem trait 未 plumb，沿用 pragmatic atomic_write 路径）。
//!
//! # 双轨期
//!
//! 现有 `dream_gate.rs` + `result_listener.rs` 简单 gate 逻辑保留作 Phase 0
//! POC 兼容层（CLAUDE.md §硬约束 #15）；本 PR **不动** 这两个模块，只新增
//! tier3 policy 模块旁路并存。P6.1 真切换由后续 PR 完成。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, AtomicU8, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, Mutex};

use crate::atomic_write::{atomic_write, BoxError};
use crate::lock;
use crate::tier::{
    GateDecision, LlmCallParams, LlmCallRequestPayload, LlmCallResultPayload, LlmMessage, LlmUsage,
    MemoryTier, TierGate,
};

// ──────────────────────────────────────────────────────────────────────────
// 阈值常量（CrabClaw 同源真值；Rust 端独立维护以便迭代）
// ──────────────────────────────────────────────────────────────────────────

/// gate 1 — 最小 dream 间隔（小时）。CrabClaw `ResolvedMinHours = 24`。
pub const DEFAULT_MIN_HOURS: u64 = 24;

/// gate 2 — 会话扫描节流间隔（ms）。CrabClaw `sessionScanInterval =
/// 10 min = 600_000 ms`。
pub const SESSION_SCAN_INTERVAL_MS: u64 = 10 * 60 * 1000;

/// gate 3 — 触发 dream 所需的最小新会话数。CrabClaw `ResolvedMinSessions = 5`。
pub const DEFAULT_MIN_SESSIONS: u32 = 5;

/// LLM 调用反向 IPC 等待超时（每 phase 独立 60s；dream 内容多于 extract，
/// 留更宽容的超时）。
pub const LLM_CALL_TIMEOUT_MS: u64 = 60_000;

/// fragment 老化阈值（ms）。Phase 4 Prune 兜底清理超过此 mtime 的 fragment。
/// 默认 30 天，给 Phase 0 / 用户人工审核留足缓冲。
pub const FRAGMENT_STALE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

// ──────────────────────────────────────────────────────────────────────────
// Tier-3 prompt 模板（**Rust 字符串常量，不进系统 prompt 体系**）
//
// CLAUDE.md §硬约束 #15 第 8 条铁律：Tier1/2/3 prompt 模板在 orchestrator
// 内嵌 Rust 字符串常量，**不**改动 5 个严格档（用户裁决）。设计借鉴
// CrabClaw `prompts/tier3_dream_consolidation.md`，重写为全英文、不 paste
// 中文、不写品牌字面（无 LLM 模型名 / family / 价格 hardcode；§硬约束 #1）。
// ──────────────────────────────────────────────────────────────────────────

/// Phase 0 — Self-RAG reflection over the prior dream's insights.
///
/// Placeholders:
/// - `{{prior_insights}}` — newline-separated list of `dreams/insight_*.md`
///   bodies (capped; processor truncates per call).
pub const TIER3_DREAM_PHASE0_PROMPT: &str = r#"You are the dream-consolidation reflection agent. Your job is to evaluate which of the prior dream insights are still valid given recent activity and which have become stale or contradictory.

# Prior insights to evaluate

{{prior_insights}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "still_valid_ids": ["insight_id_1", ...],
  "stale_ids": ["insight_id_2", ...],
  "notes": "<one-line summary of your reasoning>"
}

Rules:
- IDs are the filename stems of the insight files (no `.md`, no `insight_` prefix is stripped — keep them verbatim).
- If you cannot evaluate (e.g. zero prior insights), return both arrays empty.
- Do NOT hallucinate IDs that are not in the input."#;

/// Phase 1 — Orient: cluster recent themes from sessions + memdir.
///
/// Placeholders:
/// - `{{session_summary}}` — newline-separated session digest lines.
/// - `{{memdir_summary}}` — newline-separated existing memory manifest.
pub const TIER3_DREAM_PHASE1_ORIENT_PROMPT: &str = r#"You are the dream-consolidation orientation agent. Identify the top recurring themes from recent activity that warrant deeper consolidation.

# Recent sessions

{{session_summary}}

# Existing memdir manifest

{{memdir_summary}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "themes": [
    {"id": "<snake_case_theme_id>", "label": "<one-line label>", "rationale": "<one-line rationale>"}
  ]
}

Rules:
- Emit 1 to 5 themes. Prefer fewer high-quality themes over many weak ones.
- Theme IDs must be unique snake_case; reuse a memdir name only if the theme is a direct continuation.
- If nothing in recent activity warrants a theme, return an empty `themes` array."#;

/// Phase 2 — Gather: for each theme, collect supporting evidence refs.
///
/// Placeholders:
/// - `{{theme_id}}` — the snake_case theme id from Phase 1.
/// - `{{theme_label}}` — the human-readable label.
/// - `{{session_excerpts}}` — newline-separated session excerpts.
pub const TIER3_DREAM_PHASE2_GATHER_PROMPT: &str = r#"You are the dream-consolidation evidence gathering agent. For the given theme, collect the supporting evidence from recent activity.

# Theme

ID: {{theme_id}}
Label: {{theme_label}}

# Session excerpts to mine

{{session_excerpts}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "evidence_refs": [
    {"source": "<session-id-or-memory-file>", "snippet": "<verbatim quote under 200 chars>", "weight": <0..1>}
  ]
}

Rules:
- Emit 0 to 8 evidence refs. Each must be a verbatim quote, not a paraphrase.
- `weight` is the strength of evidence (0=weak, 1=strong); weights below 0.4 mark the theme as "fragment" candidate.
- If no evidence supports the theme, return an empty array — Phase 3 will skip this theme."#;

/// Phase 3 — Consolidate: synthesize an insight markdown from theme + evidence.
///
/// Placeholders:
/// - `{{theme_id}}` — theme id.
/// - `{{theme_label}}` — theme label.
/// - `{{evidence_list}}` — newline-separated evidence snippets.
pub const TIER3_DREAM_PHASE3_CONSOLIDATE_PROMPT: &str = r#"You are the dream-consolidation synthesis agent. Produce a structured insight markdown for the given theme + evidence.

# Theme

ID: {{theme_id}}
Label: {{theme_label}}

# Evidence

{{evidence_list}}

# Output format

Emit EXACTLY one memory block in this form (no surrounding markdown fences):

---
name: {{theme_id}}
type: insight
description: <one-line description under 150 chars>
confidence: <low|medium|high>
---

<full body — durable, multi-paragraph synthesis. Cite the evidence inline by source where relevant.>

Rules:
- Confidence is `high` when evidence weights average >= 0.7; `medium` between 0.4 and 0.7; `low` below 0.4 (which means fragment-class).
- Do NOT include hallucinated facts not present in the evidence.
- If evidence is empty or contradictory, emit a single short block with confidence: low and a body explaining the uncertainty."#;

/// Phase 4 — Prune: select fragments to delete based on Phase 0 reflection.
///
/// Placeholders:
/// - `{{stale_ids}}` — JSON array of stale insight IDs from Phase 0.
/// - `{{fragment_list}}` — newline-separated current fragment files.
pub const TIER3_DREAM_PHASE4_PRUNE_PROMPT: &str = r#"You are the dream-consolidation pruning agent. Select which fragment files should be deleted.

# Stale insight IDs (from Phase 0)

{{stale_ids}}

# Current fragments

{{fragment_list}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "delete_ids": ["fragment_id_1", ...]
}

Rules:
- IDs are filename stems (no `.md`).
- Only mark for deletion fragments that are (a) on the stale_ids list, OR (b) clearly superseded by a newer insight.
- If nothing should be deleted, return an empty array."#;

// ──────────────────────────────────────────────────────────────────────────
// Gate input / output
// ──────────────────────────────────────────────────────────────────────────

/// Gate input — passed from TS-side per-query/per-day trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AutoDreamGateInput {
    /// Absolute path to memory_dir (where `.consolidate-lock` lives).
    pub memory_dir: PathBuf,
    /// Touched-since-last-dream session count. Caller is responsible for
    /// computing this (TS side reads session files mtime); orchestrator
    /// only enforces the threshold.
    pub touched_session_count: u32,
    /// Per-instance forced flag — when `true`, skips time + session + scan
    /// gates (still honors the PID lock unless `forced_skip_lock` set).
    #[serde(default)]
    pub forced: bool,
    /// Skip the PID lock check (manual `/dream` invocations only). Default
    /// false — always honor the lock.
    #[serde(default)]
    pub forced_skip_lock: bool,
    /// Optional override of `DEFAULT_MIN_HOURS` (cluster-tuned dream
    /// interval). 0 = use default.
    #[serde(default)]
    pub min_hours_override: u64,
    /// W-MEMORY-SYNERGY W6 (2026-07-16, 6c) — 重要性积分压力：true = 未固化
    /// 记忆的 importance 积分已过阈值（`importance_pressure` 模块），时间门
    /// （gate 1）豁免本次评估；会话数门 / 扫描节流 / 锁门**不放宽**。纯
    /// 定时节律对高活跃期反应迟钝，这是它的事件驱动对偶（Park et al.）。
    #[serde(default)]
    pub importance_pressure: bool,
    /// Optional override of `DEFAULT_MIN_SESSIONS` (cluster-tuned threshold).
    /// 0 = use default.
    #[serde(default)]
    pub min_sessions_override: u32,
    /// Optional gate-instance key. Lets callers maintain per-project state
    /// for the scan-throttle gate. Empty string falls back to a process-wide
    /// singleton (most common path).
    #[serde(default)]
    pub instance_key: String,
}

/// Gate output — propagated to `DreamProcessor::process`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AutoDreamGateOutput {
    /// Lock token (path + PID). Processor must surrender the lock when done.
    pub lock_path: PathBuf,
    /// Lock holder PID (== current process pid).
    pub holder_pid: u32,
    /// Prior consolidation mtime_ms (0 = first-ever dream). Used by the
    /// processor for rollback-on-failure semantics.
    pub prior_mtime_ms: u64,
    /// Touched session count captured at gate-pass time. Diagnostic only.
    pub touched_session_count_at_trigger: u32,
}

/// Tier-3 gate evaluation error.
#[derive(Debug, thiserror::Error)]
pub enum AutoDreamGateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("lock acquire error: {0}")]
    LockAcquire(String),
}

// ──────────────────────────────────────────────────────────────────────────
// Per-instance gate state (scan-throttle + last-dream tracking)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct AutoDreamInstanceState {
    /// gate 2 — last session-scan timestamp (ms since epoch). 0 = never.
    last_session_scan_at_ms: AtomicU64,
    /// flag: dream currently in progress (0 = idle, 1 = busy). Process-level
    /// CAS to serialize concurrent triggers within a single orchestrator.
    /// (Cross-process serialization is via the PID lock.)
    dream_in_progress: AtomicU8,
}

// ──────────────────────────────────────────────────────────────────────────
// AutoDreamGate (TierGate trait impl)
// ──────────────────────────────────────────────────────────────────────────

/// Deterministic Tier-3 gate. Reads lock mtime + a per-instance state map;
/// performs PID-lock acquire on the success path. No LLM calls here.
pub struct AutoDreamGate {
    instances: Arc<Mutex<BTreeMap<String, Arc<AutoDreamInstanceState>>>>,
}

impl Default for AutoDreamGate {
    fn default() -> Self {
        Self {
            instances: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl AutoDreamGate {
    pub fn new() -> Self {
        Self::default()
    }

    async fn get_or_init_state(&self, key: &str) -> Arc<AutoDreamInstanceState> {
        let mut map = self.instances.lock().await;
        if let Some(existing) = map.get(key) {
            return Arc::clone(existing);
        }
        let state = Arc::new(AutoDreamInstanceState::default());
        map.insert(key.to_string(), Arc::clone(&state));
        state
    }

    fn resolve_instance_key(input: &AutoDreamGateInput) -> String {
        if input.instance_key.is_empty() {
            input.memory_dir.to_string_lossy().to_string()
        } else {
            input.instance_key.clone()
        }
    }
}

#[async_trait]
impl TierGate for AutoDreamGate {
    type GateInput = AutoDreamGateInput;
    type GateOutput = AutoDreamGateOutput;
    type Error = AutoDreamGateError;

    async fn evaluate_gate(
        &self,
        input: Self::GateInput,
    ) -> Result<GateDecision<Self::GateOutput>, Self::Error> {
        let key = Self::resolve_instance_key(&input);
        let state = self.get_or_init_state(&key).await;

        // Process-level concurrency guard: if a dream is already in progress
        // within this orchestrator, skip immediately.
        if state.dream_in_progress.load(Ordering::Acquire) != 0 {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("dream_in_progress".to_string()),
            });
        }

        let min_hours = if input.min_hours_override > 0 {
            input.min_hours_override
        } else {
            DEFAULT_MIN_HOURS
        };
        let min_sessions = if input.min_sessions_override > 0 {
            input.min_sessions_override
        } else {
            DEFAULT_MIN_SESSIONS
        };

        // Read lock mtime as the "last consolidated at" SoT (mirrors
        // CrabClaw + the existing dream_gate.rs convention).
        let prior_mtime_ms = lock::last_consolidated_at(&input.memory_dir)
            .await
            .map_err(|e| AutoDreamGateError::LockAcquire(e.to_string()))?;
        let now = now_ms();

        if !input.forced {
            // ── Gate 1: 时间门控 ──
            // W6 (6c)：importance_pressure = 重要性积分过阈值 → 时间门豁免
            // （事件驱动提前做梦）；其余 gate 照常。
            let elapsed_ms = now.saturating_sub(prior_mtime_ms);
            let min_interval_ms = min_hours.saturating_mul(60 * 60 * 1000);
            if prior_mtime_ms != 0 && elapsed_ms < min_interval_ms && !input.importance_pressure {
                return Ok(GateDecision {
                    should_trigger: false,
                    payload: None,
                    skip_reason: Some("time_gate_unmet".to_string()),
                });
            }

            // ── Gate 2: 扫描节流 ──
            let last_scan_ms = state.last_session_scan_at_ms.load(Ordering::Acquire);
            if last_scan_ms != 0 && now.saturating_sub(last_scan_ms) < SESSION_SCAN_INTERVAL_MS {
                return Ok(GateDecision {
                    should_trigger: false,
                    payload: None,
                    skip_reason: Some("scan_throttled".to_string()),
                });
            }
            // Stamp the scan timestamp eagerly — even if subsequent gates
            // fail, we don't want a tight retry to hammer readdir.
            state.last_session_scan_at_ms.store(now, Ordering::Release);

            // ── Gate 3: 会话门控 ──
            if input.touched_session_count < min_sessions {
                return Ok(GateDecision {
                    should_trigger: false,
                    payload: None,
                    skip_reason: Some("session_count_unmet".to_string()),
                });
            }
        }

        // ── Gate 4: PID 锁 ──
        let lock_path = lock::lock_path(&input.memory_dir);
        let holder_pid = std::process::id();

        let (lock_mtime_observed, acquired) = if input.forced && input.forced_skip_lock {
            // Forced + skip-lock: treat as a virtual acquire — the caller is
            // responsible for not interleaving with another dream.
            (prior_mtime_ms, true)
        } else {
            let owner = lock::LockOwner { holder_pid };
            let options = lock::LockOptions::default();
            match lock::try_acquire_for(&input.memory_dir, &owner, &options).await {
                Ok(Some(prior)) => (prior, true),
                Ok(None) => (prior_mtime_ms, false),
                Err(e) => {
                    return Err(AutoDreamGateError::LockAcquire(e.to_string()));
                }
            }
        };

        if !acquired {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("lock_held".to_string()),
            });
        }

        // Mark in-progress for the duration of the processor run. Processor's
        // Drop guard clears this on settle.
        state.dream_in_progress.store(1, Ordering::Release);

        Ok(GateDecision {
            should_trigger: true,
            payload: Some(AutoDreamGateOutput {
                lock_path,
                holder_pid,
                prior_mtime_ms: lock_mtime_observed,
                touched_session_count_at_trigger: input.touched_session_count,
            }),
            skip_reason: None,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// DreamProcessor (reverse IPC LLM driver + 5-phase pipeline)
// ──────────────────────────────────────────────────────────────────────────

/// LLM emitter trait — abstracted so tests can inject an in-memory mock.
/// Mirrors the Tier-1 / Tier-2 emitter trait shape so production wiring
/// can install a single broadcast emitter that satisfies all three Tier
/// processors via `Arc<dyn LlmCallEmitter>`.
#[async_trait]
pub trait LlmCallEmitter: Send + Sync {
    async fn emit_request(&self, request: LlmCallRequestPayload);
}

/// In-memory mock emitter (recording). Mirrors the Tier-1/Tier-2
/// RecordingEmitter shape.
#[derive(Debug, Default, Clone)]
pub struct RecordingEmitter {
    inner: Arc<Mutex<Vec<LlmCallRequestPayload>>>,
}

impl RecordingEmitter {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn recorded(&self) -> Vec<LlmCallRequestPayload> {
        self.inner.lock().await.clone()
    }
}

#[async_trait]
impl LlmCallEmitter for RecordingEmitter {
    async fn emit_request(&self, request: LlmCallRequestPayload) {
        self.inner.lock().await.push(request);
    }
}

/// One dream-consolidation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DreamProcessInput {
    /// Absolute path to memory_dir. Used to derive the `dreams/` subdir.
    pub memory_dir: PathBuf,
    /// Gate payload from the preceding `evaluate_gate` call.
    pub gate_payload: AutoDreamGateOutput,
    /// Newline-separated digest of recent sessions (TS side built; orchestrator
    /// doesn't read session files directly — it relies on caller for the
    /// summary).
    pub recent_sessions_summary: String,
    /// Newline-separated memdir manifest (existing user/feedback/project/
    /// reference files). Empty if memdir is empty.
    pub memdir_manifest: String,
    /// Optional model hint (TS side decides actual model selection; not a
    /// brand literal — §硬约束 #1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    /// Sampling parameters (None = TS uses SDK defaults).
    #[serde(default)]
    pub params: LlmCallParams,
    /// Optional instance key (same semantics as the gate). Empty = derive
    /// from memory_dir.
    #[serde(default)]
    pub instance_key: String,
    /// W-MEMORY-SELF-EVOLVE-DGM G3-a (2026-07-16) — 本轮语料实际消费到的
    /// 会话 mtime 水位线（`DreamCorpus::consumed_watermark_ms`）。成功 settle
    /// 时锁 mtime 盖到该值而非 now，撞帽未消费的积压留给下轮。`None` =
    /// 语料无会话或调用方不关心（如 watch 定向做梦）→ fresh mtime（旧语义）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_watermark_ms: Option<u64>,
}

/// One dream-consolidation result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DreamProcessOutput {
    /// Phase ids covered (always `["phase0", "phase1", "phase2", "phase3", "phase4"]`
    /// on success; partial on early-exit phases).
    pub phases_completed: Vec<String>,
    /// Paths of `dreams/insight_*.md` written.
    pub insight_paths: Vec<PathBuf>,
    /// Paths of `dreams/fragment_*.md` written.
    pub fragment_paths: Vec<PathBuf>,
    /// Paths of fragments deleted by Phase 4 Prune.
    pub pruned_paths: Vec<PathBuf>,
    /// Total LLM `req_id`s issued during the run (one per phase that ran).
    pub req_ids: Vec<String>,
    /// Aggregate LLM usage across all phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_usage: Option<LlmUsageWire>,
    /// Themes identified in Phase 1 (id list). Empty if Phase 1 emitted
    /// nothing.
    pub theme_ids: Vec<String>,
    /// D6 (W-MEMORY-LIFECYCLE 2026-07-09) — number of phase LLM responses that
    /// failed to parse during this run (each also logged to the daily log as a
    /// `memory.dream.parse_failure` event). The pipeline stays fail-soft
    /// (defaults substituted, run continues), but the silent-swallow is gone:
    /// a run that "succeeded" on garbage output is now observable. `default`
    /// keeps older serialized outputs deserializable.
    #[serde(default)]
    pub parse_failures: u32,
    /// 2026-07-27 §19.1-6「无据不产出」——本次运行里因**证据集为空**而被
    /// 跳过、未发起 Phase-3 的主题数。与 `parse_failures` 刻意分开计数：
    /// 前者是「解析坏了」，本项是「解析成功但真的没证据」。两种语义在
    /// 磁盘上必须可区分，否则又是一个"坏了看起来像没事"（§25.4）。
    #[serde(default)]
    pub themes_skipped_no_evidence: u32,
}

/// Mirror of `LlmUsage` — kept independent of `tier::LlmUsage` so the public
/// wire shape doesn't accidentally couple the Processor's output to its input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct LlmUsageWire {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum DreamProcessError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("write: {0}")]
    Write(String),
    #[error("llm call timeout after {0}ms")]
    Timeout(u64),
    #[error("llm call failed: {0}")]
    LlmFailure(String),
    #[error("orchestrator shutdown while awaiting llm result")]
    Shutdown,
    #[error("invalid llm output: {0}")]
    InvalidOutput(String),
}

/// Tier-3 processor — owns the gate, the pending oneshot map, the emitter,
/// and the per-instance state map shared with the gate.
pub struct DreamProcessor {
    gate: Arc<AutoDreamGate>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<LlmCallResultPayload>>>>,
    emitter: Arc<dyn LlmCallEmitter>,
    req_id_counter: AtomicU64,
}

impl DreamProcessor {
    pub fn new(gate: Arc<AutoDreamGate>, emitter: Arc<dyn LlmCallEmitter>) -> Self {
        Self {
            gate,
            pending: Arc::new(Mutex::new(HashMap::new())),
            emitter,
            req_id_counter: AtomicU64::new(0),
        }
    }

    pub fn gate(&self) -> &Arc<AutoDreamGate> {
        &self.gate
    }

    /// Deliver a reverse IPC LLM call result. Unknown `req_id` → no-op
    /// (treated as late delivery after timeout).
    pub async fn deliver_result(&self, result: LlmCallResultPayload) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(sender) = map.remove(&result.req_id) {
            let _ = sender.send(result);
            true
        } else {
            false
        }
    }

    fn next_req_id(&self, phase: &str) -> String {
        let n = self.req_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("tier3-{phase}-{n}-{}", now_ms())
    }

    /// Single round-trip helper: register a pending oneshot, emit the
    /// request, await the result (timeout), validate.
    async fn call_llm(
        &self,
        phase: &str,
        messages: Vec<LlmMessage>,
        model_hint: Option<String>,
        params: LlmCallParams,
    ) -> Result<(String, String, Option<LlmUsage>), DreamProcessError> {
        let req_id = self.next_req_id(phase);
        let (tx, rx) = oneshot::channel::<LlmCallResultPayload>();
        {
            let mut map = self.pending.lock().await;
            map.insert(req_id.clone(), tx);
        }

        let request_payload = LlmCallRequestPayload {
            req_id: req_id.clone(),
            tier: MemoryTier::Dream,
            phase: Some(phase.to_string()),
            messages,
            model_hint,
            params,
        };
        self.emitter.emit_request(request_payload).await;

        let result =
            match tokio::time::timeout(Duration::from_millis(LLM_CALL_TIMEOUT_MS), rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_recv_err)) => {
                    self.pending.lock().await.remove(&req_id);
                    return Err(DreamProcessError::Shutdown);
                }
                Err(_elapsed) => {
                    self.pending.lock().await.remove(&req_id);
                    return Err(DreamProcessError::Timeout(LLM_CALL_TIMEOUT_MS));
                }
            };

        if let Some(err) = result.error.as_ref() {
            return Err(DreamProcessError::LlmFailure(err.clone()));
        }
        let response = result
            .response
            .ok_or_else(|| DreamProcessError::LlmFailure("empty response".to_string()))?;

        Ok((req_id, response, result.usage))
    }

    /// Run one full Tier-3 dream consolidation: Phase 0 → 1 → 2 → 3 → 4.
    ///
    /// A3 fix (P0-3, 2026-06-05) — RAII-on-process settlement of the real
    /// `.consolidate-lock`. The `evaluate_dream_run_now` / `AutoDreamGate`
    /// gate acquires the lock with the live orchestrator PID; the W-MEMORY-
    /// EVOLUTION `spawn_dream_now` / `run_dream_tick` paths call `process()`
    /// DIRECTLY (bypassing `ResultListener::handle_completed`, the only other
    /// release path). Previously the file lock was NEVER released on these
    /// paths → it stayed held until ~1h stale → every subsequent gate returned
    /// `lock_held` and dreaming self-deadlocked for an hour. We now settle the
    /// lock at BOTH return sites of the pipeline, reproducing
    /// `ResultListener`'s ASYMMETRIC semantics EXACTLY.
    ///
    /// On SUCCESS: `lock::record_consolidation_complete` (empty body + FRESH
    /// mtime → arms the time-gate as "just consolidated"). NOT a rollback —
    /// rolling back to the prior mtime on success would re-arm the gate to fire
    /// immediately every run = tight-loop dreaming (the inverse bug).
    ///
    /// On FAILURE: `lock::rollback(prior_mtime_ms)` (restore the prior mtime so
    /// the next scheduled dream can re-attempt on the original cadence).
    ///
    /// We settle EXPLICITLY (Drop can't await; the ops are plain fs writes,
    /// same as `ResultListener`). When the gate did a `forced_skip_lock`
    /// virtual-acquire it took NO real lock — detected here because the lock
    /// file does NOT hold this process's PID — so settle is a NO-OP (never
    /// clobber a foreign holder).
    pub async fn process(
        &self,
        input: DreamProcessInput,
    ) -> Result<DreamProcessOutput, DreamProcessError> {
        let memory_dir = input.memory_dir.clone();
        let prior_mtime_ms = input.gate_payload.prior_mtime_ms;
        let holder_pid = input.gate_payload.holder_pid;
        let consumed_watermark_ms = input.consumed_watermark_ms;

        let result = self.run_pipeline(input).await;
        self.settle_consolidate_lock(
            &memory_dir,
            prior_mtime_ms,
            holder_pid,
            result.is_ok(),
            consumed_watermark_ms,
        )
        .await;
        result
    }

    /// Settle the `.consolidate-lock` after a `process()` run. NO-OP unless we
    /// actually hold the real lock (its body == our live PID). See `process`
    /// doc for the success=fresh-mtime / failure=prior-mtime asymmetry. Errors
    /// are logged fail-soft: a settle IO failure must not mask the pipeline
    /// result (and a stale lease still self-heals via the ~1h staleness gate).
    async fn settle_consolidate_lock(
        &self,
        memory_dir: &Path,
        prior_mtime_ms: u64,
        holder_pid: u32,
        succeeded: bool,
        consumed_watermark_ms: Option<u64>,
    ) {
        // Only settle when we genuinely hold the real lock. A
        // `forced_skip_lock` virtual-acquire writes no PID to the lock file, so
        // `current_holder_pid` won't match → NO-OP (do not clobber a foreign /
        // already-released holder).
        match lock::current_holder_pid(memory_dir).await {
            Ok(Some(pid)) if pid == holder_pid && pid == std::process::id() => {}
            Ok(_) => return,
            Err(e) => {
                log::warn!("tier3 dream: failed to read consolidate-lock holder (fail-soft): {e}");
                return;
            }
        }

        let settle = if succeeded {
            // G3-a：语料撞帽时水位线只推进到已消费最新会话（0/None = 旧
            // 语义 fresh mtime）。单调性由语料过滤保证（consumed > since）。
            lock::record_consolidation_complete_at(memory_dir, consumed_watermark_ms.unwrap_or(0))
                .await
        } else {
            lock::rollback(memory_dir, prior_mtime_ms).await
        };
        if let Err(e) = settle {
            log::warn!("tier3 dream: failed to settle consolidate-lock (fail-soft): {e}");
        }
    }

    /// The 5-phase pipeline body. Settlement of the file lock is handled by the
    /// outer `process` wrapper; this method owns only the in-memory
    /// `dream_in_progress` flag (via the Drop releaser) + the LLM round-trips.
    async fn run_pipeline(
        &self,
        input: DreamProcessInput,
    ) -> Result<DreamProcessOutput, DreamProcessError> {
        // Resolve the instance key + look up the state shared with the gate
        // so we can release `dream_in_progress` on exit (success or failure).
        let key = if input.instance_key.is_empty() {
            input.memory_dir.to_string_lossy().to_string()
        } else {
            input.instance_key.clone()
        };
        let state = self.gate.get_or_init_state(&key).await;

        struct InProgressReleaser {
            state: Arc<AutoDreamInstanceState>,
        }
        impl Drop for InProgressReleaser {
            fn drop(&mut self) {
                self.state.dream_in_progress.store(0, Ordering::Release);
            }
        }
        let _releaser = InProgressReleaser {
            state: Arc::clone(&state),
        };

        let dreams_dir = input.memory_dir.join("dreams");
        tokio::fs::create_dir_all(&dreams_dir).await?;

        // G3-c (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16) — 写前近重复门限
        // （dream-config `dedup` 段；1.0 = 关闭，只剩精确去重兜底）。
        let variant_psd = crate::dream_gate::project_state_dir_from_memory_dir(&input.memory_dir);
        let jaccard_threshold = crate::dream_config::read_dream_config(&variant_psd)
            .map(|cfg| cfg.dedup.jaccard_threshold)
            .unwrap_or(crate::dream_config::DEFAULT_DEDUP_JACCARD_THRESHOLD);
        // 8c (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16) — phase3 变体选择（UCB1，
        // 编译期常量族；产物 frontmatter 归因，冗余拦截记负、存活 14 天记
        // 胜由 fitness sweep 完成）。
        let phase3_variant = {
            let archive = crate::evolution::variants::load_archive(&variant_psd);
            crate::evolution::variants::select_variant(
                crate::evolution::variants::PHASE3_VARIANTS,
                &archive,
            )
        };

        let mut phases_completed: Vec<String> = Vec::new();
        let mut req_ids: Vec<String> = Vec::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        // D6 — per-run count of phase LLM responses that failed to parse.
        let mut parse_failures: u32 = 0;
        // §19.1-6 —— 因证据集为空而跳过（未发起 Phase-3）的主题数。
        let mut themes_skipped_no_evidence: u32 = 0;
        // 实际发起过的 Phase-3 调用次数，用于诚实填 `phases_completed`。
        let mut phase3_calls: u32 = 0;

        // ── Phase 0: Self-RAG reflection ──
        let prior_insights = scan_dreams_files(&dreams_dir, "insight_").await?;
        let phase0_input = build_phase0_messages(&prior_insights);
        let (p0_req, p0_response, p0_usage) = self
            .call_llm(
                "phase0",
                phase0_input,
                input.model_hint.clone(),
                input.params.clone(),
            )
            .await?;
        req_ids.push(p0_req);
        accumulate_usage(p0_usage, &mut input_tokens, &mut output_tokens);
        // D6 — fail-soft on parse failure (defaults substituted) but observable:
        // count + daily-log instead of a bare `unwrap_or_default` swallow.
        let reflection = match parse_phase0_json(&p0_response) {
            Some(reflection) => reflection,
            None => {
                record_parse_failure(&input.memory_dir, "phase0", &mut parse_failures).await;
                Phase0Reflection::default()
            }
        };
        phases_completed.push("phase0".to_string());

        // ── Phase 1: Orient ──
        let phase1_input =
            build_phase1_messages(&input.recent_sessions_summary, &input.memdir_manifest);
        let (p1_req, p1_response, p1_usage) = self
            .call_llm(
                "phase1",
                phase1_input,
                input.model_hint.clone(),
                input.params.clone(),
            )
            .await?;
        req_ids.push(p1_req);
        accumulate_usage(p1_usage, &mut input_tokens, &mut output_tokens);
        let themes = match parse_phase1_json(&p1_response) {
            Some(themes) => themes,
            None => {
                record_parse_failure(&input.memory_dir, "phase1", &mut parse_failures).await;
                Vec::new()
            }
        };
        phases_completed.push("phase1".to_string());

        let theme_ids: Vec<String> = themes.iter().map(|t| t.id.clone()).collect();

        if themes.is_empty() {
            // No themes → Phase 2/3 skipped; Phase 4 still runs (prune stale
            // fragments).
            let phase4_paths = self
                .run_phase4_prune(
                    &input.memory_dir,
                    &dreams_dir,
                    &reflection,
                    &input.model_hint,
                    &input.params,
                    &mut req_ids,
                    &mut input_tokens,
                    &mut output_tokens,
                    &mut parse_failures,
                )
                .await?;
            phases_completed.push("phase4".to_string());

            let aggregate = build_usage_wire(input_tokens, output_tokens);
            return Ok(DreamProcessOutput {
                phases_completed,
                insight_paths: Vec::new(),
                fragment_paths: Vec::new(),
                pruned_paths: phase4_paths,
                req_ids,
                aggregate_usage: aggregate,
                theme_ids,
                parse_failures,
                themes_skipped_no_evidence,
            });
        }

        // ── Phase 2 + 3 per theme: Gather then Consolidate ──
        let mut insight_paths: Vec<PathBuf> = Vec::new();
        let mut fragment_paths: Vec<PathBuf> = Vec::new();

        for theme in &themes {
            // Phase 2.
            let phase2_input =
                build_phase2_messages(&theme.id, &theme.label, &input.recent_sessions_summary);
            let (p2_req, p2_response, p2_usage) = self
                .call_llm(
                    "phase2",
                    phase2_input,
                    input.model_hint.clone(),
                    input.params.clone(),
                )
                .await?;
            req_ids.push(p2_req);
            accumulate_usage(p2_usage, &mut input_tokens, &mut output_tokens);
            let evidence = match parse_phase2_json(&p2_response) {
                Some(evidence) => evidence,
                None => {
                    record_parse_failure(&input.memory_dir, "phase2", &mut parse_failures).await;
                    Vec::new()
                }
            };

            // 2026-07-27 §19.1-6 —— **无据不产出（硬纪律）**。
            //
            // 旧行为：证据为空时仍然发起 Phase-3，`build_phase3_messages` 把
            // `(no evidence collected)` 塞给模型、却仍要求它"综合出一条记忆
            // 块" —— 模型在零素材下写记忆，等于结构性地鼓励编造。产物落盘
            // 后会被索引、被召回、再喂给下一轮做梦与想象，形成自我强化的
            // 虚构。做梦的全部合法性来自"它是对真实会话的再综合"，无据产出
            // 直接抽掉这个前提。
            //
            // 现在：跳过该主题，**不发起 Phase-3 调用**（顺带省掉一次 LLM
            // 往返）。这一条独立于 Phase-2 的解析鲁棒化都必须成立 —— 即使
            // Phase-2 合法地返回了空证据，也不该产出无据洞察。
            if evidence.is_empty() {
                record_theme_skipped_no_evidence(
                    &input.memory_dir,
                    &theme.id,
                    &mut themes_skipped_no_evidence,
                )
                .await;
                continue;
            }

            // Phase 3.
            let mut phase3_input = build_phase3_messages(&theme.id, &theme.label, &evidence);
            // 8c — 变体 addendum 追加到 system message（v0 基线为空 = 零差异）。
            if !phase3_variant.addendum.is_empty() {
                if let Some(system) = phase3_input.first_mut() {
                    system.content.push_str(phase3_variant.addendum);
                }
            }
            let (p3_req, p3_response, p3_usage) = self
                .call_llm(
                    "phase3",
                    phase3_input,
                    input.model_hint.clone(),
                    input.params.clone(),
                )
                .await?;
            phase3_calls += 1;
            req_ids.push(p3_req);
            accumulate_usage(p3_usage, &mut input_tokens, &mut output_tokens);

            let block = parse_phase3_block(&p3_response);
            if let Some(block) = block {
                // §19.1-6 之后 `evidence` 在此处恒非空（空证据主题已在
                // Phase-3 之前 `continue`），原先的 `|| evidence.is_empty()`
                // 分支随之不可达，删除以免留下误导性死条件。
                let is_fragment = matches!(block.confidence.as_str(), "low");
                let (prefix, target_vec) = if is_fragment {
                    ("fragment_", &mut fragment_paths)
                } else {
                    ("insight_", &mut insight_paths)
                };
                // D8 — filename carries a theme_id short-hash suffix so two
                // themes whose sanitized stems collide no longer overwrite
                // each other (same theme stays idempotent).
                let filename = theme_filename(prefix, &theme.id);
                let path = dreams_dir.join(filename);
                // W-MEMORY-SYNERGY W6 (2026-07-16, 6b) — 正文级精确去重：
                // dreams/ 里已有 body 相同的其它文件时跳过写入（重复
                // insight 堆积是最普通的记忆污染形态；同名同主题的更新仍
                // 覆盖——只拦「不同文件名、相同内容」的冗余）。
                // G3-c：精确去重（SHA-256）+ 近重复去重（词集 Jaccard）双门。
                if dreams_dir_has_duplicate_body(&dreams_dir, &path, &block.full_content).await
                    || dreams_dir_has_near_duplicate_body(
                        &dreams_dir,
                        &path,
                        &block.full_content,
                        jaccard_threshold,
                    )
                    .await
                {
                    log::info!(
                        "tier3 dream: duplicate body detected — skipping redundant write of {}",
                        path.display()
                    );
                    // 8c — 当选变体产出冗余 → 记负。
                    crate::evolution::variants::record_outcome(
                        &variant_psd,
                        phase3_variant.id,
                        false,
                        now_ms(),
                    )
                    .await;
                    continue;
                }
                // 8c — 产物 frontmatter 归因（只动 frontmatter，正文去重
                // hash 不受影响 —— variants 测试钉死）。
                let attributed = crate::evolution::variants::inject_frontmatter_line(
                    &block.full_content,
                    "prompt_variant",
                    phase3_variant.id,
                );
                atomic_write(&path, attributed.as_bytes())
                    .await
                    .map_err(|e: BoxError| DreamProcessError::Write(e.to_string()))?;
                target_vec.push(path);
            } else {
                // D6 — a phase-3 response that is not a frontmatter block means
                // this theme produced NO artifact; that loss must be visible.
                record_parse_failure(&input.memory_dir, "phase3", &mut parse_failures).await;
            }
        }
        phases_completed.push("phase2".to_string());
        // §19.1-6：所有主题都因证据为空被跳过时，Phase-3 一次都没发起过，
        // 不能再无条件宣称它"已完成" —— `phases_completed` 是诊断契约，
        // 谎报会让"全部跳过"看起来和"正常跑完"一样（§25.4）。
        if phase3_calls > 0 {
            phases_completed.push("phase3".to_string());
        }

        // ── Phase 4: Prune ──
        let pruned_paths = self
            .run_phase4_prune(
                &input.memory_dir,
                &dreams_dir,
                &reflection,
                &input.model_hint,
                &input.params,
                &mut req_ids,
                &mut input_tokens,
                &mut output_tokens,
                &mut parse_failures,
            )
            .await?;
        phases_completed.push("phase4".to_string());

        let aggregate = build_usage_wire(input_tokens, output_tokens);
        Ok(DreamProcessOutput {
            phases_completed,
            insight_paths,
            fragment_paths,
            pruned_paths,
            req_ids,
            aggregate_usage: aggregate,
            theme_ids,
            parse_failures,
            themes_skipped_no_evidence,
        })
    }

    /// Phase 4 helper. Combines the Phase 0 stale set with an LLM-driven
    /// prune over current fragments. Deletes fragments mentioned in the
    /// LLM `delete_ids` set OR fragments older than `FRAGMENT_STALE_MS`.
    #[allow(clippy::too_many_arguments)]
    async fn run_phase4_prune(
        &self,
        memory_dir: &Path,
        dreams_dir: &Path,
        reflection: &Phase0Reflection,
        model_hint: &Option<String>,
        params: &LlmCallParams,
        req_ids: &mut Vec<String>,
        input_tokens: &mut u32,
        output_tokens: &mut u32,
        parse_failures: &mut u32,
    ) -> Result<Vec<PathBuf>, DreamProcessError> {
        let fragments = scan_dreams_files(dreams_dir, "fragment_").await?;
        let now = now_ms();

        // Stage 1: age-based pruning (deterministic, no LLM).
        let mut targets: Vec<(String, PathBuf)> = Vec::new();
        for f in &fragments {
            if f.mtime_ms != 0 && now.saturating_sub(f.mtime_ms) > FRAGMENT_STALE_MS {
                targets.push((f.id.clone(), f.path.clone()));
            }
        }

        // Stage 2: LLM-driven pruning. D5 (W-MEMORY-LIFECYCLE 2026-07-09): the
        // old gate `!fragments.is_empty() && (!stale.is_empty() ||
        // !fragments.is_empty())` was a tautology — ANY fragment burned a prune
        // LLM round-trip every run even with zero prune signal. The LLM only
        // has something to decide when Phase 0 flagged stale ids OR at least
        // one fragment has aged past `FRAGMENT_STALE_MS` (== stage-1 `targets`,
        // computed from exactly that predicate).
        if !fragments.is_empty() && (!reflection.stale_ids.is_empty() || !targets.is_empty()) {
            let fragment_list = fragments
                .iter()
                .map(|f| format!("- {}", f.id))
                .collect::<Vec<_>>()
                .join("\n");
            let stale_ids_json =
                serde_json::to_string(&reflection.stale_ids).unwrap_or_else(|_| "[]".to_string());
            let messages = vec![
                LlmMessage {
                    role: "system".to_string(),
                    content: TIER3_DREAM_PHASE4_PRUNE_PROMPT
                        .replace("{{stale_ids}}", &stale_ids_json)
                        .replace("{{fragment_list}}", &fragment_list),
                },
                LlmMessage {
                    role: "user".to_string(),
                    content: "Apply the rules and return the delete_ids JSON.".to_string(),
                },
            ];
            let (p4_req, p4_response, p4_usage) = self
                .call_llm("phase4", messages, model_hint.clone(), params.clone())
                .await?;
            req_ids.push(p4_req);
            accumulate_usage(p4_usage, input_tokens, output_tokens);

            let delete_ids = match parse_phase4_json(&p4_response) {
                Some(delete_ids) => delete_ids,
                None => {
                    record_parse_failure(memory_dir, "phase4", parse_failures).await;
                    Vec::new()
                }
            };
            for id in &delete_ids {
                if let Some(f) = fragments.iter().find(|f| &f.id == id) {
                    if !targets.iter().any(|(seen_id, _)| seen_id == &f.id) {
                        targets.push((f.id.clone(), f.path.clone()));
                    }
                }
            }
        }

        // Execute deletions.
        let mut pruned: Vec<PathBuf> = Vec::new();
        for (_, path) in targets {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => pruned.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Already gone (concurrent cleanup) — no-op.
                }
                Err(e) => return Err(DreamProcessError::Io(e)),
            }
        }
        Ok(pruned)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Phase 0/1/2/3/4 message builders + parsers
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct Phase0Reflection {
    #[allow(dead_code)]
    still_valid_ids: Vec<String>,
    stale_ids: Vec<String>,
    #[allow(dead_code)]
    notes: String,
}

#[derive(Debug, Clone)]
struct Phase1Theme {
    id: String,
    label: String,
}

#[derive(Debug, Clone)]
struct Phase2Evidence {
    #[allow(dead_code)]
    source: String,
    snippet: String,
    weight: f64,
}

#[derive(Debug, Clone)]
struct Phase3Block {
    full_content: String,
    confidence: String,
}

fn build_phase0_messages(prior_insights: &[DreamFileScan]) -> Vec<LlmMessage> {
    let body = if prior_insights.is_empty() {
        "(no prior insights — first dream)".to_string()
    } else {
        prior_insights
            .iter()
            .take(10) // cap input size; LLM context is precious.
            .map(|f| format!("- {} (path: {})", f.id, f.path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_DREAM_PHASE0_PROMPT.replace("{{prior_insights}}", &body),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Apply the rules and return the reflection JSON.".to_string(),
        },
    ]
}

fn build_phase1_messages(session_summary: &str, memdir_summary: &str) -> Vec<LlmMessage> {
    let session_body = if session_summary.trim().is_empty() {
        "(no recent sessions)".to_string()
    } else {
        session_summary.to_string()
    };
    let memdir_body = if memdir_summary.trim().is_empty() {
        "(memdir empty)".to_string()
    } else {
        memdir_summary.to_string()
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_DREAM_PHASE1_ORIENT_PROMPT
                .replace("{{session_summary}}", &session_body)
                .replace("{{memdir_summary}}", &memdir_body),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Apply the rules and return the themes JSON.".to_string(),
        },
    ]
}

fn build_phase2_messages(
    theme_id: &str,
    theme_label: &str,
    session_summary: &str,
) -> Vec<LlmMessage> {
    let excerpts = if session_summary.trim().is_empty() {
        "(no session excerpts)".to_string()
    } else {
        session_summary.to_string()
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_DREAM_PHASE2_GATHER_PROMPT
                .replace("{{theme_id}}", theme_id)
                .replace("{{theme_label}}", theme_label)
                .replace("{{session_excerpts}}", &excerpts),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Apply the rules and return the evidence_refs JSON.".to_string(),
        },
    ]
}

fn build_phase3_messages(
    theme_id: &str,
    theme_label: &str,
    evidence: &[Phase2Evidence],
) -> Vec<LlmMessage> {
    let evidence_list = if evidence.is_empty() {
        "(no evidence collected)".to_string()
    } else {
        evidence
            .iter()
            .map(|e| format!("- (weight {:.2}) {}", e.weight, e.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_DREAM_PHASE3_CONSOLIDATE_PROMPT
                .replace("{{theme_id}}", theme_id)
                .replace("{{theme_label}}", theme_label)
                .replace("{{evidence_list}}", &evidence_list),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Emit the consolidated memory block now.".to_string(),
        },
    ]
}

fn parse_phase0_json(raw: &str) -> Option<Phase0Reflection> {
    // §19.1-5：四个 JSON 解析器共用同一个宽容前端，避免"各家严格度不一"
    // 这个缺陷本身（§19.3）在别的相里重演。字段语义各自保持不变。
    let value = parse_json_lenient(raw)?;
    let still_valid_ids = value
        .get("still_valid_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let stale_ids = value
        .get("stale_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let notes = value
        .get("notes")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_default();
    Some(Phase0Reflection {
        still_valid_ids,
        stale_ids,
        notes,
    })
}

fn parse_phase1_json(raw: &str) -> Option<Vec<Phase1Theme>> {
    let value = parse_json_lenient(raw)?;
    let arr = value.get("themes")?.as_array()?;
    let mut out = Vec::new();
    for entry in arr {
        let id = entry.get("id").and_then(|v| v.as_str())?.to_string();
        let label = entry
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        out.push(Phase1Theme { id, label });
    }
    Some(out)
}

/// Phase-2 证据集的可接受键名。2026-07-27 §19.1-5：键名漂移
/// （`evidence` / `refs`）是实测失败形态之一，接受别名的成本是一个常量表。
const PHASE2_EVIDENCE_KEYS: [&str; 3] = ["evidence_refs", "evidence", "refs"];

/// Phase-2 证据解析。
///
/// 2026-07-27 §19.3 —— **解析器的严格度必须与失败后果匹配**。全五个解析器
/// 里，phase3 失败只是少一份产物（安全），而 phase2 失败会让下游拿着空证据
/// 继续跑（危险），偏偏它此前用的是最严格的实现（整串必须是合法 JSON +
/// 键名类型必须精确），实测 8/9 次解析失败全部落在它身上。现在把它改成最
/// 宽容的一个：
/// - 散文包裹 → `parse_json_lenient` 截配平 JSON；
/// - 顶层直接是数组 → 当作证据列表；
/// - 键名 `evidence_refs` / `evidence` / `refs` 都接受；
/// - 数组元素是裸字符串 → 当作 snippet（weight 0）。
///
/// 仍然**不做语义修复**：拿不到任何结构就返回 `None`，由调用方记
/// `parse_failure` 并跳过该主题（无据不产出，§19.1-6）。
fn parse_phase2_json(raw: &str) -> Option<Vec<Phase2Evidence>> {
    let value = parse_json_lenient(raw)?;
    let arr = match value.as_array() {
        Some(arr) => arr,
        None => PHASE2_EVIDENCE_KEYS
            .iter()
            .find_map(|key| value.get(*key).and_then(serde_json::Value::as_array))?,
    };
    let mut out = Vec::new();
    for entry in arr {
        if let Some(snippet) = entry.as_str() {
            if !snippet.is_empty() {
                out.push(Phase2Evidence {
                    source: String::new(),
                    snippet: snippet.to_owned(),
                    weight: 0.0,
                });
            }
            continue;
        }
        let source = entry
            .get("source")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_default();
        let snippet = entry
            .get("snippet")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_default();
        let weight = entry
            .get("weight")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        if snippet.is_empty() {
            continue;
        }
        out.push(Phase2Evidence {
            source,
            snippet,
            weight,
        });
    }
    Some(out)
}

fn parse_phase3_block(raw: &str) -> Option<Phase3Block> {
    let trimmed = strip_fences(raw.trim());
    if !trimmed.starts_with("---") {
        return None;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    // Find the closing `---`.
    let mut closing_idx = None;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            closing_idx = Some(i);
            break;
        }
    }
    let closing = closing_idx?;

    let mut confidence = String::new();
    for line in &lines[1..closing] {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("confidence:") {
            confidence = v.trim().to_string();
        }
    }
    if confidence.is_empty() {
        confidence = "medium".to_string();
    }

    let mut content = String::new();
    content.push_str("---\n");
    for line in &lines[1..closing] {
        content.push_str(line);
        content.push('\n');
    }
    content.push_str("---\n\n");
    for line in &lines[closing + 1..] {
        content.push_str(line);
        content.push('\n');
    }
    // Trim trailing newlines, keep one.
    let mut content = content.trim_end().to_string();
    content.push('\n');

    Some(Phase3Block {
        full_content: content,
        confidence,
    })
}

fn parse_phase4_json(raw: &str) -> Option<Vec<String>> {
    let value = parse_json_lenient(raw)?;
    let arr = value.get("delete_ids")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
    )
}

/// 去掉 ```` ``` ```` 围栏。2026-07-27 §19.1-5 放宽：原实现只认
/// ```` ```json\n ```` 与 ```` ```\n ```` 两种**精确**前缀，`​```JSON`、
/// ```` ```json ```` 后跟空格、以及收尾围栏前没有换行的形态全部漏网 ——
/// 而这些都是常见模型输出。现在接受任意语言标签，收尾围栏也不要求前置
/// 换行。无围栏时原样返回。
fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // 跳过可选语言标签行（```json / ```JSON / ``` 后直接换行都覆盖）。
    let body = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => rest,
    };
    match body.rsplit_once("```") {
        Some((inner, _)) => inner.trim_end(),
        None => body,
    }
}

/// 从任意文本里截出**第一个括号配平**的 JSON 对象或数组切片。
///
/// 2026-07-27 §19.1-5：模型在 JSON 前后附带散文（"Here is the evidence:" /
/// 结尾补一句解释）是最常见的失败形态之一，而 `serde_json::from_str` 要求
/// **整串**都是合法 JSON。这里只做括号配平 + 字符串/转义感知的扫描，
/// **不做任何语义修复**（不补引号、不猜键名）——拿不到配平结构就返回
/// `None`，让调用方走各自的失败分支。
fn extract_first_json_value(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{' || *b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if *byte == b'"' {
            in_string = true;
        } else if *byte == open {
            depth += 1;
        } else if *byte == close {
            depth -= 1;
            if depth == 0 {
                // 括号与引号都是 ASCII，切片边界必然落在 UTF-8 字符边界上。
                return Some(&s[start..=idx]);
            }
        }
    }
    None
}

/// 宽容 JSON 解析：先按原样解析（快路径，行为与旧实现一致），失败后退回
/// "截第一个配平 JSON"再解析一次。两次都不成才是真的解析失败。
fn parse_json_lenient(raw: &str) -> Option<serde_json::Value> {
    let trimmed = strip_fences(raw.trim());
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(value);
    }
    serde_json::from_str::<serde_json::Value>(extract_first_json_value(trimmed)?).ok()
}

// ──────────────────────────────────────────────────────────────────────────
// dreams/ directory scanner
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DreamFileScan {
    /// Filename stem with the prefix stripped (e.g. `insight_FOO.md` → `FOO`).
    id: String,
    path: PathBuf,
    mtime_ms: u64,
}

/// Scan `dreams_dir` for files matching `prefix*.md`. Returns empty Vec if
/// the directory doesn't exist (first-ever dream).
async fn scan_dreams_files(dreams_dir: &Path, prefix: &str) -> std::io::Result<Vec<DreamFileScan>> {
    if !dreams_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<DreamFileScan> = Vec::new();
    let mut entries = tokio::fs::read_dir(dreams_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(prefix) || !name.ends_with(".md") {
            continue;
        }
        let stem = name
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(".md"))
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        let mtime_ms = match entry.metadata().await {
            Ok(meta) => meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            Err(_) => 0,
        };
        out.push(DreamFileScan {
            id: stem,
            path,
            mtime_ms,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

// ──────────────────────────────────────────────────────────────────────────
// helpers
// ──────────────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn accumulate_usage(usage: Option<LlmUsage>, input_tokens: &mut u32, output_tokens: &mut u32) {
    if let Some(u) = usage {
        *input_tokens = input_tokens.saturating_add(u.input_tokens);
        *output_tokens = output_tokens.saturating_add(u.output_tokens);
    }
}

fn build_usage_wire(input_tokens: u32, output_tokens: u32) -> Option<LlmUsageWire> {
    if input_tokens == 0 && output_tokens == 0 {
        None
    } else {
        Some(LlmUsageWire {
            input_tokens,
            output_tokens,
        })
    }
}

/// Sanitize a memory name into a safe filename stem. Mirrors the Tier-2
/// helper of the same name; kept private here to avoid cross-tier import
/// churn (the helper is short).
pub fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// D8 (W-MEMORY-LIFECYCLE 2026-07-09) — insight/fragment filename:
/// `{prefix}{sanitize(theme_id)}_{sha256(theme_id)[..8 hex]}.md`.
///
/// `sanitize_name` is lossy (`a/b` and `a b` both sanitize to `a_b`), so two
/// distinct themes could silently overwrite each other's artifact. The short
/// hash is derived from the RAW theme_id, so:
/// * same theme → same filename → idempotent overwrite preserved;
/// * cross-theme sanitize collisions → distinct filenames.
///
/// The `insight_` / `fragment_` prefix is untouched, so prefix-matched
/// consumers (`collect_tier_files`, `scan_dreams_files`, auto-promotion)
/// stay compatible.
/// W-MEMORY-SYNERGY W6 (2026-07-16, 6b) — dreams/ 目录里是否已存在
/// **其它文件**与新内容 body 相同（frontmatter 排除后 SHA-256，复用
/// `dedup_hash`）。`target` 自身除外 —— 同名同主题的幂等覆盖保持允许，
/// 只拦「不同文件名、相同内容」的冗余堆积。fail-soft：任何读错都按
/// 「无重复」处理（宁可多写一份也不丢产物）。
pub(crate) async fn dreams_dir_has_duplicate_body(
    dreams_dir: &Path,
    target: &Path,
    new_content: &str,
) -> bool {
    let new_hash = crate::dedup_hash::memory_body_hash(new_content);
    let Ok(read_dir) = std::fs::read_dir(dreams_dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path == target {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(existing) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        if crate::dedup_hash::memory_body_hash(&existing) == new_hash {
            return true;
        }
    }
    false
}

/// W-MEMORY-SELF-EVOLVE-DGM G3-c (2026-07-16) — dreams/ 目录**近重复**检测，
/// 与精确去重（上方 `dreams_dir_has_duplicate_body`，frontmatter 排除后
/// SHA-256）互补：拦「同一洞见换了措辞」的语义冗余。判据 = 正文词集
/// Jaccard ≥ `threshold`（`se_integration::tokenize`，charabia 分词 — CJK
/// 正确切分）。护栏：`threshold >= 1.0` = 关闭；任一方词集 <
/// `NEAR_DUP_MIN_TOKENS` 不判（短文本假阳性高）。fail-soft：读错按无重复
/// （宁可多写一份也不丢产物）。
pub(crate) async fn dreams_dir_has_near_duplicate_body(
    dreams_dir: &Path,
    target: &Path,
    new_content: &str,
    threshold: f64,
) -> bool {
    const NEAR_DUP_MIN_TOKENS: usize = 12;
    if threshold >= 1.0 {
        return false;
    }
    let new_tokens: std::collections::HashSet<String> =
        crate::se_integration::tokenize(crate::dedup_hash::memory_body(new_content))
            .into_iter()
            .collect();
    if new_tokens.len() < NEAR_DUP_MIN_TOKENS {
        return false;
    }
    let Ok(read_dir) = std::fs::read_dir(dreams_dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path == target {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(existing) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let existing_tokens: std::collections::HashSet<String> =
            crate::se_integration::tokenize(crate::dedup_hash::memory_body(&existing))
                .into_iter()
                .collect();
        if existing_tokens.len() < NEAR_DUP_MIN_TOKENS {
            continue;
        }
        let intersection = new_tokens.intersection(&existing_tokens).count() as f64;
        let union = (new_tokens.len() + existing_tokens.len()) as f64 - intersection;
        if union > 0.0 && intersection / union >= threshold {
            log::info!(
                "tier3 dream: near-duplicate body (jaccard >= {threshold}) vs {} — skip",
                path.display()
            );
            return true;
        }
    }
    false
}

pub(crate) fn theme_filename(prefix: &str, theme_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(theme_id.as_bytes());
    let digest = hasher.finalize();
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{}_{short}.md", sanitize_name(theme_id))
}

/// D6 (W-MEMORY-LIFECYCLE 2026-07-09) — record one phase LLM-response parse
/// failure: bump the per-run counter, warn, and append a
/// `memory.dream.parse_failure` event (payload carries the phase name) to the
/// project daily log. The pipeline stays fail-soft — this observability hop
/// must never fail the dream, so daily-log IO errors are logged and dropped.
/// The synthetic `TranscriptMeta` mirrors `result_listener::
/// append_runner_daily_log` (the dream pipeline has no real transcript).
/// 2026-07-27 §19.1-6 —— 记录"主题因证据为空被跳过"。
///
/// 刻意与 `record_parse_failure` 分开：`memory.dream.theme_skipped_no_evidence`
/// 是**解析成功但真的没料**，`memory.dream.parse_failure` 是**解析坏了**。
/// 两者混在一个桶里就正好复现 §25.4 那条家族缺陷——仪表分不清"没事可做"
/// 与"坏了"。
async fn record_theme_skipped_no_evidence(
    memory_dir: &Path,
    theme_id: &str,
    themes_skipped: &mut u32,
) {
    *themes_skipped += 1;
    log::info!(
        "tier3 dream: theme {theme_id} skipped — Phase-2 produced no evidence; \
         Phase-3 not invoked (no evidence, no output)"
    );

    let project_state_dir = crate::dream_gate::project_state_dir_from_memory_dir(memory_dir);
    let occurred_at_ms = now_ms();
    crate::evolution::gate_stats::record_theme_skipped_no_evidence(
        &project_state_dir,
        occurred_at_ms,
    )
    .await;
    let transcript_meta = crate::daily_log::TranscriptMeta {
        session_id: "tier3-dream".to_owned(),
        path: project_state_dir.join("tier3-dream.jsonl"),
        mtime_ms: occurred_at_ms,
        size_bytes: 0,
        sealed: true,
    };
    let event = crate::daily_log::SessionEvent {
        event_id: format!("tier3-dream-no-evidence-{occurred_at_ms}-{themes_skipped}"),
        kind: "memory.dream.theme_skipped_no_evidence".to_owned(),
        occurred_at_ms,
        payload: serde_json::json!({ "theme_id": theme_id }),
    };
    if let Err(e) =
        crate::daily_log::append_daily_log(&project_state_dir, &transcript_meta, &[event]).await
    {
        log::warn!("tier3 dream: theme_skipped daily-log append failed (fail-soft): {e}");
    }
}

async fn record_parse_failure(memory_dir: &Path, phase: &str, parse_failures: &mut u32) {
    *parse_failures += 1;
    log::warn!(
        "tier3 dream: {phase} LLM response failed to parse (fail-soft, defaults substituted)"
    );

    let project_state_dir = crate::dream_gate::project_state_dir_from_memory_dir(memory_dir);
    let occurred_at_ms = now_ms();
    // R3-4：解析失败此前只进日志、不进任何指标 —— 于是"无据生成"对系统的
    // 自我认知完全不可见。现在它进 gate-stats，成为可被观测的一维。
    crate::evolution::gate_stats::record_parse_failure(&project_state_dir, occurred_at_ms).await;
    let transcript_meta = crate::daily_log::TranscriptMeta {
        session_id: "tier3-dream".to_owned(),
        path: project_state_dir.join("tier3-dream.jsonl"),
        mtime_ms: occurred_at_ms,
        size_bytes: 0,
        sealed: true,
    };
    let event = crate::daily_log::SessionEvent {
        event_id: format!("tier3-dream-parse-failure-{phase}-{occurred_at_ms}-{parse_failures}"),
        kind: "memory.dream.parse_failure".to_owned(),
        occurred_at_ms,
        payload: serde_json::json!({ "phase": phase }),
    };
    if let Err(e) =
        crate::daily_log::append_daily_log(&project_state_dir, &transcript_meta, &[event]).await
    {
        log::warn!("tier3 dream: parse_failure daily-log append failed (fail-soft): {e}");
    }
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-SELF-EVOLUTION B1 (2026-06-11, 用户裁决④) — insight auto-promotion
// ──────────────────────────────────────────────────────────────────────────

/// Minimal frontmatter scan for an insight file: returns
/// `(name, description, confidence)` with honest fallbacks. Only the head of
/// the file is needed; we read the whole file because insights are small
/// (single LLM block).
pub(crate) fn scan_insight_frontmatter(raw: &str) -> (String, String, String) {
    let mut name = String::new();
    let mut description = String::new();
    let mut confidence = String::new();
    let mut in_frontmatter = false;
    for (i, line) in raw.lines().enumerate() {
        let t = line.trim();
        if i == 0 {
            if t == "---" {
                in_frontmatter = true;
                continue;
            }
            break;
        }
        if !in_frontmatter {
            break;
        }
        if t == "---" {
            break;
        }
        if let Some(v) = t.strip_prefix("name:") {
            name = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("description:") {
            description = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("confidence:") {
            confidence = v.trim().to_string();
        }
    }
    (name, description, confidence)
}

/// Promote qualifying dream insights into the `MEMORY.md` index — the strong
/// injection channel (MEMORY.md is loaded into the system prompt every
/// session). This is the loop-closing hop: without it, dream output sits in
/// `dreams/` where only the weak query-time recall selector can ever surface
/// it, and "self-evolution" never feeds back into behaviour (立项 doc §1
/// 缺陷1). Tier policy comes from `dream-config.json` `auto_promote`
/// (default: only `high`-confidence insights; 用户裁决④ — configurable, not
/// hardcoded). Imagination drafts are NEVER routed through here.
///
/// Fail-soft: returns the number of newly-indexed insights; IO problems are
/// logged and skipped (a failed promotion must not fail the dream).
pub async fn auto_promote_insights(
    memory_dir: &Path,
    auto_promote: crate::dream_config::AutoPromoteTier,
    insight_paths: &[PathBuf],
) -> usize {
    use crate::dream_config::AutoPromoteTier;
    if auto_promote == AutoPromoteTier::Off || insight_paths.is_empty() {
        return 0;
    }
    let mut lines: Vec<String> = Vec::new();
    for path in insight_paths {
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(raw) => raw,
            Err(e) => {
                log::warn!("auto-promote: read {} failed (skip): {e}", path.display());
                continue;
            }
        };
        let (name, description, confidence) = scan_insight_frontmatter(&raw);
        if !auto_promote.admits(&confidence) {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let title = if description.is_empty() {
            if name.is_empty() {
                filename.to_string()
            } else {
                name.clone()
            }
        } else {
            description.clone()
        };
        lines.push(format!(
            "- [{title}](dreams/{filename}) — 梦境洞察（{confidence}·自动晋级）"
        ));
    }
    if lines.is_empty() {
        return 0;
    }
    let count = lines.len();
    let memory_md = memory_dir.join("MEMORY.md");
    match crate::tier::tier2_extract_memories::append_to_memory_index(&memory_md, &lines).await {
        Ok(()) => {
            log::info!("auto-promote: indexed {count} insight(s) into MEMORY.md");
            count
        }
        Err(e) => {
            log::warn!("auto-promote: MEMORY.md append failed (fail-soft): {e}");
            0
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    // ── W-MEMORY-SELF-EVOLUTION B1: insight auto-promotion ──

    fn write_insight(dir: &Path, filename: &str, confidence: &str) -> PathBuf {
        let dreams = dir.join("dreams");
        std::fs::create_dir_all(&dreams).unwrap();
        let path = dreams.join(filename);
        std::fs::write(
            &path,
            format!(
                "---\ntype: insight\nname: theme-x\ndescription: 用户偏好洞察\nconfidence: {confidence}\n---\n\nbody\n"
            ),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn auto_promote_high_only_indexes_high_confidence_into_memory_md() {
        let tmp = TempDir::new().unwrap();
        let high = write_insight(tmp.path(), "insight_a.md", "high");
        let medium = write_insight(tmp.path(), "insight_b.md", "medium");

        let count = auto_promote_insights(
            tmp.path(),
            crate::dream_config::AutoPromoteTier::High,
            &[high, medium],
        )
        .await;

        assert_eq!(count, 1);
        let index = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(index.contains("(dreams/insight_a.md)"));
        assert!(!index.contains("insight_b.md"));
        assert!(index.contains("自动晋级"));
    }

    #[tokio::test]
    async fn auto_promote_off_writes_nothing_and_dedups_on_rerun() {
        let tmp = TempDir::new().unwrap();
        let high = write_insight(tmp.path(), "insight_a.md", "high");

        let off = auto_promote_insights(
            tmp.path(),
            crate::dream_config::AutoPromoteTier::Off,
            std::slice::from_ref(&high),
        )
        .await;
        assert_eq!(off, 0);
        assert!(!tmp.path().join("MEMORY.md").exists());

        // Medium tier admits high; running twice must not duplicate the line.
        for _ in 0..2 {
            let _ = auto_promote_insights(
                tmp.path(),
                crate::dream_config::AutoPromoteTier::Medium,
                std::slice::from_ref(&high),
            )
            .await;
        }
        let index = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert_eq!(index.matches("insight_a.md").count(), 1);
    }

    #[test]
    fn auto_promote_tier_parse_and_admits() {
        use crate::dream_config::AutoPromoteTier;
        assert_eq!(AutoPromoteTier::parse("HIGH"), Some(AutoPromoteTier::High));
        assert_eq!(AutoPromoteTier::parse("off"), Some(AutoPromoteTier::Off));
        assert_eq!(AutoPromoteTier::parse("bogus"), None);
        assert!(AutoPromoteTier::High.admits("high"));
        assert!(!AutoPromoteTier::High.admits("medium"));
        assert!(AutoPromoteTier::Medium.admits("medium"));
        assert!(!AutoPromoteTier::Off.admits("high"));
        // 默认 = High（用户裁决④ 合理默认，可经 dream-config.json 调整）。
        assert_eq!(AutoPromoteTier::default(), AutoPromoteTier::High);
    }

    fn input(memory_dir: &Path, touched: u32, forced: bool) -> AutoDreamGateInput {
        AutoDreamGateInput {
            memory_dir: memory_dir.to_path_buf(),
            touched_session_count: touched,
            forced,
            forced_skip_lock: false,
            importance_pressure: false,
            min_hours_override: 0,
            min_sessions_override: 0,
            instance_key: String::new(),
        }
    }

    // ── Gate 1: 时间门控 ──
    #[tokio::test]
    async fn gate1_time_unmet_blocks() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        // Plant a fresh lock file (mtime = now) to simulate recent dream.
        let lock_path = lock::lock_path(&memory_dir);
        tokio::fs::write(&lock_path, "").await.unwrap();

        let gate = AutoDreamGate::new();
        let decision = gate
            .evaluate_gate(input(&memory_dir, 99, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("time_gate_unmet"));
    }

    #[tokio::test]
    async fn gate1_time_satisfied_when_lock_absent() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        // No lock file → prior_mtime_ms = 0 → time gate passes.
        let gate = AutoDreamGate::new();
        // Need session count too to pass — supply enough.
        let decision = gate
            .evaluate_gate(input(&memory_dir, DEFAULT_MIN_SESSIONS, false))
            .await
            .unwrap();
        assert!(
            decision.should_trigger,
            "skip_reason: {:?}",
            decision.skip_reason
        );
    }

    // ── Gate 2: 扫描节流 ──
    #[tokio::test]
    async fn gate2_scan_throttled_blocks_repeat() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = AutoDreamGate::new();
        // First call: passes (sets scan timestamp + may attempt lock).
        let _ = gate
            .evaluate_gate(input(&memory_dir, DEFAULT_MIN_SESSIONS, false))
            .await
            .unwrap();
        // Clear the lock + in_progress so we can re-evaluate; the scan
        // throttle should still block us.
        let _ = lock::rollback(&memory_dir, 0).await;
        let key = memory_dir.to_string_lossy().to_string();
        let state = gate.get_or_init_state(&key).await;
        state.dream_in_progress.store(0, Ordering::Release);

        let decision = gate
            .evaluate_gate(input(&memory_dir, DEFAULT_MIN_SESSIONS, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("scan_throttled"));
    }

    // ── Gate 3: 会话门控 ──
    #[tokio::test]
    async fn gate3_session_count_unmet_blocks() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = AutoDreamGate::new();
        let decision = gate
            .evaluate_gate(input(&memory_dir, DEFAULT_MIN_SESSIONS - 1, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("session_count_unmet"));
    }

    // ── Gate 4: PID 锁 ──
    #[tokio::test]
    async fn gate4_lock_held_skipped_when_a_running_holder_exists() {
        // We simulate "lock held by a different live PID" by writing a lock
        // file with the current process's own PID and setting `dream_in_progress`
        // to mimic an already-running dream — but the simplest, deterministic
        // way to exercise the lock_held branch is to pre-set the in-progress
        // flag. The lock module test coverage already proves the PID-based
        // serialization path.
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = AutoDreamGate::new();
        let key = memory_dir.to_string_lossy().to_string();
        let state = gate.get_or_init_state(&key).await;
        state.dream_in_progress.store(1, Ordering::Release);

        let decision = gate
            .evaluate_gate(input(&memory_dir, DEFAULT_MIN_SESSIONS, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("dream_in_progress"));
    }

    // ── All gates pass: forced bypasses time + scan + session ──
    #[tokio::test]
    async fn forced_skips_time_scan_session_gates() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        // Plant a fresh lock to make time-gate fail in non-forced mode.
        tokio::fs::write(lock::lock_path(&memory_dir), "")
            .await
            .unwrap();
        let gate = AutoDreamGate::new();
        // Forced bypasses time + scan + session, still acquires lock.
        let decision = gate
            .evaluate_gate(input(&memory_dir, 0, true))
            .await
            .unwrap();
        assert!(
            decision.should_trigger,
            "forced should pass; skip: {:?}",
            decision.skip_reason
        );
    }

    #[tokio::test]
    async fn forced_with_skip_lock_passes_even_when_lock_acquire_would_fail() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = AutoDreamGate::new();

        let mut input = input(&memory_dir, 0, true);
        input.forced_skip_lock = true;
        let decision = gate.evaluate_gate(input).await.unwrap();
        assert!(decision.should_trigger);
    }

    #[tokio::test]
    async fn instance_key_overrides_memory_dir() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = AutoDreamGate::new();
        let mut inp = input(&memory_dir, DEFAULT_MIN_SESSIONS, false);
        inp.instance_key = "custom-key".to_string();
        let _ = gate.evaluate_gate(inp.clone()).await.unwrap();
        // The state under "custom-key" should have a non-zero scan timestamp.
        let state = gate.get_or_init_state("custom-key").await;
        assert!(state.last_session_scan_at_ms.load(Ordering::Acquire) > 0);
    }

    // ── Phase 0 parser ──
    #[test]
    fn phase0_parser_extracts_valid_and_stale_ids() {
        let raw = r#"{
  "still_valid_ids": ["insight_a", "insight_b"],
  "stale_ids": ["insight_c"],
  "notes": "consolidated"
}"#;
        let r = parse_phase0_json(raw).unwrap();
        assert_eq!(r.still_valid_ids, vec!["insight_a", "insight_b"]);
        assert_eq!(r.stale_ids, vec!["insight_c"]);
        assert_eq!(r.notes, "consolidated");
    }

    #[test]
    fn phase0_parser_handles_fenced_output() {
        let raw = "```json\n{\"still_valid_ids\":[],\"stale_ids\":[],\"notes\":\"\"}\n```";
        let r = parse_phase0_json(raw).unwrap();
        assert!(r.still_valid_ids.is_empty());
        assert!(r.stale_ids.is_empty());
    }

    #[test]
    fn phase0_parser_returns_none_on_invalid() {
        assert!(parse_phase0_json("not json").is_none());
    }

    // ── Phase 1 parser ──
    #[test]
    fn phase1_parser_extracts_themes() {
        let raw = r#"{"themes":[{"id":"t_alpha","label":"Alpha","rationale":"r1"},{"id":"t_beta","label":"Beta"}]}"#;
        let themes = parse_phase1_json(raw).unwrap();
        assert_eq!(themes.len(), 2);
        assert_eq!(themes[0].id, "t_alpha");
        assert_eq!(themes[1].label, "Beta");
    }

    #[test]
    fn phase1_parser_empty_themes_array() {
        let themes = parse_phase1_json(r#"{"themes":[]}"#).unwrap();
        assert!(themes.is_empty());
    }

    // ── Phase 2 parser ──
    #[test]
    fn phase2_parser_extracts_evidence() {
        let raw = r#"{"evidence_refs":[{"source":"sess-1","snippet":"quote one","weight":0.8},{"source":"sess-2","snippet":"quote two","weight":0.3}]}"#;
        let e = parse_phase2_json(raw).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].weight, 0.8);
        assert_eq!(e[1].snippet, "quote two");
    }

    // ── Phase 3 parser ──
    #[test]
    fn phase3_parser_extracts_block_with_confidence() {
        let raw = "---\nname: t_alpha\ntype: insight\ndescription: alpha\nconfidence: high\n---\n\nBody paragraph.\n\nSecond paragraph.";
        let b = parse_phase3_block(raw).unwrap();
        assert_eq!(b.confidence, "high");
        assert!(b.full_content.contains("name: t_alpha"));
        assert!(b.full_content.contains("Body paragraph"));
    }

    #[test]
    fn phase3_parser_defaults_confidence_to_medium() {
        let raw = "---\nname: x\ntype: insight\n---\n\nBody.";
        let b = parse_phase3_block(raw).unwrap();
        assert_eq!(b.confidence, "medium");
    }

    #[test]
    fn phase3_parser_returns_none_when_no_frontmatter() {
        assert!(parse_phase3_block("just text").is_none());
    }

    // ── Phase 4 parser ──
    #[test]
    fn phase4_parser_extracts_delete_ids() {
        let raw = r#"{"delete_ids":["f1","f2"]}"#;
        let ids = parse_phase4_json(raw).unwrap();
        assert_eq!(ids, vec!["f1", "f2"]);
    }

    // ── dreams/ scanner ──
    #[tokio::test]
    async fn scan_returns_empty_when_dir_absent() {
        let tmp = TempDir::new().unwrap();
        let dreams_dir = tmp.path().join("dreams-not-yet");
        let r = scan_dreams_files(&dreams_dir, "insight_").await.unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn scan_filters_by_prefix_and_extension() {
        let tmp = TempDir::new().unwrap();
        let dreams_dir = tmp.path().join("dreams");
        tokio::fs::create_dir_all(&dreams_dir).await.unwrap();
        tokio::fs::write(dreams_dir.join("insight_alpha.md"), "x")
            .await
            .unwrap();
        tokio::fs::write(dreams_dir.join("insight_beta.md"), "y")
            .await
            .unwrap();
        tokio::fs::write(dreams_dir.join("fragment_gamma.md"), "z")
            .await
            .unwrap();
        tokio::fs::write(dreams_dir.join("other.txt"), "ignore")
            .await
            .unwrap();

        let insights = scan_dreams_files(&dreams_dir, "insight_").await.unwrap();
        assert_eq!(insights.len(), 2);
        assert_eq!(insights[0].id, "alpha");
        assert_eq!(insights[1].id, "beta");

        let fragments = scan_dreams_files(&dreams_dir, "fragment_").await.unwrap();
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].id, "gamma");
    }

    // ── Processor: full 5-phase roundtrip ──
    /// Helper: spawn `process()` in a task, return both the join handle and
    /// a clone of the emitter for delivering canned responses.
    async fn await_first_emitted(emitter: &Arc<RecordingEmitter>) -> String {
        for _ in 0..100 {
            let recorded = emitter.recorded().await;
            if let Some(r) = recorded.first() {
                return r.req_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("emitter did not record any request");
    }

    async fn await_nth_emitted(emitter: &Arc<RecordingEmitter>, n: usize) -> String {
        for _ in 0..200 {
            let recorded = emitter.recorded().await;
            if recorded.len() > n {
                return recorded[n].req_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("emitter did not record {n}th request");
    }

    #[tokio::test]
    async fn processor_full_pipeline_writes_insight_and_runs_phase4() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        // Plant a stale fragment to give Phase 4 something to consider.
        let dreams_dir = memory_dir.join("dreams");
        tokio::fs::create_dir_all(&dreams_dir).await.unwrap();
        tokio::fs::write(dreams_dir.join("fragment_old.md"), "stale")
            .await
            .unwrap();

        let gate_payload = AutoDreamGateOutput {
            lock_path: lock::lock_path(&memory_dir),
            holder_pid: std::process::id(),
            prior_mtime_ms: 0,
            touched_session_count_at_trigger: 7,
        };
        let proc_input = DreamProcessInput {
            consumed_watermark_ms: None,
            memory_dir: memory_dir.clone(),
            gate_payload,
            recent_sessions_summary: "u: hi\na: discussed alpha".to_string(),
            memdir_manifest: "- user_pref [user] (user_pref.md)".to_string(),
            model_hint: None,
            params: LlmCallParams::default(),
            instance_key: String::new(),
        };

        let p_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { p_clone.process(proc_input).await });

        // Phase 0: deliver canned reflection.
        let p0_req = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0_req,
                response: Some(
                    r#"{"still_valid_ids":[],"stale_ids":["old"],"notes":""}"#.to_string(),
                ),
                usage: Some(LlmUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
                error: None,
            })
            .await;

        // Phase 1: deliver canned theme set.
        let p1_req = await_nth_emitted(&emitter, 1).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p1_req,
                response: Some(
                    r#"{"themes":[{"id":"theme_alpha","label":"Alpha topic"}]}"#.to_string(),
                ),
                usage: None,
                error: None,
            })
            .await;

        // Phase 2 (per theme): deliver canned evidence.
        let p2_req = await_nth_emitted(&emitter, 2).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p2_req,
                response: Some(
                    r#"{"evidence_refs":[{"source":"sess-1","snippet":"alpha details","weight":0.9}]}"#
                        .to_string(),
                ),
                usage: None,
                error: None,
            })
            .await;

        // Phase 3 (per theme): deliver canned insight block.
        let p3_req = await_nth_emitted(&emitter, 3).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p3_req,
                response: Some(
                    "---\nname: theme_alpha\ntype: insight\ndescription: alpha sum\nconfidence: high\n---\n\nAlpha consolidated body."
                        .to_string(),
                ),
                usage: None,
                error: None,
            })
            .await;

        // Phase 4: deliver delete_ids JSON.
        let p4_req = await_nth_emitted(&emitter, 4).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p4_req,
                response: Some(r#"{"delete_ids":["old"]}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;

        let output = task.await.unwrap().expect("process ok");
        assert_eq!(
            output.phases_completed,
            vec!["phase0", "phase1", "phase2", "phase3", "phase4"]
        );
        assert_eq!(output.theme_ids, vec!["theme_alpha"]);
        assert_eq!(output.insight_paths.len(), 1);
        assert!(output.insight_paths[0]
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .starts_with("insight_theme_alpha"));
        // Phase 4 should have pruned the stale fragment.
        assert_eq!(output.pruned_paths.len(), 1);
        assert!(output.pruned_paths[0]
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .starts_with("fragment_old"));

        // Verify the insight file is on disk.
        let insight = tokio::fs::read_to_string(&output.insight_paths[0])
            .await
            .unwrap();
        assert!(insight.contains("Alpha consolidated body"));
        // Stale fragment file is gone.
        assert!(!dreams_dir.join("fragment_old.md").exists());
    }

    #[tokio::test]
    async fn processor_no_themes_skips_phase2_3_runs_phase4_only() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        // Plant a FRESH fragment. D5: a fresh fragment with no Phase-0 stale
        // ids and no aged fragment gives the prune LLM nothing to decide — the
        // phase-4 LLM call must NOT fire (the old tautological gate burned one
        // here every run).
        let dreams_dir = memory_dir.join("dreams");
        tokio::fs::create_dir_all(&dreams_dir).await.unwrap();
        tokio::fs::write(dreams_dir.join("fragment_x.md"), "frag")
            .await
            .unwrap();

        let gate_payload = AutoDreamGateOutput {
            lock_path: lock::lock_path(&memory_dir),
            holder_pid: std::process::id(),
            prior_mtime_ms: 0,
            touched_session_count_at_trigger: 7,
        };
        let proc_input = DreamProcessInput {
            consumed_watermark_ms: None,
            memory_dir: memory_dir.clone(),
            gate_payload,
            recent_sessions_summary: "(nothing)".to_string(),
            memdir_manifest: "(empty)".to_string(),
            model_hint: None,
            params: LlmCallParams::default(),
            instance_key: String::new(),
        };

        let p_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { p_clone.process(proc_input).await });

        // Phase 0
        let p0 = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: Some(r#"{"still_valid_ids":[],"stale_ids":[],"notes":""}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;
        // Phase 1 — empty themes. Phase 4 then completes WITHOUT an LLM call
        // (D5 gate: no stale ids + no aged fragments), so the pipeline settles
        // right after this delivery.
        let p1 = await_nth_emitted(&emitter, 1).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p1,
                response: Some(r#"{"themes":[]}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;

        let output = task.await.unwrap().unwrap();
        assert_eq!(output.theme_ids, Vec::<String>::new());
        assert!(output.insight_paths.is_empty());
        assert!(output.fragment_paths.is_empty());
        assert!(
            output.pruned_paths.is_empty(),
            "fresh fragment must survive"
        );
        assert_eq!(output.phases_completed, vec!["phase0", "phase1", "phase4"]);
        // D5 regression: exactly two LLM round-trips (phase0 + phase1) — no
        // prune call was burned on a fragment with zero prune signal.
        assert_eq!(emitter.recorded().await.len(), 2);
        assert_eq!(output.req_ids.len(), 2);
        assert!(dreams_dir.join("fragment_x.md").exists());
    }

    #[tokio::test]
    async fn processor_low_confidence_block_lands_as_fragment() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let gate_payload = AutoDreamGateOutput {
            lock_path: lock::lock_path(&memory_dir),
            holder_pid: std::process::id(),
            prior_mtime_ms: 0,
            touched_session_count_at_trigger: 7,
        };
        let proc_input = DreamProcessInput {
            consumed_watermark_ms: None,
            memory_dir: memory_dir.clone(),
            gate_payload,
            recent_sessions_summary: "u: hi\na: hi".to_string(),
            memdir_manifest: String::new(),
            model_hint: None,
            params: LlmCallParams::default(),
            instance_key: String::new(),
        };
        let p_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { p_clone.process(proc_input).await });

        let p0 = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: Some(r#"{"still_valid_ids":[],"stale_ids":[],"notes":""}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;
        let p1 = await_nth_emitted(&emitter, 1).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p1,
                response: Some(r#"{"themes":[{"id":"weak_theme","label":"Weak"}]}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;
        let p2 = await_nth_emitted(&emitter, 2).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p2,
                response: Some(
                    r#"{"evidence_refs":[{"source":"sess-1","snippet":"tiny","weight":0.2}]}"#
                        .to_string(),
                ),
                usage: None,
                error: None,
            })
            .await;
        let p3 = await_nth_emitted(&emitter, 3).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p3,
                response: Some(
                    "---\nname: weak_theme\ntype: insight\ndescription: weak\nconfidence: low\n---\n\nWeak body."
                        .to_string(),
                ),
                usage: None,
                error: None,
            })
            .await;
        // Phase 4 — the just-written fragment is FRESH and Phase 0 flagged no
        // stale ids, so D5 skips the prune LLM call; the pipeline settles here.

        let output = task.await.unwrap().unwrap();
        // Low-confidence → fragment, not insight.
        assert!(output.insight_paths.is_empty());
        assert_eq!(output.fragment_paths.len(), 1);
        let frag_name = output.fragment_paths[0]
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        assert!(frag_name.starts_with("fragment_weak_theme"));
        // D5: only phase0/1/2/3 round-trips — no prune call for a fresh
        // fragment with zero prune signal.
        assert_eq!(emitter.recorded().await.len(), 4);
    }

    #[tokio::test]
    async fn processor_phase0_llm_failure_propagates() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let gate_payload = AutoDreamGateOutput {
            lock_path: lock::lock_path(&memory_dir),
            holder_pid: std::process::id(),
            prior_mtime_ms: 0,
            touched_session_count_at_trigger: 7,
        };
        let proc_input = DreamProcessInput {
            consumed_watermark_ms: None,
            memory_dir: memory_dir.clone(),
            gate_payload,
            recent_sessions_summary: String::new(),
            memdir_manifest: String::new(),
            model_hint: None,
            params: LlmCallParams::default(),
            instance_key: String::new(),
        };
        let p_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { p_clone.process(proc_input).await });

        let p0 = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: None,
                usage: None,
                error: Some("rate limited".to_string()),
            })
            .await;

        let result = task.await.unwrap();
        assert!(matches!(result, Err(DreamProcessError::LlmFailure(_))));
    }

    // ── A3 fix (P0-3): process() settles the real .consolidate-lock ──
    //
    // Mirrors `result_listener.rs` ~226-283 style: drive a full `process()`
    // run via the run_dream_tick / spawn_dream_now shape (process() called
    // DIRECTLY, bypassing ResultListener), then assert the lock is
    // re-acquirable. Before A3 the lock stayed held until ~1h stale → every
    // subsequent gate returned `lock_held` (self-deadlocked dreaming).

    /// Helper: pre-seed the consolidate-lock exactly as the real
    /// `AutoDreamGate` / `evaluate_dream_run_now` acquire does — body = our
    /// live PID — so `process()`'s settle recognizes us as the genuine holder.
    async fn seed_real_lock(memory_dir: &Path, prior_mtime_secs: i64) {
        use filetime::{set_file_mtime, FileTime};
        tokio::fs::create_dir_all(memory_dir).await.unwrap();
        tokio::fs::write(lock::lock_path(memory_dir), std::process::id().to_string())
            .await
            .unwrap();
        set_file_mtime(
            lock::lock_path(memory_dir),
            FileTime::from_unix_time(prior_mtime_secs, 0),
        )
        .unwrap();
    }

    /// Drive a minimal full pipeline (no themes → phase0/1/4 only) and deliver
    /// canned LLM results. Returns the `process()` join result.
    async fn drive_no_theme_dream(
        processor: &Arc<DreamProcessor>,
        emitter: &Arc<RecordingEmitter>,
        proc_input: DreamProcessInput,
    ) -> Result<DreamProcessOutput, DreamProcessError> {
        let p = Arc::clone(processor);
        let task = tokio::spawn(async move { p.process(proc_input).await });

        let p0 = await_first_emitted(emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: Some(r#"{"still_valid_ids":[],"stale_ids":[],"notes":""}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;
        // Phase 1: empty themes → phase2/3 skipped; phase4 has no fragments to
        // discuss so it issues no LLM call → pipeline finishes after phase1.
        let p1 = await_nth_emitted(emitter, 1).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p1,
                response: Some(r#"{"themes":[]}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;

        task.await.unwrap()
    }

    fn proc_input_for(memory_dir: &Path, prior_mtime_ms: u64) -> DreamProcessInput {
        DreamProcessInput {
            consumed_watermark_ms: None,
            memory_dir: memory_dir.to_path_buf(),
            gate_payload: AutoDreamGateOutput {
                lock_path: lock::lock_path(memory_dir),
                holder_pid: std::process::id(),
                prior_mtime_ms,
                touched_session_count_at_trigger: 7,
            },
            recent_sessions_summary: String::new(),
            memdir_manifest: String::new(),
            model_hint: None,
            params: LlmCallParams::default(),
            instance_key: String::new(),
        }
    }

    #[tokio::test]
    async fn process_success_releases_lock_with_fresh_mtime_and_is_reacquirable() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        // Prior consolidation long ago (2023) — a successful release must NOT
        // roll back to this; it must stamp a FRESH mtime (else the time-gate
        // re-arms to fire immediately = tight-loop dreaming).
        seed_real_lock(&memory_dir, 1_700_000_000).await;

        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let output = drive_no_theme_dream(
            &processor,
            &emitter,
            proc_input_for(&memory_dir, 1_700_000_000_000),
        )
        .await
        .expect("process ok");
        assert_eq!(output.phases_completed, vec!["phase0", "phase1", "phase4"]);

        // Lock body is the empty release sentinel.
        assert_eq!(
            std::fs::read_to_string(lock::lock_path(&memory_dir)).unwrap(),
            ""
        );
        // mtime is FRESH (not the seeded 2023 value).
        let now = now_ms();
        let after = lock::last_consolidated_at(&memory_dir).await.unwrap();
        assert!(
            after > 1_700_000_000_000 && now.saturating_sub(after) < 60_000,
            "success must stamp a fresh mtime, got {after} (now {now})"
        );
        // Re-acquirable by a fresh acquirer.
        let reacq = lock::try_acquire(&memory_dir).await.unwrap();
        assert!(reacq.is_some(), "lock must be re-acquirable after success");
    }

    #[tokio::test]
    async fn process_failure_rolls_lock_back_to_prior_mtime_and_is_reacquirable() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        seed_real_lock(&memory_dir, 1_800_000_000).await;

        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let proc_input = proc_input_for(&memory_dir, 1_700_000_000_000);
        let p = Arc::clone(&processor);
        let task = tokio::spawn(async move { p.process(proc_input).await });
        let p0 = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: None,
                usage: None,
                error: Some("rate limited".to_string()),
            })
            .await;
        let result = task.await.unwrap();
        assert!(matches!(result, Err(DreamProcessError::LlmFailure(_))));

        // Failure → rollback to the PRIOR mtime (the gate_payload value), NOT a
        // fresh stamp — so the next scheduled dream re-attempts on cadence.
        assert_eq!(
            lock::last_consolidated_at(&memory_dir).await.unwrap(),
            1_700_000_000_000
        );
        let reacq = lock::try_acquire(&memory_dir).await.unwrap();
        assert!(reacq.is_some(), "lock must be re-acquirable after failure");
    }

    #[tokio::test]
    async fn process_does_not_clobber_lock_when_we_are_not_the_holder() {
        // forced_skip_lock virtual-acquire shape: the lock file holds a FOREIGN
        // pid (the gate took no real lock). settle MUST be a NO-OP — never
        // touch a foreign holder's lock.
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let foreign_pid = std::process::id().wrapping_add(1).to_string();
        tokio::fs::write(lock::lock_path(&memory_dir), &foreign_pid)
            .await
            .unwrap();

        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        drive_no_theme_dream(&processor, &emitter, proc_input_for(&memory_dir, 0))
            .await
            .expect("process ok");

        // Foreign holder untouched (no fresh-mtime release, no rollback).
        assert_eq!(
            std::fs::read_to_string(lock::lock_path(&memory_dir)).unwrap(),
            foreign_pid
        );
    }

    #[tokio::test]
    async fn processor_deliver_unknown_req_id_returns_false() {
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = DreamProcessor::new(Arc::clone(&gate), emitter as Arc<dyn LlmCallEmitter>);
        let r = processor
            .deliver_result(LlmCallResultPayload {
                req_id: "unknown-tier3".to_string(),
                response: Some("hi".to_string()),
                usage: None,
                error: None,
            })
            .await;
        assert!(!r);
    }

    #[tokio::test]
    async fn req_id_uses_tier3_prefix_with_phase() {
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = DreamProcessor::new(
            Arc::clone(&gate),
            Arc::clone(&emitter) as Arc<dyn LlmCallEmitter>,
        );
        // Trigger one synthetic round-trip by calling the LLM helper, then
        // dropping the pending entry (simulated shutdown).
        let processor = Arc::new(processor);
        let p_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move {
            p_clone
                .call_llm(
                    "phaseX",
                    vec![LlmMessage {
                        role: "user".to_string(),
                        content: "hi".to_string(),
                    }],
                    None,
                    LlmCallParams::default(),
                )
                .await
        });
        let req_id = await_first_emitted(&emitter).await;
        assert!(req_id.starts_with("tier3-phaseX-"), "got: {req_id}");
        // Cancel by closing pending sender.
        {
            let mut map = processor.pending.lock().await;
            for (_id, tx) in map.drain() {
                drop(tx);
            }
        }
        let _ = task.await.unwrap();
    }

    // ── TierGate trait shape ──
    #[tokio::test]
    async fn tier_gate_trait_decision_shape_round_trip() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = AutoDreamGate::new();
        let d = gate
            .evaluate_gate(input(&memory_dir, 0, false))
            .await
            .unwrap();
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["should_trigger"], false);
        // session_count gate fails before scan throttle records; scan
        // throttle field is `scan_throttled` on second call only. On first
        // call we expect session_count_unmet (touched=0, min=5).
        assert_eq!(json["skip_reason"], "session_count_unmet");
    }

    // ── Phase 4 stale fragment age-based pruning ──
    #[tokio::test]
    async fn phase4_age_based_prunes_old_fragments_without_llm_delete_id() {
        // Plant a fragment whose mtime is in the distant past (older than
        // FRAGMENT_STALE_MS). The processor's `run_phase4_prune` should
        // pick it up via age, even if the LLM returns an empty delete list.
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let dreams_dir = memory_dir.join("dreams");
        tokio::fs::create_dir_all(&dreams_dir).await.unwrap();
        let stale = dreams_dir.join("fragment_ancient.md");
        tokio::fs::write(&stale, "old").await.unwrap();
        // Set mtime to a far-past timestamp.
        filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();

        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let gate_payload = AutoDreamGateOutput {
            lock_path: lock::lock_path(&memory_dir),
            holder_pid: std::process::id(),
            prior_mtime_ms: 0,
            touched_session_count_at_trigger: 0,
        };
        let proc_input = DreamProcessInput {
            consumed_watermark_ms: None,
            memory_dir: memory_dir.clone(),
            gate_payload,
            recent_sessions_summary: String::new(),
            memdir_manifest: String::new(),
            model_hint: None,
            params: LlmCallParams::default(),
            instance_key: String::new(),
        };
        let p_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { p_clone.process(proc_input).await });

        let p0 = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: Some(r#"{"still_valid_ids":[],"stale_ids":[],"notes":""}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;
        let p1 = await_nth_emitted(&emitter, 1).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p1,
                response: Some(r#"{"themes":[]}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;
        let p4 = await_nth_emitted(&emitter, 2).await;
        // LLM returns empty delete_ids — age path must still prune.
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p4,
                response: Some(r#"{"delete_ids":[]}"#.to_string()),
                usage: None,
                error: None,
            })
            .await;

        let output = task.await.unwrap().unwrap();
        assert_eq!(output.pruned_paths.len(), 1);
        assert!(!stale.exists(), "stale fragment must have been removed");
    }

    // ── D6: parse failures are counted + daily-logged (no silent swallow) ──
    #[tokio::test]
    async fn parse_failures_are_counted_and_daily_logged() {
        let tmp = TempDir::new().unwrap();
        // memory_dir = <project_state_dir>/memory so the daily log lands in
        // <project_state_dir>/.memory-rust-derived (production layout).
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let p_clone = Arc::clone(&processor);
        let proc_input = proc_input_for(&memory_dir, 0);
        let task = tokio::spawn(async move { p_clone.process(proc_input).await });

        // Phase 0 + Phase 1 both return garbage — fail-soft continues on
        // defaults, but each failure is counted + daily-logged.
        let p0 = await_first_emitted(&emitter).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p0,
                response: Some("definitely not json".to_string()),
                usage: None,
                error: None,
            })
            .await;
        let p1 = await_nth_emitted(&emitter, 1).await;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: p1,
                response: Some("still not json".to_string()),
                usage: None,
                error: None,
            })
            .await;

        let output = task.await.unwrap().expect("pipeline stays fail-soft");
        assert_eq!(output.phases_completed, vec!["phase0", "phase1", "phase4"]);
        assert_eq!(
            output.parse_failures, 2,
            "phase0 + phase1 both failed to parse"
        );

        // The daily log carries one parse_failure record per failed phase.
        let logs_root = tmp.path().join(".memory-rust-derived").join("logs");
        let mut bodies = String::new();
        for entry in walkdir::WalkDir::new(&logs_root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                bodies.push_str(&std::fs::read_to_string(entry.path()).unwrap());
            }
        }
        assert!(
            bodies.contains(r#""kind":"memory.dream.parse_failure""#),
            "daily log must record parse failures, got: {bodies}"
        );
        assert!(bodies.contains(r#""phase":"phase0""#), "log: {bodies}");
        assert!(bodies.contains(r#""phase":"phase1""#), "log: {bodies}");
    }

    #[tokio::test]
    async fn clean_run_reports_zero_parse_failures() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(AutoDreamGate::new());
        let emitter: Arc<RecordingEmitter> = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(DreamProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let output = drive_no_theme_dream(&processor, &emitter, proc_input_for(&memory_dir, 0))
            .await
            .expect("process ok");
        assert_eq!(output.parse_failures, 0);
        assert!(
            !tmp.path()
                .join(".memory-rust-derived")
                .join("logs")
                .exists(),
            "clean run writes no parse_failure records"
        );
    }

    // ── W-MEMORY-SYNERGY W6 (2026-07-16) ──────────────────────────────────

    /// 6c — importance_pressure 豁免时间门（其余 gate 不放宽）：新 lock
    /// （时间门必拒）+ 压力位 → 触发；无压力位 → time_gate_unmet。
    #[tokio::test]
    async fn w6_importance_pressure_bypasses_time_gate_only() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        // 新 lock：时间门必拒（真实时钟语义）。
        lock::record_consolidation_complete(&memory_dir)
            .await
            .unwrap();

        let gate = AutoDreamGate::new();
        let mut gate_in = input(&memory_dir, 5, false);
        gate_in.instance_key = "w6-pressure-off".to_string();
        let decision = gate.evaluate_gate(gate_in).await.unwrap();
        assert_eq!(
            decision.skip_reason.as_deref(),
            Some("time_gate_unmet"),
            "无压力位时时间门照拒"
        );

        let mut gate_in = input(&memory_dir, 5, false);
        gate_in.instance_key = "w6-pressure-on".to_string();
        gate_in.importance_pressure = true;
        let decision = gate.evaluate_gate(gate_in).await.unwrap();
        assert!(
            decision.should_trigger,
            "压力位豁免时间门后其余 gate（会话数满足、锁可得）放行，got {:?}",
            decision.skip_reason
        );
    }

    /// 6b — dreams/ 正文级精确去重：不同文件名相同 body 拦下；目标文件
    /// 自身（幂等覆盖）与不同 body 放行。
    #[tokio::test]
    async fn w6_dreams_duplicate_body_detection() {
        let tmp = TempDir::new().unwrap();
        let dreams = tmp.path().join("dreams");
        tokio::fs::create_dir_all(&dreams).await.unwrap();
        let content = "---\ntype: insight\n---\n用户偏好暗色主题\n";
        tokio::fs::write(dreams.join("insight_existing.md"), content)
            .await
            .unwrap();

        let new_target = dreams.join("insight_other.md");
        assert!(
            dreams_dir_has_duplicate_body(&dreams, &new_target, content).await,
            "不同文件名相同 body 必须判重"
        );
        assert!(
            !dreams_dir_has_duplicate_body(&dreams, &dreams.join("insight_existing.md"), content)
                .await,
            "目标自身除外（幂等覆盖保持允许）"
        );
        assert!(
            !dreams_dir_has_duplicate_body(
                &dreams,
                &new_target,
                "---\ntype: insight\n---\n完全不同的内容\n"
            )
            .await
        );
    }

    /// W-MEMORY-SELF-EVOLVE-DGM G3-c — dreams/ 近重复（词集 Jaccard）：
    /// 换措辞的同一洞见拦下；短文本护栏与 1.0 关闭门放行；不同主题放行。
    #[tokio::test]
    async fn g3c_dreams_near_duplicate_body_detection() {
        let tmp = TempDir::new().unwrap();
        let dreams = tmp.path().join("dreams");
        tokio::fs::create_dir_all(&dreams).await.unwrap();
        let existing = "---\ntype: insight\n---\n\
            用户在项目里偏好暗色主题界面 交付前必须先跑完整测试套件 \
            数据层调度放在 Rust 侧 业务与模型调用放在 TS 侧\n";
        tokio::fs::write(dreams.join("insight_existing.md"), existing)
            .await
            .unwrap();
        let target = dreams.join("insight_new.md");

        // 换措辞但词集高度重叠（去掉一小截、语序微调）→ 近重复。
        let paraphrase = "---\ntype: insight\n---\n\
            用户在项目里偏好暗色主题界面 交付前必须先跑完整测试套件 \
            数据层调度放在 Rust 侧 业务与模型调用放在 TS\n";
        assert!(
            dreams_dir_has_near_duplicate_body(&dreams, &target, paraphrase, 0.85).await,
            "高 Jaccard 换措辞必须判近重复"
        );
        // threshold = 1.0 = 关闭（词集有差异就不判）。
        assert!(
            !dreams_dir_has_near_duplicate_body(&dreams, &target, paraphrase, 1.0).await,
            "1.0 = 关闭近重复门"
        );
        // 完全不同主题 → 放行。
        let different = "---\ntype: insight\n---\n\
            浏览器扩展的站点策略需要可信 host 白名单 急停开关必须持久化 \
            版本握手失败时提示用户重装同构建产物\n";
        assert!(!dreams_dir_has_near_duplicate_body(&dreams, &target, different, 0.85).await);
        // 短文本护栏：词集 < 12 不判（假阳性高）。
        assert!(
            !dreams_dir_has_near_duplicate_body(
                &dreams,
                &target,
                "---\ntype: insight\n---\n暗色主题\n",
                0.5,
            )
            .await,
            "短文本不判近重复"
        );
    }

    // ── D8: theme filename hash suffix ──
    #[test]
    fn theme_filename_appends_short_hash_and_disambiguates_sanitize_collisions() {
        // Same theme id → identical filename (idempotent overwrite preserved).
        assert_eq!(
            theme_filename("insight_", "theme_alpha"),
            theme_filename("insight_", "theme_alpha")
        );

        // Two theme ids that sanitize to the SAME stem must no longer collide.
        let a = theme_filename("insight_", "a/b");
        let b = theme_filename("insight_", "a b");
        assert_eq!(
            sanitize_name("a/b"),
            sanitize_name("a b"),
            "collision premise"
        );
        assert_ne!(a, b, "sanitize-colliding theme ids must get distinct files");

        // Shape: prefix + sanitized stem + `_` + 8 hex chars + `.md`.
        assert!(a.starts_with("insight_a_b_"), "got: {a}");
        assert!(a.ends_with(".md"));
        let hash_part = a
            .strip_prefix("insight_a_b_")
            .and_then(|s| s.strip_suffix(".md"))
            .unwrap();
        assert_eq!(hash_part.len(), 8, "8-hex short hash, got: {hash_part}");
        assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()));

        // The prefix contract `collect_tier_files` / `scan_dreams_files`
        // matches on is untouched.
        assert!(theme_filename("fragment_", "weak_theme").starts_with("fragment_weak_theme_"));
    }

    // ── prompt template smoke ──
    #[test]
    fn prompt_templates_have_required_placeholders() {
        assert!(TIER3_DREAM_PHASE0_PROMPT.contains("{{prior_insights}}"));
        assert!(TIER3_DREAM_PHASE1_ORIENT_PROMPT.contains("{{session_summary}}"));
        assert!(TIER3_DREAM_PHASE1_ORIENT_PROMPT.contains("{{memdir_summary}}"));
        assert!(TIER3_DREAM_PHASE2_GATHER_PROMPT.contains("{{theme_id}}"));
        assert!(TIER3_DREAM_PHASE2_GATHER_PROMPT.contains("{{session_excerpts}}"));
        assert!(TIER3_DREAM_PHASE3_CONSOLIDATE_PROMPT.contains("{{theme_id}}"));
        assert!(TIER3_DREAM_PHASE3_CONSOLIDATE_PROMPT.contains("{{evidence_list}}"));
        assert!(TIER3_DREAM_PHASE4_PRUNE_PROMPT.contains("{{stale_ids}}"));
        assert!(TIER3_DREAM_PHASE4_PRUNE_PROMPT.contains("{{fragment_list}}"));
    }

    #[test]
    fn prompt_templates_carry_no_brand_literals() {
        // Sanity guard: §硬约束 #1 forbids any model brand / family literal
        // in Rust prompt constants. Keep the search broad.
        let banned = [
            "claude",
            "gpt-",
            "deepseek",
            "qwen",
            "gemini",
            "anthropic",
            "openai",
        ];
        for prompt in [
            TIER3_DREAM_PHASE0_PROMPT,
            TIER3_DREAM_PHASE1_ORIENT_PROMPT,
            TIER3_DREAM_PHASE2_GATHER_PROMPT,
            TIER3_DREAM_PHASE3_CONSOLIDATE_PROMPT,
            TIER3_DREAM_PHASE4_PRUNE_PROMPT,
        ] {
            let lower = prompt.to_lowercase();
            for b in banned {
                assert!(
                    !lower.contains(b),
                    "prompt contains banned brand literal {b:?}"
                );
            }
        }
    }

    // ── sanitize_name ──
    #[test]
    fn sanitize_name_replaces_unsafe_chars() {
        assert_eq!(sanitize_name("ok_id"), "ok_id");
        assert_eq!(sanitize_name("ok-1.2"), "ok-1.2");
        assert_eq!(sanitize_name("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_name("with space"), "with_space");
    }
}
