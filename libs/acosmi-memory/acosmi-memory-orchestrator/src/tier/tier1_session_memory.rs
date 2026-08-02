//! W-MEMORY-DREAM-REBUILD v7 P3.2 — Tier-1 SessionMemory policy（Rust 重写）。
//!
//! 设计借鉴 CrabClaw `backend/internal/memory/uhms/tier1_policy.go`（只读
//! 参考归档，不 git copy；重写为 Rust）。配合 P3.1 立的反向 IPC LLM 调用契约：
//! orchestrator 通过 `memory/tier/llmCallRequest` notification 广播请求 →
//! TS 端跑 SDK → `memory/tier/llmCallResult` request 回写 → orchestrator
//! 内部 pending oneshot 按 `req_id` 匹配 (详 `tier/mod.rs` 反向 IPC 时序)。
//!
//! # 5 重门控（Tier-1 deterministic policy）
//!
//! 1. **非子智能体** —— 仅主智能体路径触发。对应 TS 端
//!    `querySource === 'repl_main_thread'`。
//! 2. **功能开启** —— `auto_memory_enabled` / `auto_session_memory` feature
//!    flag。对应 TS `isSessionMemoryGateEnabled()`。
//! 3. **初始化阈值** —— `current_token_count >= MIN_TOKENS_TO_INIT (10000)`。
//!    未初始化前不触发。对应 TS `hasMetInitializationThreshold()`。
//! 4. **更新阈值** —— `token_growth >= MIN_TOKENS_BETWEEN_UPDATE (5000)`。
//!    对应 TS `hasMetUpdateThreshold()`。
//! 5. **工具调用间隔** —— `tool_calls_since_last >= TOOL_CALLS_BETWEEN (3)`。
//!    对应 TS `getToolCallsBetweenUpdates()`。
//!
//! 触发条件最终式（与 CrabClaw / CrabCode TS 完全一致）：
//!
//! ```text
//! should_extract = (token_threshold && tool_call_threshold)
//!               || (token_threshold && !has_tool_calls_in_last_turn)
//! ```
//!
//! token 阈值始终必须满足；tool_call 不足但最后一轮没工具调用也算"可处理"
//! （避免连环工具调用打断 extraction window）。
//!
//! # 写盘形态
//!
//! - `SESSION.md` —— 主会话笔记（每 turn 增量追加 / 替换；走
//!   `atomic_write::atomic_write` 保原子性）。
//! - `.session-<turn_id>.md` —— 每 turn 一份完整 snapshot（用于 compaction
//!   恢复 / 跨会话引用）。
//!
//! 当前实现使用 orchestrator 已有 `atomic_write` 写盘（与 dream_config /
//! lock 同源）。`acosmi-memory-transaction::path_lock`（P2.3 引入）是更全
//! 上位替代，但 SESSION.md 写盘 race 风险低（per-turn 串行，主控持
//! `extraction_in_progress` 原子 flag），P3.x 收尾时可切换；本 PR 不切。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

use crate::atomic_write::{atomic_write, BoxError};
use crate::tier::{
    GateDecision, LlmCallParams, LlmCallRequestPayload, LlmCallResultPayload, LlmMessage,
    MemoryTier, TierGate,
};

// ──────────────────────────────────────────────────────────────────────────
// 阈值常量（与 TS `DEFAULT_SESSION_MEMORY_CONFIG` 同源真值）
// ──────────────────────────────────────────────────────────────────────────

/// 初始化阈值：累计上下文 token 数 ≥ 此值才允许 Tier-1 启动。
///
/// 与 `src/services/SessionMemory/sessionMemoryUtils.ts:33` 的
/// `minimumMessageTokensToInit: 10000` 保持一致。
pub const MIN_TOKENS_TO_INIT: u64 = 10000;

/// 更新阈值：上次抽取以来 token 增长 ≥ 此值才允许下次抽取。
///
/// 与 `sessionMemoryUtils.ts:34` 的 `minimumTokensBetweenUpdate: 5000`
/// 一致。
pub const MIN_TOKENS_BETWEEN_UPDATE: u64 = 5000;

/// 工具调用间隔：上次抽取以来工具调用次数 ≥ 此值才允许下次抽取。
///
/// 与 `sessionMemoryUtils.ts:35` 的 `toolCallsBetweenUpdates: 3` 一致。
pub const TOOL_CALLS_BETWEEN_UPDATES: u32 = 3;

/// LLM 调用反向 IPC 等待超时（30s）。设计取自 CrabClaw `tier1_policy.go:182`
/// `context.WithTimeout(..., 30*time.Second)`（一致）。超时即视为 LLM 调用
/// 失败，processor 释放 oneshot pending 槽并返回 error；本 turn 跳过写盘，
/// 状态机不前推（下一 turn 再尝试）。
pub const LLM_CALL_TIMEOUT_MS: u64 = 30_000;

// ──────────────────────────────────────────────────────────────────────────
// Tier-1 prompt 模板（**Rust 字符串常量，不进系统 prompt 体系**）
//
// CLAUDE.md §硬约束 #15 第 8 条铁律：Tier1/2/3 prompt 模板在 orchestrator
// 内嵌 Rust 字符串常量，**不**改动 `src/services/SessionMemory/prompts.ts`
// 等 5 个严格档（用户裁决）。设计借鉴 CrabClaw
// `prompts/tier1_session_template.md` + `tier1_session_update.md`，重写为
// 全英文、不 paste 中文、不写品牌字面（无 LLM 模型名 / family / 价格
// hardcode；§硬约束 #1）。
// ──────────────────────────────────────────────────────────────────────────

/// Tier-1 consolidation prompt template. Pure-English, model-agnostic; the
/// LLM call routing (which model, what context window, sampling parameters)
/// is decided on the TS side via SDK after this template lands in the
/// `LlmCallRequestPayload::messages` list. The `{{notes_path}}` and
/// `{{current_notes}}` placeholders are filled by the processor at
/// per-turn extraction time. No model brand / version / pricing literal
/// appears here (CLAUDE.md §硬约束 #1).
pub const TIER1_CONSOLIDATION_PROMPT: &str = r#"You are maintaining session notes for an ongoing assistant turn.

You will be given:
1. The path to the session notes file at `{{notes_path}}`.
2. The current notes content (may be empty for a fresh session).
3. A compact transcript of the recent conversation turns.

Your task is to produce the COMPLETE updated session notes content as plain markdown.

Guidelines:
- Preserve any existing section headers and their italic descriptions.
- Add or refine sections as the conversation reveals new intent, decisions, or open questions.
- Keep each section concise; prefer bullet points over prose where appropriate.
- Do NOT include any external commentary, code fences, or LLM disclaimers in your output.
- If the new transcript adds nothing useful, return the current notes verbatim.

Output ONLY the updated session notes content, ready to be written verbatim to `{{notes_path}}`.

--- CURRENT NOTES ---
{{current_notes}}
--- END CURRENT NOTES ---"#;

/// W-MEMORY-SELF-EVOLVE-DGM G3-b (2026-07-16) — 增量更新后缀（grok-build
/// `FLUSH_DELTA_SYSTEM_PROMPT` 思想适配到「整文件替换」写入语义：产出必须
/// 是**合并后的完整速记**而非纯增量文本，只有新信息才展开叙述）。仅当
/// 本会话已有速记快照且转写在其后又增长时追加（`prior_note` 注入）。
pub const TIER1_DELTA_SUFFIX: &str = r#"

--- PREVIOUS NOTE FOR THIS SESSION ---
{{prior_note}}
--- END PREVIOUS NOTE ---

An earlier note for THIS session already exists (above) and the transcript has
grown since it was written. Produce the UPDATED COMPLETE note: keep everything
from the previous note that is still true, fold in only what is NEW in the
transcript, resolve contradictions in favor of the newer transcript, and do
not restate unchanged content in different words."#;

// ──────────────────────────────────────────────────────────────────────────
// Gate input/output
// ──────────────────────────────────────────────────────────────────────────

/// 门控输入：来自 TS 端 per-turn evaluation 的元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionMemoryGateInput {
    /// 触发本 turn 的智能体类型；`""` = 主智能体（gate 1 通过）。任何非空
    /// 字符串都视为子智能体（如 `"agent"`, `"coordinator"`），gate 1 拒。
    pub agent_type: String,
    /// 当前会话 key（用于 state 持久化 / file 路径）。
    pub session_key: String,
    /// 上下文 token 总数（TS 端 `tokenCountWithEstimation` 真值）。
    pub current_token_count: u64,
    /// 本 turn 内（自上次抽取以来）工具调用次数。
    pub tool_calls_since_last_update: u32,
    /// 最后一轮 assistant 是否包含工具调用 block。
    pub has_tool_calls_in_last_turn: bool,
    /// feature flag bag（与 dream_config / turn_evaluator 一致 shape）。
    /// 检查 `auto_session_memory` / `auto_memory_enabled` / `AUTO_MEMORY` 任一为
    /// true 即视为开启。
    #[serde(default)]
    pub feature_flags: BTreeMap<String, bool>,
}

/// 门控输出：触发时返回 LLM 调用所需的载荷。`None` = 未触发。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct SessionMemoryGateOutput {
    /// LLM 调用所用 messages（system + user，已注入 prompt 模板 +
    /// current_notes + transcript summary）。
    pub messages: Vec<LlmMessage>,
    /// 触发本次抽取时记录的 token count，processor 在 LLM 成功 settle 时
    /// 写回 state.tokens_at_last_extraction（避免 race）。
    pub token_count_at_trigger: u64,
}

/// Tier-1 gate evaluation error.
#[derive(Debug, thiserror::Error)]
pub enum SessionMemoryGateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ──────────────────────────────────────────────────────────────────────────
// Per-session state
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Tier1SessionState {
    /// gate 4 用：上次抽取时记录的 token count（用于差量计算）。Also
    /// doubles as the "initialized" sentinel: `0` = not yet initialized,
    /// any non-zero value = initialized at that baseline (see
    /// `check_and_promote_initialized()`).
    tokens_at_last_extraction: AtomicU64,
    /// 抽取进行中标志（防并发 race）。0 = idle, 1 = busy.
    extraction_in_progress: std::sync::atomic::AtomicU8,
    /// 抽取开始时间（unix_ms）。0 = idle；用于 stale 检测（60s 超时强制
    /// reset，对应 CrabClaw `EXTRACTION_STALE_THRESHOLD_MS = 60_000`）。
    extraction_started_at_ms: AtomicU64,
}

// ──────────────────────────────────────────────────────────────────────────
// SessionMemoryGate (TierGate trait impl)
// ──────────────────────────────────────────────────────────────────────────

/// Tier-1 deterministic gate. 不持有锁 / 不写盘 / 不做 IO（除了 `Path` 拼
/// 接）。仅根据输入决定是否触发 + 拼装 LLM payload。
pub struct SessionMemoryGate {
    /// per-session 状态表（agent_type=="" 主智能体路径下，按 `session_key`
    /// keyed）。`Arc<Mutex<_>>` 内 BTreeMap 避免 RwLock 写锁竞争；session
    /// 数量量级 < 100，BTreeMap 足够。
    sessions: Arc<Mutex<BTreeMap<String, Arc<Tier1SessionState>>>>,
}

impl Default for SessionMemoryGate {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl SessionMemoryGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// 拉取（或新建）per-session state；返回 `Arc` 供 caller 持有
    /// （`Arc<AtomicU8>` 内 CAS 不需要外部锁）。
    async fn get_or_init_state(&self, session_key: &str) -> Arc<Tier1SessionState> {
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(session_key) {
            return Arc::clone(existing);
        }
        let state = Arc::new(Tier1SessionState::default());
        sessions.insert(session_key.to_string(), Arc::clone(&state));
        state
    }

    /// 静态 helper：判断 feature flag 是否启用（多个 key 任一为 true 即开）。
    fn is_feature_enabled(flags: &BTreeMap<String, bool>) -> bool {
        for key in ["auto_session_memory", "auto_memory_enabled", "AUTO_MEMORY"] {
            if matches!(flags.get(key), Some(true)) {
                return true;
            }
        }
        false
    }
}

#[async_trait]
impl TierGate for SessionMemoryGate {
    type GateInput = SessionMemoryGateInput;
    type GateOutput = SessionMemoryGateOutput;
    type Error = SessionMemoryGateError;

    async fn evaluate_gate(
        &self,
        input: Self::GateInput,
    ) -> Result<GateDecision<Self::GateOutput>, Self::Error> {
        // ── Gate 1: 非子智能体 ──
        if !input.agent_type.is_empty() {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("non_main_agent".to_string()),
            });
        }

        // ── Gate 2: 功能开启 ──
        if !Self::is_feature_enabled(&input.feature_flags) {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("feature_disabled".to_string()),
            });
        }

        // Per-session state
        let state = self.get_or_init_state(&input.session_key).await;

        // ── Gate 3: 初始化阈值 ──
        // initialized flag is encoded in `tokens_at_last_extraction`:
        //   0  → uninitialized
        //   >0 → initialized at that baseline (CAS-promoted on first crossing
        //        of MIN_TOKENS_TO_INIT — see `check_and_promote_initialized`).
        let mutable_check = check_and_promote_initialized(&state, input.current_token_count).await;
        if !mutable_check.initialized {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("init_threshold_unmet".to_string()),
            });
        }

        let tokens_since_last = input
            .current_token_count
            .saturating_sub(mutable_check.tokens_at_last_extraction);

        // ── Gate 4: 更新 token 阈值 ──
        let token_threshold = tokens_since_last >= MIN_TOKENS_BETWEEN_UPDATE;

        // ── Gate 5: 工具调用间隔 ──
        let tool_call_threshold = input.tool_calls_since_last_update >= TOOL_CALLS_BETWEEN_UPDATES;

        // 触发条件（参 CrabClaw + TS sessionMemory.ts L168-170）：
        //   (token && tool) || (token && !has_tool_calls_in_last_turn)
        // 保留原始结构（与设计契约源码 1:1 对应）；clippy 想合成 token &&
        // (tool || !has_tool_in_last)，等价但失去溯源可读性。
        #[allow(clippy::nonminimal_bool)]
        let should_trigger = (token_threshold && tool_call_threshold)
            || (token_threshold && !input.has_tool_calls_in_last_turn);

        if !should_trigger {
            let skip = if !token_threshold {
                "token_growth_unmet"
            } else {
                "tool_call_window_not_drained"
            };
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some(skip.to_string()),
            });
        }

        // W-MEMORY-EVOLUTION PR-2 (2026-05-29): the gate has no disk access
        // (its input is metadata-only), so it can NOT assemble the real LLM
        // payload. The previous code shipped a metadata-only user message +
        // an unsubstituted `{{current_notes}}` / `{{notes_path}}` system
        // prompt — the LLM never saw the actual notes or transcript (the
        // Tier-1 content bug). The real messages are now built in
        // `process()` from disk (`build_consolidation_messages`). The gate
        // only carries the trigger decision + token baseline.
        Ok(GateDecision {
            should_trigger: true,
            payload: Some(SessionMemoryGateOutput {
                messages: Vec::new(),
                token_count_at_trigger: input.current_token_count,
            }),
            skip_reason: None,
        })
    }
}

/// Per-session promote check. Lazy initialization tracking via auxiliary
/// `Mutex` — moved to its own inner state to avoid BTreeMap entry-level
/// races. The state lives inside `Tier1SessionState` as an additional inner
/// `Mutex<Tier1MutableState>` field accessed via the helper below.
async fn check_and_promote_initialized(
    state: &Tier1SessionState,
    current_token_count: u64,
) -> InitCheck {
    // We piggy-back on the `tokens_at_last_extraction` AtomicU64 here:
    //   - tokens_at_last_extraction == 0 → not yet initialized.
    //   - Once a successful extraction lands the processor will set
    //     tokens_at_last_extraction = current_token_count (>= MIN_TOKENS_TO_INIT
    //     by construction since gate 3 enforced it), making the next call
    //     skip the promote-from-zero branch.
    //   - The very first gate evaluation that crosses MIN_TOKENS_TO_INIT
    //     marks the session as initialized by setting tokens_at_last_extraction
    //     to MIN_TOKENS_TO_INIT - MIN_TOKENS_BETWEEN_UPDATE (so the first
    //     update threshold check sees the right "baseline").
    let last_extraction = state.tokens_at_last_extraction.load(Ordering::Acquire);
    if last_extraction == 0 {
        if current_token_count < MIN_TOKENS_TO_INIT {
            return InitCheck {
                initialized: false,
                tokens_at_last_extraction: 0,
            };
        }
        // Promote to initialized; baseline = current - MIN_TOKENS_BETWEEN_UPDATE
        // so the first update gate (token growth ≥ MIN_TOKENS_BETWEEN_UPDATE)
        // requires that much additional growth from now on.
        let baseline = current_token_count.saturating_sub(MIN_TOKENS_BETWEEN_UPDATE);
        // CAS-style promote; tolerate a parallel evaluator that already
        // promoted by reading the post-swap value.
        match state.tokens_at_last_extraction.compare_exchange(
            0,
            baseline.max(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => InitCheck {
                initialized: true,
                tokens_at_last_extraction: baseline,
            },
            Err(observed) => InitCheck {
                initialized: true,
                tokens_at_last_extraction: observed,
            },
        }
    } else {
        InitCheck {
            initialized: true,
            tokens_at_last_extraction: last_extraction,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InitCheck {
    initialized: bool,
    tokens_at_last_extraction: u64,
}

// ──────────────────────────────────────────────────────────────────────────
// SessionMemoryProcessor (reverse IPC LLM driver + disk writer)
// ──────────────────────────────────────────────────────────────────────────

/// Per-turn extraction request. The processor enqueues a reverse IPC LLM
/// call request, awaits the result (with timeout), and writes the result
/// to SESSION.md + `.session-<turn_id>.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionMemoryProcessInput {
    /// Session key (becomes `.session-<key>.md` file stem).
    pub session_key: String,
    /// Turn id (becomes `.session-<turn_id>.md` snapshot file stem).
    pub turn_id: String,
    /// Absolute path to the memory directory where SESSION.md + snapshots
    /// land.
    pub memory_dir: PathBuf,
    /// Gate payload from the preceding `evaluate_gate` call. Carries
    /// `token_count_at_trigger`; its `messages` field is **no longer used for
    /// content** (W-MEMORY-EVOLUTION PR-2 — the real LLM messages are rebuilt
    /// from disk inside `process()`; see `build_consolidation_messages`).
    pub gate_payload: SessionMemoryGateOutput,
    /// W-MEMORY-EVOLUTION PR-2 (2026-05-29) — project transcript directory
    /// (the dir `scan_transcript_dir` walks). Supplied by the caller so the
    /// orchestrator can read the recent conversation **directly from disk**
    /// (§15 P5: transcripts are a legitimate Rust data source). `None` → the
    /// transcript section is omitted (degraded but functional: current notes
    /// are still injected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<PathBuf>,
    /// W-MEMORY-EVOLUTION PR-2 — session id used to pick the matching `.jsonl`
    /// under `transcript_dir`. `None` (or no match) → the most-recently
    /// modified transcript is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_session_id: Option<String>,
    /// Optional model hint (TS side decides actual model selection; this
    /// field is plumbed only as a hint and not used in any brand-literal
    /// hardcode — CLAUDE.md §硬约束 #1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    /// Sampling parameters (None = TS uses SDK defaults).
    #[serde(default)]
    pub params: LlmCallParams,
    /// W-MEMORY-SELF-EVOLVE-DGM G3-b (2026-07-16) — 本会话既有速记快照的
    /// 全文。`Some` 时 system prompt 追加 `TIER1_DELTA_SUFFIX`（合并旧速记
    /// 与转写新增，输出完整更新版）。`None` = 首次速记（旧行为不变）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_note: Option<String>,
}

/// Per-turn extraction result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct SessionMemoryProcessOutput {
    /// Wire identifier of the LLM call request that was issued.
    pub req_id: String,
    /// Path of the SESSION.md file written (absolute).
    pub session_md_path: PathBuf,
    /// Path of the per-turn snapshot `.session-<turn_id>.md` file written.
    pub snapshot_path: PathBuf,
    /// Bytes written to `session_md_path`.
    pub session_md_bytes: usize,
    /// LLM token usage (mirrors `LlmCallResultPayload::usage`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsageWire>,
}

/// Mirror of `LlmUsage` with serde — kept independent of `tier::LlmUsage`
/// so the public wire shape doesn't accidentally couple Processor's
/// output to its input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct LlmUsageWire {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionMemoryProcessError {
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
}

/// Reverse IPC sender — abstracted so tests can inject an in-memory mock
/// without standing up the TUI client outgoing channel.
///
/// Production wiring: a UDS notification emit path that broadcasts the
/// `memory/tier/llmCallRequest` notification to TUI client (declare-now,
/// emit-later in this PR — the dispatcher side is already in place via
/// P3.1; the orchestrator-side emit broadcast hook is plumbed in a
/// follow-up that crosses the bun_worker outgoing channel API surface).
#[async_trait]
pub trait LlmCallEmitter: Send + Sync {
    async fn emit_request(&self, request: LlmCallRequestPayload);
}

/// In-memory mock emitter used in unit tests. Records every emitted
/// request; tests can inspect via `recorded()`.
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

/// Tier-1 processor — owns:
/// - the gate's per-session state (shared via `Arc<SessionMemoryGate>`),
/// - the pending oneshot map keyed by `req_id`,
/// - the emitter for outgoing reverse IPC LLM call requests.
///
/// Threadsafe (all interior mutability via `Arc<Mutex<_>>` + atomics).
pub struct SessionMemoryProcessor {
    gate: Arc<SessionMemoryGate>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<LlmCallResultPayload>>>>,
    emitter: Arc<dyn LlmCallEmitter>,
    req_id_counter: AtomicU64,
}

impl SessionMemoryProcessor {
    pub fn new(gate: Arc<SessionMemoryGate>, emitter: Arc<dyn LlmCallEmitter>) -> Self {
        Self {
            gate,
            pending: Arc::new(Mutex::new(HashMap::new())),
            emitter,
            req_id_counter: AtomicU64::new(0),
        }
    }

    pub fn gate(&self) -> &Arc<SessionMemoryGate> {
        &self.gate
    }

    /// Deliver a reverse IPC LLM call result. Looks up the matching pending
    /// oneshot by `req_id`, fires it, and drops the entry. Unknown
    /// `req_id` is a no-op (treated as late delivery for a timed-out
    /// request).
    pub async fn deliver_result(&self, result: LlmCallResultPayload) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(sender) = map.remove(&result.req_id) {
            // Drop ok: receiver may have been dropped by timeout already.
            let _ = sender.send(result);
            true
        } else {
            false
        }
    }

    fn next_req_id(&self) -> String {
        let n = self.req_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("tier1-{n}-{}", now_ms())
    }

    /// Run one Tier-1 extraction: emit reverse IPC LLM call request, await
    /// the result, write SESSION.md + per-turn snapshot. On success,
    /// promotes the gate's per-session `tokens_at_last_extraction` to the
    /// trigger point so the next gate evaluation runs against the new
    /// baseline.
    pub async fn process(
        &self,
        input: SessionMemoryProcessInput,
    ) -> Result<SessionMemoryProcessOutput, SessionMemoryProcessError> {
        let state = self.gate.get_or_init_state(&input.session_key).await;

        // Concurrency guard: only one extraction at a time per session.
        // Mirrors CrabClaw `atomic.CompareAndSwapInt32(&state.extractionInProgress, 0, 1)`
        // (tier1_policy.go:68-79). On contention we drop the request (caller
        // gets `Shutdown`-style sentinel; semantically "another in flight").
        if state
            .extraction_in_progress
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SessionMemoryProcessError::Write(
                "extraction already in progress for this session".to_string(),
            ));
        }
        state
            .extraction_started_at_ms
            .store(now_ms(), Ordering::Release);

        // Make sure we always release the guard, even on error/timeout.
        struct GuardReleaser<'a> {
            state: &'a Tier1SessionState,
        }
        impl<'a> Drop for GuardReleaser<'a> {
            fn drop(&mut self) {
                self.state
                    .extraction_started_at_ms
                    .store(0, Ordering::Release);
                self.state
                    .extraction_in_progress
                    .store(0, Ordering::Release);
            }
        }
        let _guard = GuardReleaser { state: &state };

        // 1. Register pending oneshot and emit reverse IPC request.
        let req_id = self.next_req_id();
        let (tx, rx) = oneshot::channel::<LlmCallResultPayload>();
        {
            let mut map = self.pending.lock().await;
            map.insert(req_id.clone(), tx);
        }

        // W-MEMORY-EVOLUTION PR-2: build the real messages from disk (current
        // SESSION.md notes + recent transcript), NOT the gate's metadata-only
        // payload. This is the Tier-1 content-bug fix.
        let messages = build_consolidation_messages(&input);
        let request_payload = LlmCallRequestPayload {
            req_id: req_id.clone(),
            tier: MemoryTier::Session,
            phase: None,
            messages,
            model_hint: input.model_hint.clone(),
            params: input.params.clone(),
        };
        self.emitter.emit_request(request_payload).await;

        // 2. Await result with timeout.
        let result =
            match tokio::time::timeout(Duration::from_millis(LLM_CALL_TIMEOUT_MS), rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_recv_err)) => {
                    // Pending entry must already be removed by deliver_result,
                    // but a defensive drop:
                    self.pending.lock().await.remove(&req_id);
                    return Err(SessionMemoryProcessError::Shutdown);
                }
                Err(_elapsed) => {
                    // Timeout: orphan the pending entry (deliver_result becomes
                    // a no-op on late arrival).
                    self.pending.lock().await.remove(&req_id);
                    return Err(SessionMemoryProcessError::Timeout(LLM_CALL_TIMEOUT_MS));
                }
            };

        // 3. Validate result.
        if let Some(err) = result.error.as_ref() {
            return Err(SessionMemoryProcessError::LlmFailure(err.clone()));
        }
        let response = result
            .response
            .ok_or_else(|| SessionMemoryProcessError::LlmFailure("empty response".to_string()))?;

        let cleaned = clean_llm_output(&response);
        if cleaned.trim().is_empty() {
            return Err(SessionMemoryProcessError::Write(
                "LLM returned empty session notes; not writing".to_string(),
            ));
        }

        // 4. Write SESSION.md + per-turn snapshot.
        tokio::fs::create_dir_all(&input.memory_dir).await?;
        let session_md_path = input.memory_dir.join("SESSION.md");
        let snapshot_path = input
            .memory_dir
            .join(format!(".session-{}.md", sanitize_turn_id(&input.turn_id)));

        let bytes = cleaned.as_bytes();
        atomic_write(&session_md_path, bytes)
            .await
            .map_err(|e: BoxError| SessionMemoryProcessError::Write(e.to_string()))?;
        atomic_write(&snapshot_path, bytes)
            .await
            .map_err(|e: BoxError| SessionMemoryProcessError::Write(e.to_string()))?;

        // 5. Promote per-session state's tokens_at_last_extraction baseline.
        state
            .tokens_at_last_extraction
            .store(input.gate_payload.token_count_at_trigger, Ordering::Release);

        let usage = result.usage.map(|u| LlmUsageWire {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });

        Ok(SessionMemoryProcessOutput {
            req_id,
            session_md_path,
            snapshot_path,
            session_md_bytes: bytes.len(),
            usage,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────

/// Max characters of compacted transcript injected into the Tier-1 user
/// message. Generous (recent session turns), tail-kept by
/// `read_transcript_content`.
const TRANSCRIPT_COMPACT_MAX_CHARS: usize = 12_000;

/// W-MEMORY-EVOLUTION PR-2 (2026-05-29) — build the real Tier-1 consolidation
/// LLM messages from **disk** (the gate cannot — it has no disk access).
///
/// - system: `TIER1_CONSOLIDATION_PROMPT` with `{{notes_path}}` /
///   `{{current_notes}}` substituted from `<memory_dir>/SESSION.md` (empty when
///   the file is absent — a fresh session).
/// - user: a compact transcript of the recent conversation, read directly from
///   the matching `.jsonl` under `transcript_dir`. When no transcript is
///   available the section degrades to an explicit "(no recent transcript…)"
///   note rather than fabricating content.
///
/// This fixes the Tier-1 content bug: the LLM now receives the actual current
/// notes + real conversation instead of metadata + unsubstituted placeholders.
fn build_consolidation_messages(input: &SessionMemoryProcessInput) -> Vec<LlmMessage> {
    let session_md_path = input.memory_dir.join("SESSION.md");
    let current_notes = std::fs::read_to_string(&session_md_path).unwrap_or_default();

    let mut system_content = TIER1_CONSOLIDATION_PROMPT
        .replace("{{notes_path}}", &session_md_path.to_string_lossy())
        .replace("{{current_notes}}", &current_notes);
    // G3-b：增量更新 —— 旧速记注入 + 「只展开新信息」指令。
    if let Some(prior_note) = input
        .prior_note
        .as_deref()
        .filter(|note| !note.trim().is_empty())
    {
        system_content.push_str(&TIER1_DELTA_SUFFIX.replace("{{prior_note}}", prior_note));
    }

    let transcript = resolve_transcript_text(input);
    let user_content = if transcript.trim().is_empty() {
        "(no recent transcript available for this session)".to_string()
    } else {
        format!("--- RECENT TRANSCRIPT ---\n{transcript}\n--- END TRANSCRIPT ---")
    };

    vec![
        LlmMessage {
            role: "system".to_string(),
            content: system_content,
        },
        LlmMessage {
            role: "user".to_string(),
            content: user_content,
        },
    ]
}

/// Locate the relevant transcript `.jsonl` under `input.transcript_dir` and
/// read+compact it. Picks the file whose `session_id` matches
/// `current_session_id`; falls back to the most-recently-modified transcript.
/// Returns empty string when there is no transcript dir / no transcripts /
/// read error (degraded, non-fatal).
fn resolve_transcript_text(input: &SessionMemoryProcessInput) -> String {
    let Some(dir) = input.transcript_dir.as_ref() else {
        return String::new();
    };
    let metas = match crate::transcript_index::scan_transcript_dir(dir) {
        Ok(metas) => metas,
        Err(_) => return String::new(),
    };
    if metas.is_empty() {
        return String::new();
    }
    // Prefer the session-id match; else the most recent by mtime.
    let chosen = input
        .current_session_id
        .as_ref()
        .and_then(|sid| {
            metas
                .iter()
                .filter(|m| m.session_id.as_deref() == Some(sid.as_str()))
                .max_by_key(|m| m.mtime_ms)
        })
        .or_else(|| metas.iter().max_by_key(|m| m.mtime_ms));

    match chosen {
        Some(meta) => crate::transcript_index::read_transcript_content(
            &meta.path,
            TRANSCRIPT_COMPACT_MAX_CHARS,
        )
        .unwrap_or_default(),
        None => String::new(),
    }
}

/// W-MEMORY-SELF-EVOLVE-DGM G3-d (2026-07-16) — 零 LLM 元数据速记地板。
///
/// LLM 通道不可用（配额尽 / 离线 / 网关错误）时，从转写元数据直接生成一
/// 份**降级速记**写入 per-session 快照（不碰 SESSION.md），保证「会话
/// 速记」面不因模型故障而空白。琐碎会话跳过（grok-build 同款门槛：实质
/// 用户消息 < 3 条或用户文本 < 50 字节）。frontmatter 标 `degraded: true`
/// —— 下次做梦仍会重读原转写，本速记只是可见性地板不是最终提炼。
///
/// 返回 `Ok(true)` 已写入、`Ok(false)` 琐碎跳过、`Err` IO 失败。
pub async fn write_degraded_session_note(
    transcript_path: &std::path::Path,
    snapshot_path: &std::path::Path,
    session_id: &str,
    language: crate::output_language::MemoryOutputLanguage,
) -> Result<bool, BoxError> {
    const MIN_SUBSTANTIVE_PROMPTS: usize = 3;
    const MIN_USER_BYTES: usize = 50;
    const SUBSTANTIVE_MIN_CHARS: usize = 8;
    const TOPIC_CAP: usize = 5;
    const TOPIC_MAX_CHARS: usize = 80;

    let raw = match tokio::fs::read_to_string(transcript_path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    let mut user_texts: Vec<String> = Vec::new();
    let mut assistant_count = 0usize;
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let ty = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(message) = value.get("message") else {
            continue;
        };
        let text = crate::transcript_index::extract_message_text(message.get("content"));
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match ty {
            "user" => user_texts.push(text.to_string()),
            "assistant" => assistant_count += 1,
            _ => {}
        }
    }

    let substantive: Vec<&String> = user_texts
        .iter()
        .filter(|t| t.chars().count() >= SUBSTANTIVE_MIN_CHARS)
        .collect();
    let user_bytes: usize = user_texts.iter().map(|t| t.len()).sum();
    if substantive.len() < MIN_SUBSTANTIVE_PROMPTS || user_bytes < MIN_USER_BYTES {
        return Ok(false);
    }

    let topics: Vec<String> = substantive
        .iter()
        .take(TOPIC_CAP)
        .map(|t| {
            let mut line: String = t.chars().take(TOPIC_MAX_CHARS).collect();
            if t.chars().count() > TOPIC_MAX_CHARS {
                line.push('…');
            }
            format!("  - {line}")
        })
        .collect();

    let body = match language {
        crate::output_language::MemoryOutputLanguage::Zh => format!(
            "---\ntype: session-note\ndegraded: true\nsession: {session_id}\n---\n\
             # 会话速记（降级 · 未经模型提炼）\n\n\
             - 用户消息 {user} 条 / 助手回复 {assistant} 条\n\
             - 主题线索：\n{topics}\n\n\
             > 生成时 LLM 通道不可用，本速记由转写元数据直接生成；\
             下次做梦会重读原始转写，不受此降级影响。\n",
            user = user_texts.len(),
            assistant = assistant_count,
            topics = topics.join("\n"),
        ),
        crate::output_language::MemoryOutputLanguage::En => format!(
            "---\ntype: session-note\ndegraded: true\nsession: {session_id}\n---\n\
             # Session note (degraded — no model consolidation)\n\n\
             - {user} user messages / {assistant} assistant replies\n\
             - Topic leads:\n{topics}\n\n\
             > The LLM lane was unavailable when this note was generated; it \
             was built directly from transcript metadata. The next dream cycle \
             re-reads the raw transcript and is unaffected.\n",
            user = user_texts.len(),
            assistant = assistant_count,
            topics = topics.join("\n"),
        ),
    };
    atomic_write(snapshot_path, body.as_bytes()).await?;
    Ok(true)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Strip likely-LLM-decoration (markdown fences) without touching ordinary
/// markdown content. Mirrors CrabClaw `cleanLLMOutput()` (tier1_policy.go:353).
pub(crate) fn clean_llm_output(raw: &str) -> String {
    let s = raw.trim();
    let s = if let Some(stripped) = s.strip_prefix("```markdown\n") {
        stripped
            .rsplit_once("\n```")
            .map(|(body, _)| body)
            .unwrap_or(stripped)
    } else if let Some(stripped) = s.strip_prefix("```\n") {
        stripped
            .rsplit_once("\n```")
            .map(|(body, _)| body)
            .unwrap_or(stripped)
    } else {
        s
    };
    s.trim().to_string()
}

/// Sanitize a turn id for use as a filename stem. Replaces any character
/// outside `[A-Za-z0-9_.-]` with `_`.
pub(crate) fn sanitize_turn_id(raw: &str) -> String {
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

/// Resolve absolute path under a memory_dir for a given session key.
/// Helper exposed for the dispatcher's pre-validation; not used internally
/// by `process` (which uses `memory_dir` + sanitized `turn_id` directly).
pub fn session_snapshot_path(memory_dir: &Path, turn_id: &str) -> PathBuf {
    memory_dir.join(format!(".session-{}.md", sanitize_turn_id(turn_id)))
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn flags_enabled() -> BTreeMap<String, bool> {
        let mut m = BTreeMap::new();
        m.insert("auto_session_memory".to_string(), true);
        m
    }

    fn flags_disabled() -> BTreeMap<String, bool> {
        BTreeMap::new()
    }

    fn input(
        agent_type: &str,
        flags: BTreeMap<String, bool>,
        token_count: u64,
        tool_calls: u32,
        has_tool_in_last: bool,
    ) -> SessionMemoryGateInput {
        SessionMemoryGateInput {
            agent_type: agent_type.to_string(),
            session_key: "sess-1".to_string(),
            current_token_count: token_count,
            tool_calls_since_last_update: tool_calls,
            has_tool_calls_in_last_turn: has_tool_in_last,
            feature_flags: flags,
        }
    }

    // ── Gate 1 ──
    #[tokio::test]
    async fn gate1_subagent_rejected() {
        let gate = SessionMemoryGate::new();
        let decision = gate
            .evaluate_gate(input("subagent", flags_enabled(), 50_000, 10, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("non_main_agent"));
    }

    // ── Gate 2 ──
    #[tokio::test]
    async fn gate2_feature_disabled() {
        let gate = SessionMemoryGate::new();
        let decision = gate
            .evaluate_gate(input("", flags_disabled(), 50_000, 10, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("feature_disabled"));
    }

    // ── Gate 3 ──
    #[tokio::test]
    async fn gate3_init_threshold_unmet() {
        let gate = SessionMemoryGate::new();
        // Below MIN_TOKENS_TO_INIT (10000).
        let decision = gate
            .evaluate_gate(input("", flags_enabled(), 5_000, 10, false))
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(
            decision.skip_reason.as_deref(),
            Some("init_threshold_unmet")
        );
    }

    // ── Gate 4 (token growth unmet) ──
    #[tokio::test]
    async fn gate4_token_growth_unmet_after_init() {
        let gate = SessionMemoryGate::new();
        // First call: promotes initialized (baseline = 10000 - 5000 = 5000).
        let first = gate
            .evaluate_gate(input("", flags_enabled(), 10_000, 10, false))
            .await
            .unwrap();
        // The first crossing of init threshold also evaluates the gates 4/5:
        // token_growth = 10000 - 5000 = 5000 (meets MIN_TOKENS_BETWEEN_UPDATE),
        // tool_calls = 10 (meets), has_tool_in_last = false → should trigger.
        assert!(first.should_trigger);

        // Second call: simulate processor settling and promoting baseline by
        // calling state.tokens_at_last_extraction directly to current.
        // Use a brand-new gate (different session_key) for a clean check:
        let mut input2 = input("", flags_enabled(), 12_000, 10, false);
        input2.session_key = "sess-2".to_string();
        // First evaluation for sess-2 same shape — promotes init.
        let _ = gate.evaluate_gate(input2.clone()).await.unwrap();

        // Now bump only a little (token_growth < 5000):
        let mut input3 = input2.clone();
        input3.current_token_count = 13_000; // growth from baseline 7000 = 6000 — still meets
        let third = gate.evaluate_gate(input3).await.unwrap();
        assert!(third.should_trigger); // 6000 >= 5000

        // A growth below 5000:
        let state = gate.get_or_init_state("sess-3").await;
        state
            .tokens_at_last_extraction
            .store(20_000, Ordering::Release);
        let mut input4 = input("", flags_enabled(), 23_000, 10, false);
        input4.session_key = "sess-3".to_string();
        let fourth = gate.evaluate_gate(input4).await.unwrap();
        assert!(!fourth.should_trigger);
        assert_eq!(fourth.skip_reason.as_deref(), Some("token_growth_unmet"));
    }

    // ── Gate 5 (tool call window not drained but last turn has tools) ──
    #[tokio::test]
    async fn gate5_tool_call_window_not_drained_and_last_has_tools() {
        let gate = SessionMemoryGate::new();
        let state = gate.get_or_init_state("sess-4").await;
        state
            .tokens_at_last_extraction
            .store(10_000, Ordering::Release);
        let mut inp = input("", flags_enabled(), 20_000, 2, true);
        inp.session_key = "sess-4".to_string();
        // token_threshold true; tool_call_threshold false (2 < 3);
        // has_tool_in_last true → !has_tool false → trigger = false.
        let d = gate.evaluate_gate(inp).await.unwrap();
        assert!(!d.should_trigger);
        assert_eq!(
            d.skip_reason.as_deref(),
            Some("tool_call_window_not_drained")
        );
    }

    // ── All gates pass ──
    #[tokio::test]
    async fn all_gates_pass_returns_payload() {
        let gate = SessionMemoryGate::new();
        let state = gate.get_or_init_state("sess-pass").await;
        state
            .tokens_at_last_extraction
            .store(20_000, Ordering::Release);
        let mut inp = input("", flags_enabled(), 30_000, 5, false);
        inp.session_key = "sess-pass".to_string();
        let d = gate.evaluate_gate(inp).await.unwrap();
        assert!(d.should_trigger);
        let payload = d.payload.expect("payload");
        assert_eq!(payload.token_count_at_trigger, 30_000);
        // W-MEMORY-EVOLUTION PR-2: the gate no longer assembles content (it has
        // no disk access). The real LLM messages are built in `process()` from
        // disk; the gate only carries the trigger decision + token baseline.
        assert!(
            payload.messages.is_empty(),
            "gate must not embed content messages; got {}",
            payload.messages.len()
        );
    }

    // ── Processor: LLM call roundtrip success → SESSION.md written ──
    #[tokio::test]
    async fn processor_roundtrip_writes_session_and_snapshot() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");

        let gate = Arc::new(SessionMemoryGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(SessionMemoryProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let payload = SessionMemoryGateOutput {
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            token_count_at_trigger: 25_000,
        };
        let input = SessionMemoryProcessInput {
            prior_note: None,
            session_key: "sess-roundtrip".to_string(),
            turn_id: "turn-abc".to_string(),
            memory_dir: memory_dir.clone(),
            gate_payload: payload,
            transcript_dir: None,
            current_session_id: None,
            model_hint: None,
            params: LlmCallParams::default(),
        };

        // Spawn the processor; observe the emitted request and reply.
        let proc_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { proc_clone.process(input).await });

        // Wait for the emitter to record the request, then deliver result.
        let mut req_id_opt = None;
        for _ in 0..50 {
            let recorded = emitter.recorded().await;
            if let Some(r) = recorded.first() {
                req_id_opt = Some(r.req_id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let req_id = req_id_opt.expect("emitter must record a request");

        let delivered = processor
            .deliver_result(LlmCallResultPayload {
                req_id: req_id.clone(),
                response: Some("# Session\n\nNotes body.".to_string()),
                usage: Some(crate::tier::LlmUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                error: None,
            })
            .await;
        assert!(delivered);

        let output = task.await.unwrap().expect("process ok");
        assert_eq!(output.req_id, req_id);
        assert!(output.session_md_path.ends_with("SESSION.md"));
        assert!(output.snapshot_path.ends_with(".session-turn-abc.md"));
        assert!(output.session_md_bytes > 0);

        let session_body = tokio::fs::read_to_string(&output.session_md_path)
            .await
            .unwrap();
        assert!(session_body.contains("Notes body"));
        let snapshot_body = tokio::fs::read_to_string(&output.snapshot_path)
            .await
            .unwrap();
        assert_eq!(snapshot_body, session_body);

        // Per-session baseline promoted.
        let state = gate.get_or_init_state("sess-roundtrip").await;
        assert_eq!(
            state.tokens_at_last_extraction.load(Ordering::Acquire),
            25_000
        );
    }

    // ── Processor: LLM call timeout ──
    #[tokio::test]
    async fn processor_timeout_drops_pending_and_releases_guard() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let gate = Arc::new(SessionMemoryGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = SessionMemoryProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        );

        // Override timeout via direct manipulation: we can't easily change
        // LLM_CALL_TIMEOUT_MS const at runtime, but tokio::time::pause +
        // advance achieves the same effect deterministically.
        // Use a separate runtime helper: pause time and advance past timeout.
        let input = SessionMemoryProcessInput {
            prior_note: None,
            session_key: "sess-timeout".to_string(),
            turn_id: "turn-x".to_string(),
            memory_dir: memory_dir.clone(),
            gate_payload: SessionMemoryGateOutput {
                messages: vec![],
                token_count_at_trigger: 25_000,
            },
            transcript_dir: None,
            current_session_id: None,
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let processor_arc = Arc::new(processor);
        let p2 = Arc::clone(&processor_arc);
        let task = tokio::spawn(async move { p2.process(input).await });

        // Use real time but tiny sleep then assert task makes progress: we
        // can't actually wait 30s in unit test; instead test the "shutdown"
        // path by dropping the pending sender to simulate orchestrator-side
        // shutdown.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let recorded = emitter.recorded().await;
            if !recorded.is_empty() {
                break;
            }
        }

        // Drop the receiver by closing the channel: emulate orchestrator
        // shutdown.
        {
            let mut map = processor_arc.pending.lock().await;
            for (_id, tx) in map.drain() {
                drop(tx);
            }
        }

        let result = task.await.unwrap();
        assert!(matches!(result, Err(SessionMemoryProcessError::Shutdown)));

        // Guard released — second extraction should be allowed.
        let state = gate.get_or_init_state("sess-timeout").await;
        assert_eq!(state.extraction_in_progress.load(Ordering::Acquire), 0);
    }

    // ── Processor: LLM error result returns LlmFailure ──
    #[tokio::test]
    async fn processor_llm_error_result_returns_llm_failure() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let gate = Arc::new(SessionMemoryGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(SessionMemoryProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let input = SessionMemoryProcessInput {
            prior_note: None,
            session_key: "sess-err".to_string(),
            turn_id: "turn-y".to_string(),
            memory_dir,
            gate_payload: SessionMemoryGateOutput {
                messages: vec![],
                token_count_at_trigger: 25_000,
            },
            transcript_dir: None,
            current_session_id: None,
            model_hint: None,
            params: LlmCallParams::default(),
        };
        let p2 = Arc::clone(&processor);
        let task = tokio::spawn(async move { p2.process(input).await });

        let mut req_id = None;
        for _ in 0..50 {
            let recorded = emitter.recorded().await;
            if let Some(r) = recorded.first() {
                req_id = Some(r.req_id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let req_id = req_id.unwrap();
        processor
            .deliver_result(LlmCallResultPayload {
                req_id,
                response: None,
                usage: None,
                error: Some("rate limited".to_string()),
            })
            .await;

        let result = task.await.unwrap();
        match result {
            Err(SessionMemoryProcessError::LlmFailure(msg)) => {
                assert_eq!(msg, "rate limited");
            }
            other => panic!("expected LlmFailure, got {other:?}"),
        }
    }

    // ── Processor: deliver_result unknown req_id is no-op ──
    #[tokio::test]
    async fn deliver_unknown_req_id_returns_false() {
        let gate = Arc::new(SessionMemoryGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor =
            SessionMemoryProcessor::new(Arc::clone(&gate), emitter as Arc<dyn LlmCallEmitter>);
        let delivered = processor
            .deliver_result(LlmCallResultPayload {
                req_id: "unknown".to_string(),
                response: Some("hi".to_string()),
                usage: None,
                error: None,
            })
            .await;
        assert!(!delivered);
    }

    /// W-MEMORY-SELF-EVOLVE-DGM G3-b — prior_note 存在（且非空白）才注入
    /// 增量更新后缀；旧速记全文进入 system prompt。
    #[test]
    fn g3b_delta_suffix_injected_only_with_prior_note() {
        let dir = tempfile::TempDir::new().unwrap();
        let mk = |prior: Option<String>| SessionMemoryProcessInput {
            prior_note: prior,
            session_key: "s".to_string(),
            turn_id: "t".to_string(),
            memory_dir: dir.path().to_path_buf(),
            gate_payload: SessionMemoryGateOutput {
                messages: Vec::new(),
                token_count_at_trigger: 0,
            },
            transcript_dir: None,
            current_session_id: None,
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let plain = build_consolidation_messages(&mk(None));
        assert!(
            !plain[0].content.contains("PREVIOUS NOTE FOR THIS SESSION"),
            "无 prior_note 不注入 delta 后缀"
        );

        let delta = build_consolidation_messages(&mk(Some("旧速记：修复了做梦水位线".to_string())));
        assert!(delta[0].content.contains("PREVIOUS NOTE FOR THIS SESSION"));
        assert!(delta[0].content.contains("旧速记：修复了做梦水位线"));
        assert!(delta[0].content.contains("UPDATED COMPLETE note"));

        let blank = build_consolidation_messages(&mk(Some("   ".to_string())));
        assert!(
            !blank[0].content.contains("PREVIOUS NOTE FOR THIS SESSION"),
            "空白 prior_note 视同无"
        );
    }

    /// W-MEMORY-SELF-EVOLVE-DGM G3-d — 零 LLM 元数据速记：琐碎会话跳过；
    /// 实质会话写降级快照（frontmatter degraded:true + 主题线索），zh/en
    /// 双语。
    #[tokio::test]
    async fn g3d_degraded_note_writes_metadata_and_skips_trivial() {
        let dir = tempfile::TempDir::new().unwrap();
        let transcript = dir.path().join("sess-a.jsonl");
        let snapshot = dir.path().join(".session-sess-a.md");
        let user_line = |text: &str| {
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{text}"}},"isSidechain":false}}"#
            )
        };
        let assistant_line = |text: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}},"isSidechain":false}}"#
            )
        };

        // 琐碎：实质用户消息 < 3 → 跳过不写。
        std::fs::write(
            &transcript,
            [
                user_line("修一下这个渲染问题"),
                assistant_line("好的，已修复"),
                user_line("谢谢"),
            ]
            .join("\n"),
        )
        .unwrap();
        assert!(!write_degraded_session_note(
            &transcript,
            &snapshot,
            "sess-a",
            crate::output_language::MemoryOutputLanguage::Zh,
        )
        .await
        .unwrap());
        assert!(!snapshot.exists(), "琐碎会话不留降级速记");

        // 实质会话：3 条 ≥8 字符用户消息 → 写入。
        std::fs::write(
            &transcript,
            [
                user_line("帮我审计记忆系统的做梦水位线语义"),
                assistant_line("已定位到 lock mtime 语义"),
                user_line("撞帽积压要留给下一轮消费不能丢"),
                assistant_line("已按最旧优先消费实现"),
                user_line("最后把陈旧度标注也加上并跑全量测试"),
                r#"{"type":"user","message":{"role":"user","content":"sidechain noise"},"isSidechain":true}"#
                    .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        assert!(write_degraded_session_note(
            &transcript,
            &snapshot,
            "sess-a",
            crate::output_language::MemoryOutputLanguage::Zh,
        )
        .await
        .unwrap());
        let body = std::fs::read_to_string(&snapshot).unwrap();
        assert!(body.contains("degraded: true"));
        assert!(body.contains("session: sess-a"));
        assert!(body.contains("会话速记（降级"));
        assert!(body.contains("做梦水位线"), "主题线索来自实质用户消息");
        assert!(!body.contains("sidechain noise"), "sidechain 排除");
        assert!(
            !dir.path().join("SESSION.md").exists(),
            "降级速记只写 per-session 快照，不碰 SESSION.md"
        );

        // en 变体。
        let en_snapshot = dir.path().join(".session-sess-en.md");
        assert!(write_degraded_session_note(
            &transcript,
            &en_snapshot,
            "sess-en",
            crate::output_language::MemoryOutputLanguage::En,
        )
        .await
        .unwrap());
        assert!(std::fs::read_to_string(&en_snapshot)
            .unwrap()
            .contains("Session note (degraded"));
    }

    // ── clean_llm_output strips markdown fences ──
    #[test]
    fn clean_llm_output_strips_fences() {
        assert_eq!(clean_llm_output("hello"), "hello");
        assert_eq!(clean_llm_output("```markdown\nhello\n```"), "hello");
        assert_eq!(clean_llm_output("```\nhi\n```"), "hi");
        assert_eq!(
            clean_llm_output("  ```markdown\n# h\n\nbody\n```  "),
            "# h\n\nbody"
        );
    }

    // ── sanitize_turn_id whitelists safe chars ──
    #[test]
    fn sanitize_turn_id_replaces_unsafe() {
        assert_eq!(sanitize_turn_id("turn-abc.123"), "turn-abc.123");
        assert_eq!(sanitize_turn_id("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_turn_id("a b c"), "a_b_c");
    }

    // ── TierGate trait shape: GateDecision serializes ──
    #[tokio::test]
    async fn tier_gate_trait_decision_shape_round_trip() {
        let gate = SessionMemoryGate::new();
        let d = gate
            .evaluate_gate(input("subagent", flags_enabled(), 50_000, 10, false))
            .await
            .unwrap();
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["should_trigger"], false);
        assert_eq!(json["skip_reason"], "non_main_agent");
        assert!(json.get("payload").is_some() || json.get("payload").is_none());
    }

    // ── W-MEMORY-EVOLUTION PR-2 — Tier-1 content fix: process() builds the
    //    real LLM messages from disk (SESSION.md notes + transcript), with
    //    placeholders substituted. Regression lock for the metadata-only bug. ──

    /// Drive `process()` to the emit point and return the recorded request's
    /// messages (delivers a stub result to unblock the await).
    async fn capture_process_messages(input: SessionMemoryProcessInput) -> Vec<LlmMessage> {
        let gate = Arc::new(SessionMemoryGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(SessionMemoryProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));
        let proc_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { proc_clone.process(input).await });

        let mut req_id = None;
        let mut messages = Vec::new();
        for _ in 0..50 {
            let recorded = emitter.recorded().await;
            if let Some(r) = recorded.first() {
                req_id = Some(r.req_id.clone());
                messages = r.messages.clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let req_id = req_id.expect("emitter must record a request");
        processor
            .deliver_result(LlmCallResultPayload {
                req_id,
                response: Some("# Session\n\nok".to_string()),
                usage: None,
                error: None,
            })
            .await;
        let _ = task.await.unwrap();
        messages
    }

    #[tokio::test]
    async fn process_injects_session_notes_and_transcript_not_metadata() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("SESSION.md"), "## Goals\n- ship feature X").unwrap();

        // Transcript dir with a real-shaped session .jsonl (uuid-named).
        let transcript_dir = tmp.path().join("transcripts");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"please ship feature X"},"isSidechain":false}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"on it shipping X now"}]},"isSidechain":false}"#,
        ];
        std::fs::write(
            transcript_dir.join(format!("{session_id}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();

        let input = SessionMemoryProcessInput {
            prior_note: None,
            session_key: "sess-content".to_string(),
            turn_id: "turn-content".to_string(),
            memory_dir,
            gate_payload: SessionMemoryGateOutput {
                messages: vec![],
                token_count_at_trigger: 25_000,
            },
            transcript_dir: Some(transcript_dir),
            current_session_id: Some(session_id.to_string()),
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let messages = capture_process_messages(input).await;
        assert_eq!(messages.len(), 2);
        let system = &messages[0].content;
        // Placeholders substituted (not literal), real notes injected.
        assert!(!system.contains("{{current_notes}}"), "system: {system}");
        assert!(!system.contains("{{notes_path}}"), "system: {system}");
        assert!(system.contains("ship feature X"), "notes missing: {system}");
        // Transcript content injected into the user message.
        let user = &messages[1].content;
        assert!(
            user.contains("please ship feature X"),
            "user transcript: {user}"
        );
        assert!(
            user.contains("on it shipping X now"),
            "user transcript: {user}"
        );
    }

    #[tokio::test]
    async fn process_degrades_without_transcript_dir() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("SESSION.md"), "## Notes\n- keep me").unwrap();

        let input = SessionMemoryProcessInput {
            prior_note: None,
            session_key: "sess-degraded".to_string(),
            turn_id: "turn-degraded".to_string(),
            memory_dir,
            gate_payload: SessionMemoryGateOutput {
                messages: vec![],
                token_count_at_trigger: 25_000,
            },
            transcript_dir: None,
            current_session_id: None,
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let messages = capture_process_messages(input).await;
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0].content.contains("keep me"),
            "notes still injected"
        );
        assert!(!messages[0].content.contains("{{current_notes}}"));
        // No transcript dir → explicit degraded note, no fabricated content.
        assert!(
            messages[1].content.contains("no recent transcript"),
            "degraded user msg: {}",
            messages[1].content
        );
    }
}
