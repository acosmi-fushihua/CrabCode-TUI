//! W-MEMORY-DREAM-REBUILD v7 P3.3 — Tier-2 ExtractMemories policy（Rust 重写）。
//!
//! 设计借鉴 CrabClaw `backend/internal/memory/uhms/tier2_policy.go` +
//! `tier2_store.go`（只读参考归档，不 git copy；重写为 Rust）。配合 P3.1 立
//! 的反向 IPC LLM 调用契约：orchestrator 通过 `memory/tier/llmCallRequest`
//! notification 广播请求 → TS 端跑 SDK → `memory/tier/llmCallResult` request
//! 回写 → orchestrator 内部 pending oneshot 按 `req_id` 匹配（详
//! `tier/mod.rs` 反向 IPC 时序图）。
//!
//! # 6 重门控（Tier-2 deterministic policy）
//!
//! 1. **非 remote 模式** —— 远端跑暂跳过（local-only 优先）。
//! 2. **auto-memory 开启** —— `auto_memory_enabled` / `auto_extract_memories`
//!    / `AUTO_MEMORY` 任一为 true。
//! 3. **可见消息存在** —— `visible_message_count >= MIN_VISIBLE_MESSAGES` (=1)。
//! 4. **不在提取中** —— per-session `extraction_in_progress` atomic flag (CAS
//!    0→1)。
//! 5. **有新写入** —— 自上次抽取以来的 turn 计数 >= `turns_since_last_extraction`
//!    阈值。
//! 6. **turn 节流** —— `MIN_TURNS_BETWEEN_EXTRACTIONS` (=3) 防 turn 高频
//!    触发抢占。
//!
//! 触发条件最终式：6 重门控全部通过 → trigger；任一失败 → skip + reason。
//!
//! # 写盘形态（two-step write）
//!
//! - **Step 1** —— 解析 LLM 输出中的多个 memory block（`---` frontmatter
//!   分隔），每个写为独立 `user_*.md` / `feedback_*.md` / `project_*.md` /
//!   `reference_*.md` 文件，使用 `atomic_write` 保原子性。
//! - **Step 2** —— 在 `MEMORY.md` 索引中追加 `- [Title](file.md) — hook`
//!   行（最多 200 行，避免被截断；重复 entry 自动 dedup）。
//!
//! # 去重策略
//!
//! - 用 `adapter::parse_ts_memory` 读现有 memdir 的 markdown 文件（P2.2
//!   引入的入口）。
//! - 用 `dedup_hash::memory_body_hash` 比对 body SHA-256 hash —— exact
//!   match → skip；name 冲突 → update existing；无 match → write new。
//! - 语义去重（embedding-based）的更全 `MemoryDeduplicator`（P2.4 引入
//!   `acosmi-memory-session::MemoryDeduplicator`）需要 `VectorStore` +
//!   `Embedder` + `LlmProvider` 三 trait stack 实例，orchestrator 端
//!   暂未 plumb 该 stack；本 PR 用 SHA-256 hash dedup 作为 P2 引入的实
//!   际可用层级，语义 dedup 可在后续 PR 切换。
//!
//! # 与 Tier-1 的对照
//!
//! Tier-1 (`tier1_session_memory.rs`) 是 per-turn 增量更新 SESSION.md
//! 单一文件；Tier-2 是 per-query 批量抽取多个 typed `*.md` 文件 +
//! MEMORY.md 索引。两者共享 P3.1 反向 IPC LLM 调用契约 +
//! `LlmCallEmitter` trait + per-orchestrator `pending` HashMap 模式。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, AtomicU8, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

use crate::adapter::parse_ts_memory;
use crate::atomic_write::{atomic_write, BoxError};
use crate::dedup_hash::{memory_body_hash, scan_body_hashes};
use crate::tier::{
    GateDecision, LlmCallParams, LlmCallRequestPayload, LlmCallResultPayload, LlmMessage, LlmUsage,
    MemoryTier, TierGate,
};

// ──────────────────────────────────────────────────────────────────────────
// 阈值常量（与 CrabCode TS / CrabClaw Go 同源真值）
// ──────────────────────────────────────────────────────────────────────────

/// gate 3 最小可见消息数。CrabClaw `tier2MinVisibleMessages = 1`，
/// CrabCode TS `countModelVisibleMessagesSince` 返 0 时跳过；语义一致。
pub const MIN_VISIBLE_MESSAGES: u32 = 1;

/// gate 6 turn 节流阈值。CrabClaw 默认 1（每次都提取），但 CrabCode
/// `tengu_bramble_lintel` feature flag 实际 production 用 3。本实现取 3
/// 作为 CrabCode 现网默认（避免每 turn 都 LLM 调用打满）。
pub const MIN_TURNS_BETWEEN_EXTRACTIONS: u32 = 3;

/// LLM 调用反向 IPC 等待超时（30s）。设计取自 CrabClaw `tier2_policy.go`
/// 一致超时配置；超时即视为 LLM 调用失败，processor 释放 oneshot pending
/// 槽并返回 error；本 turn 跳过写盘，turn 计数不前推（下一 turn 再尝试）。
pub const LLM_CALL_TIMEOUT_MS: u64 = 30_000;

/// MEMORY.md 索引最大行数。CrabClaw + CrabCode 一致硬阈值 —— 防止
/// 索引被 LLM 全文截断（system prompt slot 有限）。超出后新 entry 追加
/// 但用户应人工归档。
pub const MEMORY_MD_INDEX_MAX_LINES: usize = 200;

// ──────────────────────────────────────────────────────────────────────────
// Tier-2 prompt 模板（**Rust 字符串常量，不进系统 prompt 体系**）
//
// CLAUDE.md §硬约束 #15 第 8 条铁律：Tier1/2/3 prompt 模板在 orchestrator
// 内嵌 Rust 字符串常量，**不**改动 `src/services/extractMemories/prompts.ts`
// 等 5 个严格档（用户裁决）。设计借鉴 CrabClaw
// `prompts/tier2_extract_auto.md`，重写为全英文、不 paste 中文、不写品牌
// 字面（无 LLM 模型名 / family / 价格 hardcode；§硬约束 #1）。
// ──────────────────────────────────────────────────────────────────────────

/// Tier-2 extraction prompt template. Pure-English, model-agnostic; the LLM
/// call routing is decided on the TS side via SDK after this template lands
/// in the `LlmCallRequestPayload::messages` list. Placeholders filled at
/// per-query extraction time:
///
/// - `{{new_message_count}}` — number of recent messages to analyze.
/// - `{{existing_manifest}}` — formatted list of existing memory files
///   (empty if memdir has no `*.md` yet).
///
/// No model brand / version / pricing literal appears here (§硬约束 #1).
pub const TIER2_EXTRACT_PROMPT: &str = r#"You are the memory extraction subagent for the running assistant session.

Analyze the most recent ~{{new_message_count}} messages above and use them to update the persistent memory system.

Your output is parsed by an automated writer; produce ONLY structured memory blocks (zero free-form commentary, zero markdown fences, zero LLM disclaimers).

# Existing memories

{{existing_manifest}}

# Output format

Emit zero or more memory blocks, each in this exact form:

```
---
name: <stable_snake_case_id>
type: <user|feedback|project|reference>
description: <one-line description under 150 chars>
importance: <integer 1-10 — durable significance: 1 trivial, 5 useful preference, 10 identity-level>
keywords: <comma-separated retrieval keywords, at most 6, may mix languages; omit the line if none>
---

<memory body — full prose, can span multiple paragraphs>
```

Then emit ONE blank line, then the matching MEMORY.md index line:

```
- [Title](<stable_snake_case_id>.md) — one-line hook under 150 chars
```

# Rules

- Save a memory only if the recent messages contain a durable preference, fact, decision, or recurring pattern.
- Reuse an existing `name:` to update that memory; pick a new `name:` only for genuinely new topics.
- When a `type: user` fact clearly applies across ALL of the user's projects (identity, working style, global preference — not a project-specific detail), add the frontmatter line `promote_hint: global` so the UI can suggest promoting it to global memory.
- Do NOT include sensitive data (credentials, tokens, personal addresses).
- Do NOT save short-lived debugging state, single-use file paths, or anything that will be obsolete in a week.
- If nothing in the recent messages warrants saving, return an empty output. An empty output is a valid result."#;

// ──────────────────────────────────────────────────────────────────────────
// Gate input / output
// ──────────────────────────────────────────────────────────────────────────

/// Gate input — passed from TS-side per-query trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExtractGateInput {
    /// `false` = local-mode (gate 1 pass); `true` = remote → skip per design.
    pub is_remote: bool,
    /// Session key (per-session state map key + file scope).
    pub session_key: String,
    /// New (since last extraction) model-visible message count.
    pub visible_message_count: u32,
    /// Recent messages compacted for the LLM (the gate doesn't read them,
    /// just propagates into the gate payload).
    pub recent_messages_summary: String,
    /// feature flag bag (mirrors `dream_config` / `turn_evaluator` shape).
    /// `auto_memory_enabled` / `auto_extract_memories` / `AUTO_MEMORY` any
    /// = true gates 2 pass.
    #[serde(default)]
    pub feature_flags: BTreeMap<String, bool>,
}

/// Gate output — propagated to `ExtractProcessor::process`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ExtractGateOutput {
    /// Messages for the reverse-IPC LLM call (system template + user
    /// payload). Built by the gate from the prompt template + the
    /// processor-supplied existing manifest (filled per-call).
    pub messages: Vec<LlmMessage>,
    /// New message count recorded at trigger time — diagnostic only.
    pub visible_message_count_at_trigger: u32,
}

/// Tier-2 gate evaluation error.
#[derive(Debug, thiserror::Error)]
pub enum ExtractGateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ──────────────────────────────────────────────────────────────────────────
// Per-session state
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Tier2SessionState {
    /// gate 6 — turn counter since last extraction. CAS-promoted to 0 by
    /// processor on successful settle.
    turns_since_last_extraction: AtomicU64,
    /// gate 4 — extraction in progress flag (0 = idle, 1 = busy).
    extraction_in_progress: AtomicU8,
    /// extraction started unix_ms (0 = idle). Diagnostic / stale detection.
    extraction_started_at_ms: AtomicU64,
}

// ──────────────────────────────────────────────────────────────────────────
// ExtractGate (TierGate trait impl)
// ──────────────────────────────────────────────────────────────────────────

/// Deterministic Tier-2 gate. No IO; no LLM; just decides whether to
/// trigger + assembles the LLM payload.
pub struct ExtractGate {
    sessions: Arc<Mutex<BTreeMap<String, Arc<Tier2SessionState>>>>,
}

impl Default for ExtractGate {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl ExtractGate {
    pub fn new() -> Self {
        Self::default()
    }

    async fn get_or_init_state(&self, session_key: &str) -> Arc<Tier2SessionState> {
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(session_key) {
            return Arc::clone(existing);
        }
        let state = Arc::new(Tier2SessionState::default());
        sessions.insert(session_key.to_string(), Arc::clone(&state));
        state
    }

    fn is_feature_enabled(flags: &BTreeMap<String, bool>) -> bool {
        for key in [
            "auto_extract_memories",
            "auto_memory_enabled",
            "AUTO_MEMORY",
        ] {
            if matches!(flags.get(key), Some(true)) {
                return true;
            }
        }
        false
    }

    /// Bump the per-session turn counter. Called by TS-side at the end of
    /// each query that did *not* trigger Tier-2 extraction (so the counter
    /// grows until the gate 6 threshold is met). Exposed publicly so the
    /// dispatcher can call it via the orchestrator IPC handler.
    pub async fn bump_turn(&self, session_key: &str) -> u64 {
        let state = self.get_or_init_state(session_key).await;
        state
            .turns_since_last_extraction
            .fetch_add(1, Ordering::AcqRel)
            + 1
    }
}

#[async_trait]
impl TierGate for ExtractGate {
    type GateInput = ExtractGateInput;
    type GateOutput = ExtractGateOutput;
    type Error = ExtractGateError;

    async fn evaluate_gate(
        &self,
        input: Self::GateInput,
    ) -> Result<GateDecision<Self::GateOutput>, Self::Error> {
        // ── Gate 1: 非 remote 模式 ──
        if input.is_remote {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("remote_mode".to_string()),
            });
        }

        // ── Gate 2: auto-memory 开启 ──
        if !Self::is_feature_enabled(&input.feature_flags) {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("feature_disabled".to_string()),
            });
        }

        // ── Gate 3: 可见消息存在 ──
        if input.visible_message_count < MIN_VISIBLE_MESSAGES {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("no_visible_messages".to_string()),
            });
        }

        let state = self.get_or_init_state(&input.session_key).await;

        // ── Gate 4: 不在提取中 ──
        // CAS check only (we don't seize it here; the processor seizes
        // it). A racing extraction in flight is observable by reading the
        // flag — if busy, we tell the caller to skip this turn.
        if state.extraction_in_progress.load(Ordering::Acquire) != 0 {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("extraction_in_progress".to_string()),
            });
        }

        // ── Gate 5: 有新写入 (turn 计数推进) + Gate 6: turn 节流 ──
        // Both gates collapse to "turn counter >= threshold". Bump the
        // counter on each evaluate so the threshold is reached deterministi-
        // cally; reset is done by the processor on successful settle.
        let turns_since_last = state
            .turns_since_last_extraction
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        if turns_since_last < MIN_TURNS_BETWEEN_EXTRACTIONS as u64 {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("turn_throttle_unmet".to_string()),
            });
        }

        // 拼装 LLM payload。`{{existing_manifest}}` 留待 processor 在 process
        // 时填充（因为只有 processor 知道 memory_dir）。这里先注入
        // `{{new_message_count}}` 占位。
        let mut system_content = TIER2_EXTRACT_PROMPT.to_string();
        system_content = system_content.replace(
            "{{new_message_count}}",
            &input.visible_message_count.to_string(),
        );
        // existing_manifest 占位 — processor 负责替换。
        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: system_content,
            },
            LlmMessage {
                role: "user".to_string(),
                content: format!(
                    "Recent messages (compacted):\n\n{}",
                    input.recent_messages_summary
                ),
            },
        ];

        Ok(GateDecision {
            should_trigger: true,
            payload: Some(ExtractGateOutput {
                messages,
                visible_message_count_at_trigger: input.visible_message_count,
            }),
            skip_reason: None,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// ExtractProcessor (reverse IPC LLM driver + two-step writer)
// ──────────────────────────────────────────────────────────────────────────

/// Per-query extraction request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExtractProcessInput {
    /// Session key (per-session state key + state advance scope).
    pub session_key: String,
    /// Absolute path to the memory directory (where `*.md` + `MEMORY.md` land).
    pub memory_dir: PathBuf,
    /// Gate payload from the preceding `evaluate_gate` call.
    pub gate_payload: ExtractGateOutput,
    /// Optional model hint (TS side decides actual model selection; plumbed
    /// only as a hint, not a brand-literal hardcode — §硬约束 #1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    /// Sampling parameters (None = TS uses SDK defaults).
    #[serde(default)]
    pub params: LlmCallParams,
}

/// Per-query extraction result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ExtractProcessOutput {
    /// Wire identifier of the LLM call request that was issued.
    pub req_id: String,
    /// Paths of `.md` files written / updated (absolute).
    pub written_paths: Vec<PathBuf>,
    /// Path of the `MEMORY.md` index (absolute). Empty if the LLM produced
    /// no blocks.
    pub memory_md_path: Option<PathBuf>,
    /// Number of memory blocks parsed from the LLM output.
    pub blocks_parsed: usize,
    /// Number of blocks written (after dedup skips).
    pub blocks_written: usize,
    /// Number of blocks that were exact duplicates (body-hash match) and
    /// skipped.
    pub blocks_deduped: usize,
    /// LLM token usage (mirrors `LlmCallResultPayload::usage`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsageWire>,
}

/// Mirror of `LlmUsage` — kept independent of `tier::LlmUsage` so the public
/// wire shape doesn't accidentally couple Processor's output to its input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct LlmUsageWire {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractProcessError {
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
    #[error("another extraction already in progress for session")]
    Busy,
}

/// Reverse IPC emitter — abstracted so tests can inject an in-memory mock.
#[async_trait]
pub trait LlmCallEmitter: Send + Sync {
    async fn emit_request(&self, request: LlmCallRequestPayload);
}

/// In-memory mock emitter (recording). Mirrors the Tier-1 RecordingEmitter
/// shape; kept independent so Tier-2 tests don't import Tier-1 internals.
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

/// Tier-2 processor — owns gate, pending oneshot map, emitter.
pub struct ExtractProcessor {
    gate: Arc<ExtractGate>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<LlmCallResultPayload>>>>,
    emitter: Arc<dyn LlmCallEmitter>,
    req_id_counter: AtomicU64,
}

impl ExtractProcessor {
    pub fn new(gate: Arc<ExtractGate>, emitter: Arc<dyn LlmCallEmitter>) -> Self {
        Self {
            gate,
            pending: Arc::new(Mutex::new(HashMap::new())),
            emitter,
            req_id_counter: AtomicU64::new(0),
        }
    }

    pub fn gate(&self) -> &Arc<ExtractGate> {
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

    fn next_req_id(&self) -> String {
        let n = self.req_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("tier2-{n}-{}", now_ms())
    }

    /// Run one Tier-2 extraction: emit reverse IPC LLM call request, await
    /// the result, parse blocks, two-step write (`*.md` + `MEMORY.md`).
    pub async fn process(
        &self,
        input: ExtractProcessInput,
    ) -> Result<ExtractProcessOutput, ExtractProcessError> {
        let state = self.gate.get_or_init_state(&input.session_key).await;

        // Concurrency guard — only one extraction at a time per session.
        if state
            .extraction_in_progress
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ExtractProcessError::Busy);
        }
        state
            .extraction_started_at_ms
            .store(now_ms(), Ordering::Release);

        struct GuardReleaser<'a> {
            state: &'a Tier2SessionState,
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

        // Build existing-manifest snapshot from memdir (uses adapter +
        // dedup_hash scanner). Failures are non-fatal — empty manifest is
        // a valid input to the LLM template.
        let existing_manifest = build_existing_manifest(&input.memory_dir).unwrap_or_default();

        // Patch the gate payload's system message to replace
        // `{{existing_manifest}}` with the freshly-scanned snapshot. The
        // gate emitted the template verbatim; processor finalizes.
        let mut messages = input.gate_payload.messages.clone();
        for m in messages.iter_mut() {
            if m.role == "system" {
                m.content = m
                    .content
                    .replace("{{existing_manifest}}", &existing_manifest);
            }
        }

        // 1. Register pending oneshot + emit reverse IPC request.
        let req_id = self.next_req_id();
        let (tx, rx) = oneshot::channel::<LlmCallResultPayload>();
        {
            let mut map = self.pending.lock().await;
            map.insert(req_id.clone(), tx);
        }

        let request_payload = LlmCallRequestPayload {
            req_id: req_id.clone(),
            tier: MemoryTier::Extract,
            phase: None,
            messages,
            model_hint: input.model_hint.clone(),
            params: input.params.clone(),
        };
        self.emitter.emit_request(request_payload).await;

        // 2. Await result (timeout).
        let result =
            match tokio::time::timeout(Duration::from_millis(LLM_CALL_TIMEOUT_MS), rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_recv_err)) => {
                    self.pending.lock().await.remove(&req_id);
                    return Err(ExtractProcessError::Shutdown);
                }
                Err(_elapsed) => {
                    self.pending.lock().await.remove(&req_id);
                    return Err(ExtractProcessError::Timeout(LLM_CALL_TIMEOUT_MS));
                }
            };

        // 3. Validate result.
        if let Some(err) = result.error.as_ref() {
            return Err(ExtractProcessError::LlmFailure(err.clone()));
        }
        let response = result
            .response
            .ok_or_else(|| ExtractProcessError::LlmFailure("empty response".to_string()))?;

        // 4. Parse LLM output into blocks. Empty output is a valid result
        // (LLM decided nothing's worth saving); promote the turn counter
        // anyway so we don't re-call on the next turn.
        let blocks = parse_extraction_blocks(&response);
        let blocks_parsed = blocks.len();

        if blocks.is_empty() {
            // Empty extraction is valid — reset throttle counter so we
            // don't re-trigger immediately.
            state
                .turns_since_last_extraction
                .store(0, Ordering::Release);
            let usage = result.usage.map(into_usage_wire);
            return Ok(ExtractProcessOutput {
                req_id,
                written_paths: Vec::new(),
                memory_md_path: None,
                blocks_parsed: 0,
                blocks_written: 0,
                blocks_deduped: 0,
                usage,
            });
        }

        // 5. Scan existing memdir for dedup. Failures → treat memdir as
        // empty (best-effort; we'd rather write a duplicate than fail).
        let existing_hashes = scan_body_hashes(&input.memory_dir)
            .map(|records| {
                records
                    .into_iter()
                    .map(|r| (r.hash_hex.clone(), r.path.clone()))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        // 6. Two-step write: per block.
        tokio::fs::create_dir_all(&input.memory_dir).await?;
        let mut written_paths = Vec::new();
        let mut blocks_written = 0_usize;
        let mut blocks_deduped = 0_usize;
        let mut new_index_lines = Vec::new();
        // W6 (6c) — 本轮新写记忆的重要性积分增量。
        let mut importance_delta: u64 = 0;

        for block in blocks {
            // Step 1: dedup by body hash.
            let body_hash_hex = hex_encode(&memory_body_hash(&block.full_content));
            if let Some(existing_path) = existing_hashes.get(&body_hash_hex) {
                log::debug!(
                    "tier2: skipping exact-body-hash duplicate (block name={}) → {:?}",
                    block.name,
                    existing_path
                );
                blocks_deduped += 1;
                continue;
            }

            // Step 2: name-based path resolution. If name collides with an
            // existing file we overwrite (update); if not, write new.
            let filename = format!("{}.md", sanitize_name(&block.name));
            let target_path = input.memory_dir.join(&filename);

            let bytes = block.full_content.as_bytes();
            atomic_write(&target_path, bytes)
                .await
                .map_err(|e: BoxError| ExtractProcessError::Write(e.to_string()))?;
            written_paths.push(target_path);
            blocks_written += 1;
            // W-MEMORY-SYNERGY W6 (2026-07-16, 6c) — 新写记忆的 importance
            // 进积分（缺失/非数字 = 0，不计分）。
            importance_delta = importance_delta.saturating_add(
                crate::importance_pressure::parse_importance(&block.full_content),
            );

            // Capture the LLM-supplied index line (only for new files).
            if !block.index_line.is_empty() {
                new_index_lines.push(block.index_line.clone());
            }
        }

        // W6 (6c) — 重要性积分落盘：做梦 gate 的事件驱动提前通道（Park et
        // al. reflection 阈值机制）。fail-soft：积分 IO 失败不影响提取。
        // project_state_dir = memory_dir 父目录（与 dream/session 链同派生）。
        if importance_delta > 0 {
            if let Some(project_state_dir) = input.memory_dir.parent() {
                crate::importance_pressure::add_importance(
                    project_state_dir,
                    importance_delta,
                    crate::extract_archive::now_ms(),
                )
                .await;
            }
        }

        // 7. Update MEMORY.md index (Step 2 of two-step write).
        let memory_md_path = if !new_index_lines.is_empty() {
            let path = input.memory_dir.join("MEMORY.md");
            append_to_memory_index(&path, &new_index_lines).await?;
            Some(path)
        } else {
            None
        };

        // 8. Reset throttle counter — next extraction needs another
        // `MIN_TURNS_BETWEEN_EXTRACTIONS` turns.
        state
            .turns_since_last_extraction
            .store(0, Ordering::Release);

        let usage = result.usage.map(into_usage_wire);
        Ok(ExtractProcessOutput {
            req_id,
            written_paths,
            memory_md_path,
            blocks_parsed,
            blocks_written,
            blocks_deduped,
            usage,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Existing-manifest builder (uses adapter::parse_ts_memory)
// ──────────────────────────────────────────────────────────────────────────

/// Build a human-readable manifest of existing memory files for the LLM
/// prompt. Uses `adapter::parse_ts_memory` (P2.2 entry) so the parser
/// stays consistent with the rest of the orchestrator's adapter layer.
///
/// Returns an empty string on IO failure (best-effort; the LLM still works
/// with an empty manifest).
pub fn build_existing_manifest(memory_dir: &Path) -> Result<String, BoxError> {
    if !memory_dir.exists() {
        return Ok(String::new());
    }
    let mut lines: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(memory_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == "MEMORY.md" || name == "SESSION.md" {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let path_str = path.to_string_lossy().to_string();
        // `user` parameter is "" — we don't track per-user attribution
        // for orchestrator-side manifest building.
        let parsed = match parse_ts_memory(&path_str, &content, "") {
            Ok(p) => p,
            Err(_) => continue,
        };
        let name_hint = parsed.name_hint.unwrap_or_else(|| parsed.file_stem.clone());
        let type_str = parsed.raw_type.unwrap_or_else(|| "(untyped)".to_string());
        lines.push(format!("- {name_hint} [{type_str}] ({name})"));
    }
    Ok(lines.join("\n"))
}

// ──────────────────────────────────────────────────────────────────────────
// LLM output parser
// ──────────────────────────────────────────────────────────────────────────

/// A memory block parsed from the LLM output.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractionBlock {
    /// `name:` field from the frontmatter (snake_case id).
    pub name: String,
    /// Memory `type` field (user / feedback / project / reference).
    pub kind: String,
    /// Full memory content (frontmatter + body), ready to write verbatim.
    pub full_content: String,
    /// MEMORY.md index line (`- [Title](file.md) — hook`), empty if the
    /// LLM didn't emit one.
    pub index_line: String,
}

/// Parse the LLM output into memory blocks.
///
/// Expected format (per `TIER2_EXTRACT_PROMPT`):
///
/// ```text
/// ---
/// name: some_memory_name
/// type: user|feedback|project|reference
/// description: ...
/// ---
///
/// Memory content here...
///
/// - [Title](some_memory_name.md) — one-line hook
/// ```
///
/// Multiple blocks may appear concatenated. MEMORY.md index lines start
/// with `- [` and are collected separately.
pub fn parse_extraction_blocks(raw: &str) -> Vec<ExtractionBlock> {
    // Strip optional markdown code fences the LLM might wrap output in.
    let raw = strip_fences(raw.trim());
    if raw.is_empty() {
        return Vec::new();
    }

    let lines: Vec<&str> = raw.lines().collect();
    let mut blocks: Vec<ExtractionBlock> = Vec::new();
    let mut pending_indexes: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        // Standalone MEMORY.md index line — buffered until we attach to a
        // block (matched by filename or to the most-recent block).
        if line.starts_with("- [") && line.contains("](") {
            pending_indexes.push(line.to_string());
            i += 1;
            continue;
        }

        // Block start: frontmatter opener "---" on its own line.
        if line == "---" {
            // Find the closing "---".
            let mut j = i + 1;
            let mut closing = None;
            while j < lines.len() {
                if lines[j].trim() == "---" {
                    closing = Some(j);
                    break;
                }
                j += 1;
            }
            let Some(closing) = closing else {
                // No closer → bail, treat rest as garbage.
                break;
            };

            // Parse frontmatter for name + type.
            let mut name = String::new();
            let mut kind = String::new();
            for fm_line in &lines[i + 1..closing] {
                let fm = fm_line.trim();
                if let Some(v) = fm.strip_prefix("name:") {
                    name = v.trim().to_string();
                } else if let Some(v) = fm.strip_prefix("type:") {
                    kind = v.trim().to_string();
                }
            }

            // Collect body lines until next "---" block opener or EOF.
            let body_start = closing + 1;
            let mut body_end = lines.len();
            let mut next_block_idx = lines.len();
            let mut k = body_start;
            while k < lines.len() {
                if lines[k].trim() == "---" {
                    body_end = k;
                    next_block_idx = k;
                    break;
                }
                k += 1;
            }

            // Build body_lines (raw body before fixup) for further pruning.
            let mut body_lines: Vec<&str> = lines[body_start..body_end].to_vec();
            while let Some(last) = body_lines.last() {
                if last.trim().is_empty() {
                    body_lines.pop();
                } else {
                    break;
                }
            }

            // Extract trailing MEMORY.md index line from the body — the
            // prompt template asks the LLM to emit the index line just
            // below the body. If the last non-empty body line matches
            // `- [...]( ... )`, pull it out as the block's index_line
            // (and exclude it from the typed memory file body — index
            // lines belong only in MEMORY.md, never inside the typed
            // `*.md` file).
            let mut trailing_index_line: Option<String> = None;
            if let Some(last) = body_lines.last() {
                let trimmed = last.trim();
                if trimmed.starts_with("- [") && trimmed.contains("](") {
                    trailing_index_line = Some(trimmed.to_string());
                    body_lines.pop();
                    while let Some(l) = body_lines.last() {
                        if l.trim().is_empty() {
                            body_lines.pop();
                        } else {
                            break;
                        }
                    }
                }
            }

            let mut full = String::new();
            full.push_str("---\n");
            for fm in &lines[i + 1..closing] {
                full.push_str(fm);
                full.push('\n');
            }
            full.push_str("---\n\n");
            for b in &body_lines {
                full.push_str(b);
                full.push('\n');
            }

            // Resolve the block's index_line:
            //   1. Prefer the trailing index line we just pulled out of
            //      the body.
            //   2. Otherwise attach a pending standalone index line that
            //      mentions this block's filename.
            //   3. Otherwise take the first unattached pending line.
            let block_filename = format!("{}.md", sanitize_name(&name));
            let index_line = if let Some(line) = trailing_index_line {
                line
            } else {
                let attached_idx = pending_indexes
                    .iter()
                    .position(|line| line.contains(&format!("({block_filename})")))
                    .or(if pending_indexes.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                match attached_idx {
                    Some(idx) => pending_indexes.remove(idx),
                    None => String::new(),
                }
            };

            if !name.is_empty() {
                // Trim trailing whitespace but preserve a single trailing
                // newline — `dedup_hash::memory_body_hash` is sensitive to
                // body suffix whitespace, and `extract_frontmatter`
                // consumes whitespace after the closing `---`. Keeping one
                // `\n` ensures hash parity with files planted on disk
                // (which conventionally end with `\n`).
                let mut content = full.trim_end().to_string();
                content.push('\n');
                blocks.push(ExtractionBlock {
                    name,
                    kind,
                    full_content: content,
                    index_line,
                });
            }

            i = next_block_idx;
            continue;
        }

        i += 1;
    }

    blocks
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(stripped) = s.strip_prefix("```markdown\n") {
        return stripped
            .rsplit_once("\n```")
            .map(|(b, _)| b)
            .unwrap_or(stripped);
    }
    if let Some(stripped) = s.strip_prefix("```\n") {
        return stripped
            .rsplit_once("\n```")
            .map(|(b, _)| b)
            .unwrap_or(stripped);
    }
    s
}

// ──────────────────────────────────────────────────────────────────────────
// MEMORY.md two-step writer (Step 2)
// ──────────────────────────────────────────────────────────────────────────

/// Append index lines to MEMORY.md, dedup against existing content, and
/// preserve atomic write semantics. Cap at `MEMORY_MD_INDEX_MAX_LINES`:
/// excess lines are still written (we don't truncate user content) but a
/// warning is logged.
pub async fn append_to_memory_index(
    memory_md_path: &Path,
    new_lines: &[String],
) -> Result<(), ExtractProcessError> {
    let existing = match tokio::fs::read_to_string(memory_md_path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ExtractProcessError::Io(e)),
    };

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    let mut added_count = 0;
    for line in new_lines {
        if content.contains(line) {
            continue; // dedup against existing entry.
        }
        content.push_str(line);
        content.push('\n');
        added_count += 1;
    }
    let total_lines = content.lines().count();
    if total_lines > MEMORY_MD_INDEX_MAX_LINES {
        log::warn!(
            "tier2: MEMORY.md exceeds {MEMORY_MD_INDEX_MAX_LINES} lines (now {total_lines}); user should consolidate"
        );
    }
    if added_count == 0 {
        return Ok(());
    }
    atomic_write(memory_md_path, content.as_bytes())
        .await
        .map_err(|e: BoxError| ExtractProcessError::Write(e.to_string()))
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sanitize a memory name into a safe filename stem. Replaces any
/// character outside `[A-Za-z0-9_.-]` with `_`.
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

fn into_usage_wire(u: LlmUsage) -> LlmUsageWire {
    LlmUsageWire {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
        m.insert("auto_extract_memories".to_string(), true);
        m
    }

    fn flags_disabled() -> BTreeMap<String, bool> {
        BTreeMap::new()
    }

    fn input(
        session_key: &str,
        is_remote: bool,
        flags: BTreeMap<String, bool>,
        visible_count: u32,
    ) -> ExtractGateInput {
        ExtractGateInput {
            is_remote,
            session_key: session_key.to_string(),
            visible_message_count: visible_count,
            recent_messages_summary: "u: hi\na: hello".to_string(),
            feature_flags: flags,
        }
    }

    // ── Gate 1 ──
    #[tokio::test]
    async fn gate1_remote_mode_rejected() {
        let gate = ExtractGate::new();
        let d = gate
            .evaluate_gate(input("sess-1", true, flags_enabled(), 5))
            .await
            .unwrap();
        assert!(!d.should_trigger);
        assert_eq!(d.skip_reason.as_deref(), Some("remote_mode"));
    }

    // ── Gate 2 ──
    #[tokio::test]
    async fn gate2_feature_disabled() {
        let gate = ExtractGate::new();
        let d = gate
            .evaluate_gate(input("sess-2", false, flags_disabled(), 5))
            .await
            .unwrap();
        assert!(!d.should_trigger);
        assert_eq!(d.skip_reason.as_deref(), Some("feature_disabled"));
    }

    // ── Gate 3 ──
    #[tokio::test]
    async fn gate3_no_visible_messages() {
        let gate = ExtractGate::new();
        let d = gate
            .evaluate_gate(input("sess-3", false, flags_enabled(), 0))
            .await
            .unwrap();
        assert!(!d.should_trigger);
        assert_eq!(d.skip_reason.as_deref(), Some("no_visible_messages"));
    }

    // ── Gate 4 ──
    #[tokio::test]
    async fn gate4_extraction_in_progress_skipped() {
        let gate = ExtractGate::new();
        let state = gate.get_or_init_state("sess-4").await;
        state.extraction_in_progress.store(1, Ordering::Release);
        let d = gate
            .evaluate_gate(input("sess-4", false, flags_enabled(), 5))
            .await
            .unwrap();
        assert!(!d.should_trigger);
        assert_eq!(d.skip_reason.as_deref(), Some("extraction_in_progress"));
    }

    // ── Gate 5+6: turn throttle ──
    #[tokio::test]
    async fn gate6_turn_throttle_first_call_unmet() {
        let gate = ExtractGate::new();
        let d = gate
            .evaluate_gate(input("sess-5", false, flags_enabled(), 5))
            .await
            .unwrap();
        // First call → turns_since_last = 1 < 3 → throttle.
        assert!(!d.should_trigger);
        assert_eq!(d.skip_reason.as_deref(), Some("turn_throttle_unmet"));
    }

    #[tokio::test]
    async fn gate6_turn_throttle_third_call_passes() {
        let gate = ExtractGate::new();
        for _ in 0..2 {
            let _ = gate
                .evaluate_gate(input("sess-6", false, flags_enabled(), 5))
                .await
                .unwrap();
        }
        // Third call → turns_since_last = 3 >= 3 → trigger.
        let d = gate
            .evaluate_gate(input("sess-6", false, flags_enabled(), 5))
            .await
            .unwrap();
        assert!(d.should_trigger);
        let payload = d.payload.expect("payload");
        assert_eq!(payload.visible_message_count_at_trigger, 5);
        assert_eq!(payload.messages.len(), 2);
        assert_eq!(payload.messages[0].role, "system");
        assert!(payload.messages[0].content.contains("memory extraction"));
        assert!(payload.messages[0]
            .content
            .contains("{{existing_manifest}}"));
    }

    // ── All gates pass via direct state mutation ──
    #[tokio::test]
    async fn all_gates_pass_returns_payload() {
        let gate = ExtractGate::new();
        let state = gate.get_or_init_state("sess-pass").await;
        // Pre-promote turn counter to threshold-1 so first call passes.
        state.turns_since_last_extraction.store(
            (MIN_TURNS_BETWEEN_EXTRACTIONS - 1) as u64,
            Ordering::Release,
        );
        let d = gate
            .evaluate_gate(input("sess-pass", false, flags_enabled(), 10))
            .await
            .unwrap();
        assert!(d.should_trigger);
        let p = d.payload.unwrap();
        assert_eq!(p.visible_message_count_at_trigger, 10);
    }

    // ── parse_extraction_blocks: single block ──
    #[test]
    fn parser_single_block_with_index_line() {
        let raw = r#"---
name: project_demo
type: project
description: A demo project
---

This is the body of the demo project memory.

It has multiple paragraphs.

- [Demo Project](project_demo.md) — Sample memory hook"#;
        let blocks = parse_extraction_blocks(raw);
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.name, "project_demo");
        assert_eq!(b.kind, "project");
        assert!(b.full_content.contains("name: project_demo"));
        assert!(b.full_content.contains("This is the body"));
        assert!(b.full_content.contains("multiple paragraphs"));
        assert!(b.index_line.starts_with("- [Demo Project]"));
    }

    // ── parse_extraction_blocks: multi-block ──
    #[test]
    fn parser_multi_block() {
        let raw = r#"---
name: user_pref
type: user
description: User preference
---

User prefers concise output.

- [User Pref](user_pref.md) — concise output

---
name: feedback_test
type: feedback
description: Test feedback
---

Always run tests before commit.

- [Test First](feedback_test.md) — run tests"#;
        let blocks = parse_extraction_blocks(raw);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].name, "user_pref");
        assert_eq!(blocks[1].name, "feedback_test");
        assert!(blocks[0].index_line.contains("user_pref.md"));
        assert!(blocks[1].index_line.contains("feedback_test.md"));
    }

    // ── parse_extraction_blocks: empty output ──
    #[test]
    fn parser_empty_output_returns_empty() {
        assert_eq!(parse_extraction_blocks("").len(), 0);
        assert_eq!(parse_extraction_blocks("   \n  \n").len(), 0);
    }

    // ── parse_extraction_blocks: with markdown fence wrapper ──
    #[test]
    fn parser_strips_markdown_fences() {
        let raw = "```markdown\n---\nname: x\ntype: user\n---\n\nBody.\n```";
        let blocks = parse_extraction_blocks(raw);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "x");
    }

    // ── sanitize_name: safe chars ──
    #[test]
    fn sanitize_name_replaces_unsafe() {
        assert_eq!(sanitize_name("ok_name-1.2"), "ok_name-1.2");
        assert_eq!(sanitize_name("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_name("with space"), "with_space");
    }

    // ── build_existing_manifest via adapter::parse_ts_memory ──
    #[tokio::test]
    async fn manifest_lists_existing_memories_via_adapter() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path();
        // Plant 2 typed memory files + 1 untyped + MEMORY.md (skipped).
        tokio::fs::write(
            memory_dir.join("user_pref.md"),
            "---\nname: user_pref\ntype: user\ndescription: prefers Chinese\n---\n\nbody",
        )
        .await
        .unwrap();
        tokio::fs::write(
            memory_dir.join("feedback_quick.md"),
            "---\nname: feedback_quick\ntype: feedback\n---\n\nbody",
        )
        .await
        .unwrap();
        tokio::fs::write(
            memory_dir.join("MEMORY.md"),
            "- [User Pref](user_pref.md)\n",
        )
        .await
        .unwrap();

        let manifest = build_existing_manifest(memory_dir).unwrap();
        assert!(manifest.contains("user_pref"));
        assert!(manifest.contains("feedback_quick"));
        assert!(!manifest.contains("MEMORY.md"), "MEMORY.md must be skipped");
    }

    // ── Processor: full LLM roundtrip + two-step write ──
    #[tokio::test]
    async fn processor_roundtrip_writes_block_and_index() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");

        let gate = Arc::new(ExtractGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(ExtractProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let gate_payload = ExtractGateOutput {
            messages: vec![
                LlmMessage {
                    role: "system".to_string(),
                    content: TIER2_EXTRACT_PROMPT.to_string(),
                },
                LlmMessage {
                    role: "user".to_string(),
                    content: "u: hi\na: hello".to_string(),
                },
            ],
            visible_message_count_at_trigger: 4,
        };
        let input = ExtractProcessInput {
            session_key: "sess-rt".to_string(),
            memory_dir: memory_dir.clone(),
            gate_payload,
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let proc_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { proc_clone.process(input).await });

        // Wait for emit, then deliver the LLM response.
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

        // Deliver an LLM response with one extractable block.
        let response_body = r#"---
name: project_foo
type: project
description: Foo project
---

Foo project notes.

- [Foo Project](project_foo.md) — Foo project hook"#;
        let delivered = processor
            .deliver_result(LlmCallResultPayload {
                req_id: req_id.clone(),
                response: Some(response_body.to_string()),
                usage: Some(LlmUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                error: None,
            })
            .await;
        assert!(delivered);

        let output = task.await.unwrap().expect("process ok");
        assert_eq!(output.req_id, req_id);
        assert_eq!(output.blocks_parsed, 1);
        assert_eq!(output.blocks_written, 1);
        assert_eq!(output.blocks_deduped, 0);
        assert_eq!(output.written_paths.len(), 1);
        assert!(output.memory_md_path.is_some());

        // Verify Step 1: project_foo.md exists with frontmatter + body.
        let written = tokio::fs::read_to_string(&output.written_paths[0])
            .await
            .unwrap();
        assert!(written.contains("name: project_foo"));
        assert!(written.contains("Foo project notes"));

        // Verify Step 2: MEMORY.md contains the index line.
        let memory_md = tokio::fs::read_to_string(output.memory_md_path.unwrap())
            .await
            .unwrap();
        assert!(memory_md.contains("Foo project hook"));
        assert!(memory_md.contains("project_foo.md"));

        // Verify gate state reset.
        let state = gate.get_or_init_state("sess-rt").await;
        assert_eq!(state.turns_since_last_extraction.load(Ordering::Acquire), 0);
        assert_eq!(state.extraction_in_progress.load(Ordering::Acquire), 0);
    }

    // ── Processor: dedup against existing body-hash ──
    #[tokio::test]
    async fn processor_dedups_against_existing_body_hash() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();

        // Plant existing file with body == "Foo project notes.\n".
        let existing = r#"---
name: project_foo
type: project
---

Foo project notes.
"#;
        tokio::fs::write(memory_dir.join("project_foo.md"), existing)
            .await
            .unwrap();

        let gate = Arc::new(ExtractGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(ExtractProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let input = ExtractProcessInput {
            session_key: "sess-dedup".to_string(),
            memory_dir: memory_dir.clone(),
            gate_payload: ExtractGateOutput {
                messages: vec![LlmMessage {
                    role: "system".to_string(),
                    content: TIER2_EXTRACT_PROMPT.to_string(),
                }],
                visible_message_count_at_trigger: 2,
            },
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let proc_clone = Arc::clone(&processor);
        let task = tokio::spawn(async move { proc_clone.process(input).await });

        let mut req_id_opt = None;
        for _ in 0..50 {
            let recorded = emitter.recorded().await;
            if let Some(r) = recorded.first() {
                req_id_opt = Some(r.req_id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let req_id = req_id_opt.unwrap();

        // LLM emits a block whose body matches the existing file.
        let response = r#"---
name: project_foo_v2
type: project
description: duplicate body
---

Foo project notes.

- [Foo v2](project_foo_v2.md) — dup hook"#;
        processor
            .deliver_result(LlmCallResultPayload {
                req_id,
                response: Some(response.to_string()),
                usage: None,
                error: None,
            })
            .await;

        let output = task.await.unwrap().unwrap();
        assert_eq!(output.blocks_parsed, 1);
        // Dedup hit → not written.
        assert_eq!(output.blocks_written, 0);
        assert_eq!(output.blocks_deduped, 1);
        assert!(output.written_paths.is_empty());
        // MEMORY.md not updated (no new writes).
        assert!(output.memory_md_path.is_none());
    }

    // ── Processor: LLM error result ──
    #[tokio::test]
    async fn processor_llm_error_result_returns_llm_failure() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let gate = Arc::new(ExtractGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(ExtractProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let input = ExtractProcessInput {
            session_key: "sess-err".to_string(),
            memory_dir,
            gate_payload: ExtractGateOutput {
                messages: vec![],
                visible_message_count_at_trigger: 2,
            },
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
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: req_id.unwrap(),
                response: None,
                usage: None,
                error: Some("rate limited".to_string()),
            })
            .await;

        let result = task.await.unwrap();
        match result {
            Err(ExtractProcessError::LlmFailure(msg)) => assert_eq!(msg, "rate limited"),
            other => panic!("expected LlmFailure, got {other:?}"),
        }
    }

    // ── Processor: empty LLM output returns ok with zero writes ──
    #[tokio::test]
    async fn processor_empty_output_promotes_counter_no_write() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let gate = Arc::new(ExtractGate::new());
        let state = gate.get_or_init_state("sess-empty").await;
        state
            .turns_since_last_extraction
            .store(7, Ordering::Release);
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = Arc::new(ExtractProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        ));

        let input = ExtractProcessInput {
            session_key: "sess-empty".to_string(),
            memory_dir,
            gate_payload: ExtractGateOutput {
                messages: vec![],
                visible_message_count_at_trigger: 2,
            },
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
        processor
            .deliver_result(LlmCallResultPayload {
                req_id: req_id.unwrap(),
                response: Some("".to_string()),
                usage: None,
                error: None,
            })
            .await;

        let output = task.await.unwrap().unwrap();
        assert_eq!(output.blocks_parsed, 0);
        assert_eq!(output.blocks_written, 0);
        // Counter reset to 0 (we treat empty output as a settled extraction).
        let s = gate.get_or_init_state("sess-empty").await;
        assert_eq!(s.turns_since_last_extraction.load(Ordering::Acquire), 0);
    }

    // ── Processor: timeout / shutdown path ──
    #[tokio::test]
    async fn processor_shutdown_drops_pending_and_releases_guard() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let gate = Arc::new(ExtractGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = ExtractProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        );

        let input = ExtractProcessInput {
            session_key: "sess-shutdown".to_string(),
            memory_dir,
            gate_payload: ExtractGateOutput {
                messages: vec![],
                visible_message_count_at_trigger: 2,
            },
            model_hint: None,
            params: LlmCallParams::default(),
        };

        let processor_arc = Arc::new(processor);
        let p2 = Arc::clone(&processor_arc);
        let task = tokio::spawn(async move { p2.process(input).await });

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let recorded = emitter.recorded().await;
            if !recorded.is_empty() {
                break;
            }
        }

        // Simulate orchestrator shutdown by closing pending channels.
        {
            let mut map = processor_arc.pending.lock().await;
            for (_id, tx) in map.drain() {
                drop(tx);
            }
        }

        let result = task.await.unwrap();
        assert!(matches!(result, Err(ExtractProcessError::Shutdown)));

        // Guard released — second extraction should be allowed.
        let state = gate.get_or_init_state("sess-shutdown").await;
        assert_eq!(state.extraction_in_progress.load(Ordering::Acquire), 0);
    }

    // ── Processor: concurrent extraction → Busy ──
    #[tokio::test]
    async fn processor_concurrent_extraction_returns_busy() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let gate = Arc::new(ExtractGate::new());
        // Pre-seize the in-progress flag to simulate another extraction
        // already running.
        let state = gate.get_or_init_state("sess-busy").await;
        state.extraction_in_progress.store(1, Ordering::Release);
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = ExtractProcessor::new(
            Arc::clone(&gate),
            emitter.clone() as Arc<dyn LlmCallEmitter>,
        );

        let input = ExtractProcessInput {
            session_key: "sess-busy".to_string(),
            memory_dir,
            gate_payload: ExtractGateOutput {
                messages: vec![],
                visible_message_count_at_trigger: 2,
            },
            model_hint: None,
            params: LlmCallParams::default(),
        };
        let result = processor.process(input).await;
        assert!(matches!(result, Err(ExtractProcessError::Busy)));
    }

    // ── deliver_unknown_req_id returns false ──
    #[tokio::test]
    async fn deliver_unknown_req_id_returns_false() {
        let gate = Arc::new(ExtractGate::new());
        let emitter = Arc::new(RecordingEmitter::new());
        let processor = ExtractProcessor::new(gate, emitter as Arc<dyn LlmCallEmitter>);
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

    // ── append_to_memory_index: dedup and create ──
    #[tokio::test]
    async fn append_to_memory_index_dedups() {
        let tmp = TempDir::new().unwrap();
        let memory_md = tmp.path().join("MEMORY.md");
        tokio::fs::write(&memory_md, "- [Existing](existing.md) — pre\n")
            .await
            .unwrap();
        append_to_memory_index(
            &memory_md,
            &[
                "- [Existing](existing.md) — pre".to_string(), // dup → skipped
                "- [New](new.md) — hook".to_string(),
            ],
        )
        .await
        .unwrap();
        let content = tokio::fs::read_to_string(&memory_md).await.unwrap();
        let occurrences = content.matches("- [Existing]").count();
        assert_eq!(occurrences, 1);
        assert!(content.contains("- [New](new.md) — hook"));
    }

    #[tokio::test]
    async fn append_to_memory_index_creates_if_missing() {
        let tmp = TempDir::new().unwrap();
        let memory_md = tmp.path().join("MEMORY.md");
        append_to_memory_index(&memory_md, &["- [First](first.md) — hook".to_string()])
            .await
            .unwrap();
        let content = tokio::fs::read_to_string(&memory_md).await.unwrap();
        assert_eq!(content, "- [First](first.md) — hook\n");
    }

    // ── TierGate trait shape ──
    #[tokio::test]
    async fn tier_gate_trait_decision_shape_round_trip() {
        let gate = ExtractGate::new();
        let d = gate
            .evaluate_gate(input("sess-rt-shape", true, flags_enabled(), 5))
            .await
            .unwrap();
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["should_trigger"], false);
        assert_eq!(json["skip_reason"], "remote_mode");
    }
}
