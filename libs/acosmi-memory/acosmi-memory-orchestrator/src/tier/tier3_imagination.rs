//! W-MEMORY-DREAM-REBUILD v7 P3.5 — Tier-3 Imagination 5-layer confidence
//! pipeline（Rust 重写）。
//!
//! 设计借鉴 CrabClaw `runImaginationPipeline()`（只读参考归档，不 git copy；
//! 重写为 Rust）。Phase 3 收尾 PR：在 P3.4 dream 主管线之上加 Phase 5
//! Imagination + 5 层 confidence 管线，处理 LLM 自发产生的假设
//! （hypothesis）—— 这些假设既可能是新颖洞见，也可能是幻觉，必须经
//! 5 层 confidence 评估 + 人工 confirm 才能 promote 到 memdir 主区。
//!
//! 复用 P3.1 立的反向 IPC LLM 调用契约：orchestrator 通过
//! `memory/tier/llmCallRequest` notification 广播请求 → TS 端跑 SDK →
//! `memory/tier/llmCallResult` request 回写 → orchestrator 内部 pending
//! oneshot 按 `req_id` 匹配（详 `tier/mod.rs` 反向 IPC 时序图）。
//!
//! # 与 P3.4 AutoDream 的关系
//!
//! Imagination 是**独立 method** `memory.tier3.imagination.process`，
//! 不动 `tier3_auto_dream.rs` 主管线。调用方（P5.3 UI 或后续 KAIROS 触发）
//! 提交假设 + evidence_refs，本 processor 跑 5 层 confidence pipeline，
//! 写盘到 `imagination/review-queue/imagined_<hash>.md`（隔离路径，不混入
//! `dreams/`）。**promotion 必须人工 confirm**（P5.3 UI 实施）；本 PR
//! 只立 schema + 写盘，不实现 promotion 路径。
//!
//! # 5 层 confidence 管线
//!
//! L1 Self-RAG —— LLM 自评假设合理性（单次 LLM 调用返 0.0-1.0 score）。
//! L2 四维评分 —— LLM 评 novelty / consistency / groundedness /
//!                actionability（单次 LLM 调用返 4 score 0.0-1.0）。
//! L3 原子验证 —— 把假设拆原子陈述，逐一对 evidence_refs 调 LLM 验证
//!                （per-atom LLM 调用，返 supported/refuted/inconclusive）。
//! L4 加权融合 —— `final_confidence = L1*0.3 + L2.avg*0.4 + L3*0.3`。
//! L5 Promotion 阈值 —— ≥ 0.7 = review queue（confidence: high）；
//!                      0.5-0.7 = pending（confidence: medium）；
//!                      < 0.5 = expire（不写盘，记日志）。
//!
//! # req_id 命名约定
//!
//! 与 P3.4 tier3 dream（`tier3-<phase>-N-<ts>`）区分：本模块用
//! `tier3-imagination-<layer>-N-<ts>` 前缀。dispatcher 收到
//! `memory.tier.llm_call_result` 时 quad-deliver（tier1 + tier2 +
//! tier3-dream + tier3-imagination），按 `req_id` 前缀匹配 pending oneshot。
//!
//! # 写盘
//!
//! 走 `atomic_write::atomic_write`（与 Tier-1/Tier-2/Tier-3 dream 一致）。
//! 子目录 `imagination/review-queue/` 在 memory_dir 下创建；frontmatter
//! 含 `confidence` + `status: pending-review` + `expiry`（ISO 8601 +14d）。
//!
//! # 不变量
//!
//! - WHITELIST 22（imagination LLM 复用 P3.1 的 `memory/tier/llmCallRequest`；
//!   W-MEMORY-EVOLUTION PR-7 想象外部取证新增 `memory/tier/toolCallRequest`
//!   → 21→22，详 CLAUDE.md §硬约束 #11）
//! - AllowAnyOrigin 3 项不变（method 全 LocalOnly default-deny）
//! - 5 个严格档 prompts.ts 0 改（Tier prompt 模板在 orchestrator 内嵌 Rust 常量；
//!   CLAUDE.md §硬约束 #15 第 8 条）

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, Mutex};

use crate::atomic_write::{atomic_write, BoxError};
use crate::tier::{
    GateDecision, LlmCallParams, LlmCallRequestPayload, LlmCallResultPayload, LlmMessage, LlmUsage,
    MemoryTier, TierGate,
};

// ──────────────────────────────────────────────────────────────────────────
// 阈值常量
// ──────────────────────────────────────────────────────────────────────────

/// L5 promotion 阈值：final_confidence ≥ 此值 → review queue 显眼层
/// （frontmatter confidence: high）。CrabClaw 同源默认 0.7。
pub const PROMOTION_THRESHOLD: f64 = 0.7;

/// L5 expire 阈值：final_confidence < 此值 → 不写盘，记日志为 expired。
/// 介于 EXPIRE_THRESHOLD 与 PROMOTION_THRESHOLD 之间 = pending（medium）。
pub const EXPIRE_THRESHOLD: f64 = 0.5;

/// 加权融合权重：L1 Self-RAG 权重。
pub const L1_WEIGHT: f64 = 0.3;

/// 加权融合权重：L2 四维评分平均权重。
pub const L2_WEIGHT: f64 = 0.4;

/// 加权融合权重：L3 原子验证权重。
pub const L3_WEIGHT: f64 = 0.3;

/// review queue 文件 expiry 默认时长（天）。frontmatter `expiry` 字段
/// 写 ISO 8601 形式（`now + 14 day`）。
pub const REVIEW_EXPIRY_DAYS: u64 = 14;

/// LLM 调用反向 IPC 等待超时（每层独立 60s；与 P3.4 dream 一致）。
pub const LLM_CALL_TIMEOUT_MS: u64 = 60_000;

// ──────────────────────────────────────────────────────────────────────────
// Tier-3 Imagination prompt 模板（**Rust 字符串常量，不进系统 prompt 体系**）
//
// CLAUDE.md §硬约束 #15 第 8 条铁律：Tier1/2/3 prompt 模板在 orchestrator
// 内嵌 Rust 字符串常量，**不**改动 5 个严格档（用户裁决）。设计借鉴
// CrabClaw `runImaginationPipeline()` 的 prompt 套件，重写为全英文、不
// paste 中文、不写品牌字面（无 LLM 模型名 / family / 价格 hardcode；
// §硬约束 #1）。
// ──────────────────────────────────────────────────────────────────────────

/// L1 — Self-RAG: hypothesis 合理性自评。
///
/// Placeholders:
/// - `{{hypothesis}}` — the imagination hypothesis under evaluation.
/// - `{{context}}` — supporting context summary.
pub const TIER3_IMAGINATION_L1_SELFRAG_PROMPT: &str = r#"You are the imagination self-evaluation agent. Your job is to assess how plausible the given hypothesis is, on its own merits, before any external evidence check.

# Hypothesis under evaluation

{{hypothesis}}

# Supporting context (for orientation only — NOT evidence)

{{context}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "plausibility": <0.0..1.0>,
  "reasoning": "<one-line reasoning summary>"
}

Rules:
- `plausibility` is a single float in [0.0, 1.0].
  0.0 = obviously implausible / contradicts well-known facts.
  0.5 = neutral / cannot decide.
  1.0 = obviously plausible / common-knowledge consistent.
- Self-evaluate WITHOUT inventing supporting evidence. This layer is about a-priori plausibility, not factual verification.
- `reasoning` MUST be under 200 characters."#;

/// L2 — 四维评分: novelty / consistency / groundedness / actionability.
///
/// Placeholders:
/// - `{{hypothesis}}` — the imagination hypothesis.
/// - `{{evidence_summary}}` — newline-separated evidence_refs summary.
pub const TIER3_IMAGINATION_L2_FOUR_DIMENSIONS_PROMPT: &str = r#"You are the imagination four-dimension scoring agent. Score the given hypothesis on four orthogonal dimensions.

# Hypothesis

{{hypothesis}}

# Evidence summary

{{evidence_summary}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "novelty": <0.0..1.0>,
  "consistency": <0.0..1.0>,
  "groundedness": <0.0..1.0>,
  "actionability": <0.0..1.0>,
  "notes": "<one-line notes>"
}

Dimension definitions (strict):
- novelty: is the hypothesis a non-obvious insight? (0=trivial restatement, 1=novel synthesis)
- consistency: does the hypothesis cohere with the supplied evidence? (0=contradicts evidence, 1=fully coheres)
- groundedness: is the hypothesis grounded in concrete evidence? (0=pure speculation, 1=multiple evidence refs)
- actionability: can downstream agents act on this hypothesis? (0=untestable / abstract, 1=immediately actionable)

Rules:
- Each score MUST be a float in [0.0, 1.0].
- Score independently; do NOT average dimensions in your head — the consumer averages downstream.
- `notes` MUST be under 200 characters."#;

/// L3 — 原子验证: atomic claim verification.
///
/// Placeholders:
/// - `{{atom}}` — a single atomic statement extracted from the hypothesis.
/// - `{{evidence_refs}}` — the candidate evidence to validate against.
pub const TIER3_IMAGINATION_L3_ATOMIC_VERIFY_PROMPT: &str = r#"You are the imagination atomic verification agent. Decide whether a single atomic statement is supported by the given evidence.

# Atomic statement

{{atom}}

# Evidence to check against

{{evidence_refs}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "verdict": "supported" | "refuted" | "inconclusive",
  "confidence": <0.0..1.0>,
  "citing_evidence_ids": ["<id>", ...]
}

Verdict semantics (strict):
- "supported": at least one evidence ref affirmatively backs the statement.
- "refuted": at least one evidence ref directly contradicts the statement.
- "inconclusive": neither sufficient affirmation nor contradiction in evidence.

Rules:
- `confidence` is your certainty in the verdict, a float in [0.0, 1.0].
- `citing_evidence_ids` MUST be empty when verdict == "inconclusive".
- Do NOT hallucinate evidence not present in the input."#;

/// L4 — Weighted fusion formula (documented constant, used for inspection /
/// auditing — the implementation uses `L1_WEIGHT` / `L2_WEIGHT` / `L3_WEIGHT`
/// directly, not this string).
pub const TIER3_IMAGINATION_L4_FUSION_FORMULA: &str =
    "final_confidence = L1 * 0.3 + L2.avg * 0.4 + L3 * 0.3";

/// L5 — Promotion threshold semantics (documented constant; the implementation
/// uses `PROMOTION_THRESHOLD` / `EXPIRE_THRESHOLD` directly).
pub const TIER3_IMAGINATION_L5_PROMOTION_RULES: &str = r#"
- final_confidence >= 0.7 → review queue (confidence: high, status: pending-review)
- 0.5 <= final_confidence < 0.7 → review queue (confidence: medium, status: pending-review)
- final_confidence < 0.5 → expire (no write, log only)
"#;

/// Stage-0 — Hypothesis self-generation (W-MEMORY-EVOLUTION PR-6, the vision
/// core). This is what makes imagination "self-evolving" rather than
/// "score-whatever-you-are-fed": before the 5-layer confidence pipeline runs,
/// the orchestrator synthesizes its OWN candidate hypotheses by reflecting over
/// the existing memory corpus (reflections + dreams + recent session).
///
/// Placeholders:
/// - `{{reflections}}` — newline-separated reflection memory files
///   (`<memory_dir>/*.md`, excluding MEMORY.md / SESSION.md / .session-*).
/// - `{{dreams}}` — newline-separated dream insights
///   (`<memory_dir>/dreams/insight_*.md`, insight preferred over fragment).
/// - `{{recent_session}}` — recent session content (`SESSION.md` + optional
///   recent transcript tail).
pub const TIER3_IMAGINATION_HYPOTHESIS_GEN_PROMPT: &str = r#"You are the imagination hypothesis-generation agent. Your job is to synthesize NOVEL, non-obvious candidate hypotheses by reflecting over the system's existing memory — its reflections, its dreams, and its recent session activity. These hypotheses are not assertions of fact: they are speculative insights that downstream layers will independently verify against evidence. Your value is in connecting dots ACROSS the three sources that no single source states outright.

# Reflections (consolidated memory notes)

{{reflections}}

# Dreams (prior insights)

{{dreams}}

# Recent session activity

{{recent_session}}

# Previously refuted hypotheses (negative knowledge)

{{refuted}}

# Recent sweep meta-review

{{meta_review}}

# Output format

Return ONE JSON object (no surrounding markdown) of the form:

{
  "hypotheses": [
    {
      "statement": "<one concise speculative insight, plain text>",
      "atoms": ["<atomic claim 1>", "<atomic claim 2>"],
      "evidence_refs": [
        { "id": "<source id, e.g. a memory filename or session marker>", "snippet": "<verbatim quote under 200 chars supporting an atom>" }
      ]
    }
  ]
}

Rules:
- Synthesize NEW insights by combining signals across reflections, dreams, and session — do NOT merely restate any single existing note.
- Each hypothesis MUST be falsifiable: phrase it so downstream evidence checking can support or refute it.
- `atoms` decompose the statement into independently checkable claims (1-4 atoms).
- `evidence_refs` cite the SOURCE text you drew from (verbatim snippets from the inputs above). Cite honestly; do NOT fabricate quotes that are not present in the inputs. An empty list is acceptable when the hypothesis is purely speculative.
- Emit 1-5 hypotheses. Prefer fewer, higher-quality, genuinely novel hypotheses over many trivial ones.
- NEVER regenerate a hypothesis from the refuted list above (or a trivial rephrasing of one) — those already failed verification; negative knowledge is final until new evidence appears in the inputs.
- Use the meta-review section to calibrate: if recent sweeps expired most candidates, generate fewer and more conservative hypotheses this round.
- If the inputs are empty or too thin to synthesize anything novel, return {"hypotheses": []}."#;

/// Stage-0 synthesis input caps (fail-soft bounds, not hard contracts).
/// Limit the number of reflection / dream files and per-file char length so
/// the synthesis prompt stays bounded regardless of corpus size.
pub const HYPGEN_MAX_REFLECTION_FILES: usize = 24;
pub const HYPGEN_MAX_DREAM_FILES: usize = 24;
pub const HYPGEN_MAX_FILE_CHARS: usize = 4_000;
pub const HYPGEN_MAX_SESSION_CHARS: usize = 8_000;
pub const HYPGEN_MAX_TRANSCRIPT_CHARS: usize = 8_000;
/// Cap on number of candidate hypotheses fanned into the L1-L5 pipeline, even
/// if the LLM emits more (defense against runaway fan-out).
pub const HYPGEN_MAX_CANDIDATES: usize = 5;

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-SYNERGY W6 (2026-07-16) — 6d-1 负知识库 + 6d-2 元评审环
// ──────────────────────────────────────────────────────────────────────────

/// 6d-1 — 低置信淘汰假设的负知识库目录（`<memory_dir>/imagination/refuted/`）。
/// 淘汰 ≠ 遗忘：被证伪/低置信的假设是防止 Stage-0 下轮重想同一个的负知识
/// （AI Co-Scientist / TruthHypo 先例：refuted 也入库）。整个 `imagination/`
/// 子树被 SE 索引排除（K2），负知识绝不会污染检索召回。滚动上限，最旧先出。
pub const REFUTED_DIRNAME: &str = "refuted";
pub const REFUTED_MAX_FILES: usize = 50;
/// Stage-0 prompt 注入的负知识条数上限（最新优先）。
pub const HYPGEN_MAX_REFUTED_LINES: usize = 20;

/// 6d-2 — 每轮 sweep 的元评审账本（`<memory_dir>/imagination/meta-review.jsonl`，
/// 非 .md → 天然不入索引）。Stage-0 注入最近 `HYPGEN_META_REVIEW_TAIL` 行，
/// 让下一轮生成按「上几轮通过率」自校准（Co-Scientist meta-review 环的
/// 确定性缩微版：统计由代码算，不烧 LLM）。
pub const META_REVIEW_FILENAME: &str = "meta-review.jsonl";
pub const META_REVIEW_MAX_LINES: usize = 100;
pub const HYPGEN_META_REVIEW_TAIL: usize = 3;

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-SELF-EVOLUTION B2 (2026-06-11) — evolution report
// ──────────────────────────────────────────────────────────────────────────

/// System prompt for the periodic evolution report. Lives here as a Rust
/// constant per CLAUDE.md §硬约束 #15 第 8 条 (Tier prompt templates are
/// orchestrator-embedded, never part of the TS system-prompt体系). The
/// report is a USER-facing markdown document, so the prompt mandates 中文
/// output and a fixed section structure the TUI can rely on.
pub const EVOLUTION_REPORT_SYSTEM_PROMPT: &str = r#"You are the memory system's evolution-report writer. You receive the user's distilled memory corpus: reflection notes, dream insights, imagination proposals (with confidence and external evidence), and the PREVIOUS evolution report (possibly empty).

Write a concise evolution report in Simplified Chinese, in Markdown, with EXACTLY these five sections (use these headings verbatim):

## 用户习惯归纳
Summarize stable user habits/preferences observable in the corpus. Cite which note each habit comes from (by file label). No speculation.

## 当前问题清单
Concrete recurring problems / friction points found in the corpus. Each item: one line problem + one line evidence.

## 外部佐证
Only list claims that carry external evidence (source URL + retrieved-at timestamp from imagination proposals). Quote the source honestly. If none, write "本期无外部取证。".

## 整改与自我进化建议
Actionable suggestions derived from the above. Mark each as [立即可做] or [需要用户确认]. Suggestions that would修改记忆内容 must always be [需要用户确认].

## 上期建议采纳回顾
Compare the PREVIOUS report's suggestions against the current corpus: adopted / partially adopted / not adopted / no longer relevant. If there is no previous report, write "首期报告，无上期可回顾。".

Rules:
- Ground every statement in the supplied corpus; do NOT invent facts, habits, or sources.
- Keep the whole report under ~120 lines.
- Do not include any YAML frontmatter; the caller adds it."#;

/// W-MEMORY-SYNERGY W3 (2026-07-16, RC-5) — the English structural twin of
/// `EVOLUTION_REPORT_SYSTEM_PROMPT`. Section headings are the TUI-visible
/// report STRUCTURE, so they must be template-selected in Rust (契约 #15-8)
/// rather than left to the TS-side prose-language directive (which owns
/// running text only). Selection = `crate::output_language`.
pub const EVOLUTION_REPORT_SYSTEM_PROMPT_EN: &str = r#"You are the memory system's evolution-report writer. You receive the user's distilled memory corpus: reflection notes, dream insights, imagination proposals (with confidence and external evidence), and the PREVIOUS evolution report (possibly empty).

Write a concise evolution report in English, in Markdown, with EXACTLY these five sections (use these headings verbatim):

## User Habit Digest
Summarize stable user habits/preferences observable in the corpus. Cite which note each habit comes from (by file label). No speculation.

## Current Issues
Concrete recurring problems / friction points found in the corpus. Each item: one line problem + one line evidence.

## External Evidence
Only list claims that carry external evidence (source URL + retrieved-at timestamp from imagination proposals). Quote the source honestly. If none, write "No external evidence this period.".

## Remediation & Self-Evolution Suggestions
Actionable suggestions derived from the above. Mark each as [ready-now] or [needs-user-confirmation]. Suggestions that would modify memory content must always be [needs-user-confirmation].

## Previous-Report Follow-up
Compare the PREVIOUS report's suggestions against the current corpus: adopted / partially adopted / not adopted / no longer relevant. If there is no previous report, write "First report; nothing to review.".

Rules:
- Ground every statement in the supplied corpus; do NOT invent facts, habits, or sources.
- Keep the whole report under ~120 lines.
- Do not include any YAML frontmatter; the caller adds it."#;

/// Caps for the report corpus (fail-soft bounds).
pub const REPORT_MAX_QUEUE_FILES: usize = 12;
pub const REPORT_MAX_PREV_CHARS: usize = 6_000;

// ──────────────────────────────────────────────────────────────────────────
// Gate input / output
// ──────────────────────────────────────────────────────────────────────────

/// Imagination gate input. Unlike `AutoDreamGate` (4 hard gates), the
/// imagination gate is **always-trigger** by design — imagination is invoked
/// explicitly by callers (P5.3 UI or future KAIROS) with a concrete
/// hypothesis. The gate exists for trait-shape symmetry with Tier-1 /
/// Tier-2 / Tier-3 dream + future feature-flag / kill-switch hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationGateInput {
    /// Absolute path to memory_dir (where `imagination/review-queue/` lives).
    pub memory_dir: PathBuf,
    /// Optional feature flag — when `false`, the gate skips with reason
    /// `"feature_disabled"`. Default `true` (always-trigger).
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Imagination gate output — propagated to `ImaginationProcessor::process`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationGateOutput {
    /// Resolved review queue directory (`memory_dir/imagination/review-queue`).
    pub review_queue_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ImaginationGateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ──────────────────────────────────────────────────────────────────────────
// ImaginationGate (TierGate trait impl)
// ──────────────────────────────────────────────────────────────────────────

/// Imagination gate — always-trigger (subject to feature flag). No lock,
/// no scan throttle (imagination is per-hypothesis, not per-tick).
#[derive(Debug, Default)]
pub struct ImaginationGate;

impl ImaginationGate {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TierGate for ImaginationGate {
    type GateInput = ImaginationGateInput;
    type GateOutput = ImaginationGateOutput;
    type Error = ImaginationGateError;

    async fn evaluate_gate(
        &self,
        input: Self::GateInput,
    ) -> Result<GateDecision<Self::GateOutput>, Self::Error> {
        if !input.enabled {
            return Ok(GateDecision {
                should_trigger: false,
                payload: None,
                skip_reason: Some("feature_disabled".to_string()),
            });
        }
        let review_queue_dir = input.memory_dir.join("imagination").join("review-queue");
        Ok(GateDecision {
            should_trigger: true,
            payload: Some(ImaginationGateOutput { review_queue_dir }),
            skip_reason: None,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Process input / output
// ──────────────────────────────────────────────────────────────────────────

/// One imagination hypothesis under evaluation. Contains the hypothesis
/// statement, candidate evidence refs, and the atomic claims that the L3
/// verifier should check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationHypothesis {
    /// The hypothesis statement (single line / paragraph, plain text).
    pub statement: String,
    /// Atomic claims extracted from the hypothesis (caller does extraction;
    /// orchestrator validates each atom independently in L3).
    pub atoms: Vec<String>,
    /// Candidate evidence refs (source id + snippet). Same shape as
    /// `tier3_auto_dream::Phase2Evidence`.
    pub evidence_refs: Vec<ImaginationEvidenceRef>,
    /// Optional supporting context (free-form summary, not evidence) — used
    /// in L1 Self-RAG to orient the model but explicitly excluded from L3
    /// citation.
    #[serde(default)]
    pub context: String,
}

/// Evidence ref tuple. `source` mirrors P3.4 Phase2Evidence semantics
/// (session id / memory file path / dream insight id). `snippet` is the
/// verbatim quote (under 200 chars by convention).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationEvidenceRef {
    /// Stable evidence id (e.g. `"sess-2026-05-25-T1"` or
    /// `"memory/user_topic.md"`). Used in L3 `citing_evidence_ids`.
    pub id: String,
    /// Verbatim snippet (caller-provided; orchestrator does not paraphrase).
    pub snippet: String,
}

/// Imagination process input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationProcessInput {
    /// Absolute path to memory_dir.
    pub memory_dir: PathBuf,
    /// Gate payload from preceding `evaluate_gate`.
    pub gate_payload: ImaginationGateOutput,
    /// The hypothesis under evaluation.
    pub hypothesis: ImaginationHypothesis,
    /// Optional model hint (TS side decides actual model selection; not a
    /// brand literal — §硬约束 #1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    /// Sampling parameters (None = TS uses SDK defaults).
    #[serde(default)]
    pub params: LlmCallParams,
    /// K10 — optional watch-scoped evidence context; threaded into
    /// `gather_evidence` (read-only `readFile` / `listDir` probes). `None`
    /// (the default, incl. every pre-K10 serialized payload) keeps behavior
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_context: Option<WatchContext>,
}

/// Self-generation (Stage-0) input — no `hypothesis` field; the processor
/// synthesizes its own candidates from the memory corpus, then runs the
/// L1-L5 pipeline on each. Mirrors `ImaginationProcessInput` minus the
/// caller-supplied hypothesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationGeneratedInput {
    /// Absolute path to memory_dir (synthesis corpus + write target root).
    pub memory_dir: PathBuf,
    /// Gate payload from preceding `evaluate_gate`.
    pub gate_payload: ImaginationGateOutput,
    /// Optional model hint (TS side decides actual selection; not a brand
    /// literal — §硬约束 #1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    /// Sampling parameters (None = TS uses SDK defaults).
    #[serde(default)]
    pub params: LlmCallParams,
    /// K10 — optional watch-scoped evidence context, cloned into every
    /// per-candidate `ImaginationProcessInput` (see that field's doc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_context: Option<WatchContext>,
}

/// Result of Stage-0 self-generation (before the L1-L5 pipeline).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct GeneratedHypotheses {
    /// Candidate hypotheses synthesized from the memory corpus (already
    /// fail-soft parsed; may be empty).
    pub hypotheses: Vec<ImaginationHypothesis>,
    /// `req_id` of the single generation LLM round-trip.
    pub req_id: String,
    /// Token usage of the generation call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_usage: Option<LlmUsageWire>,
    /// 8c (W-MEMORY-HYPGEN-VARIANT-WIRE 2026-07-16) — the hypgen prompt
    /// variant selected (UCB1) for this generation. All candidates in this
    /// batch were produced under it; `process_generated` attributes their L5
    /// verdicts back to it. `hypgen/v0` = empty-addendum baseline.
    #[serde(default)]
    pub variant_id: String,
}

/// Output of `process_generated`: the generation metadata plus one pipeline
/// output per candidate hypothesis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationGeneratedOutput {
    /// `req_id` of the Stage-0 generation call.
    pub generation_req_id: String,
    /// Token usage of the Stage-0 generation call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_usage: Option<LlmUsageWire>,
    /// One L1-L5 pipeline output per generated candidate (empty when none
    /// were generated).
    pub outputs: Vec<ImaginationProcessOutput>,
}

/// Final promotion verdict for one hypothesis.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromotionVerdict {
    /// final_confidence ≥ PROMOTION_THRESHOLD (0.7); written with
    /// confidence: high.
    ReviewQueueHigh,
    /// EXPIRE_THRESHOLD (0.5) ≤ final_confidence < PROMOTION_THRESHOLD;
    /// written with confidence: medium.
    Pending,
    /// final_confidence < EXPIRE_THRESHOLD (0.5); not written, log only.
    Expired,
}

/// 8c (W-MEMORY-HYPGEN-VARIANT-WIRE 2026-07-16) — hypgen 变体判据映射（创建时
/// verdict → 胜/负/中性）：`ReviewQueueHigh` = 胜（`Some(true)`）、`Expired` =
/// 负（`Some(false)`）、`Pending` = 中性不记（`None`）。想象按 L1–L5 置信管线
/// 的创建时 verdict 判胜、**不**走磁盘存活（理由见 `evolution::variants` 模块头
/// 「hypgen 想象」段：expiry=存活窗口、Expired 不写盘、无到期清除使存活成废
/// 信号）。genuine Expired（模型验过、置信不足）记负是想要的信号——无据抬杠
/// 的变体理应被罚。
///
/// 已知边界（有意接受、非遗漏）：D3「全层解析失败」的零信号 Expired（融合恰
/// 落中性带、`all_layers_fell_back` 布尔驱动）这里也计一负。该布尔不在
/// `ImaginationProcessOutput`（P5.3 UI 消费的跨语言 wire 结构）上，而融合值有
/// 浮点毛刺（≈0.4999）无法在输出层可靠区分；为一个**罕见**且**与变体无关**
/// （L1–L3 验证 prompt 不随 hypgen 变体变，故此类失败不偏置任何变体）的
/// fail-soft 噪声去改 wire 契约不成比例。均匀噪声不腐蚀 UCB1 相对排序，与
/// archive fail-soft 竞态同一容忍口径。
#[must_use]
fn verdict_outcome(verdict: PromotionVerdict) -> Option<bool> {
    match verdict {
        PromotionVerdict::ReviewQueueHigh => Some(true),
        PromotionVerdict::Expired => Some(false),
        PromotionVerdict::Pending => None,
    }
}

/// Imagination process output. The exact wire shape consumed by callers
/// (P5.3 UI / tests). Layer scores exposed for UI rendering + audit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ImaginationProcessOutput {
    /// L1 Self-RAG plausibility score (0.0-1.0).
    pub l1_plausibility: f64,
    /// L2 four-dimension scores.
    pub l2_scores: FourDimensionScores,
    /// L3 atomic verification aggregate (per-atom verdicts collapsed to
    /// 0.0-1.0 confidence weighted by atom count).
    pub l3_atomic_aggregate: f64,
    /// Per-atom verdicts (for UI + audit).
    pub l3_atom_verdicts: Vec<AtomVerdict>,
    /// Final fused confidence (L1*0.3 + L2.avg*0.4 + L3*0.3).
    pub final_confidence: f64,
    /// Promotion verdict (review queue / pending / expired).
    pub verdict: PromotionVerdict,
    /// Path of the imagined file written; `None` when verdict == Expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imagined_path: Option<PathBuf>,
    /// `req_id`s issued during the run (one per LLM round-trip).
    pub req_ids: Vec<String>,
    /// Aggregate LLM token usage across all layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_usage: Option<LlmUsageWire>,
}

/// Mirror of `LlmUsage` — kept independent so the public wire shape doesn't
/// accidentally couple Processor's output to its input. Mirrors P3.4
/// `tier3_auto_dream::LlmUsageWire`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct LlmUsageWire {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// L2 four-dimension scores.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct FourDimensionScores {
    pub novelty: f64,
    pub consistency: f64,
    pub groundedness: f64,
    pub actionability: f64,
}

impl FourDimensionScores {
    /// Arithmetic mean over the 4 dimensions. Used in the L4 fusion formula.
    pub fn avg(&self) -> f64 {
        (self.novelty + self.consistency + self.groundedness + self.actionability) / 4.0
    }
}

/// Per-atom verdict (one entry per atomic claim).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AtomVerdict {
    pub atom: String,
    pub verdict: AtomVerdictKind,
    pub confidence: f64,
    pub citing_evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AtomVerdictKind {
    Supported,
    Refuted,
    Inconclusive,
}

#[derive(Debug, thiserror::Error)]
pub enum ImaginationProcessError {
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

// ──────────────────────────────────────────────────────────────────────────
// LLM emitter trait (mirrors Tier-1/Tier-2/Tier-3 dream pattern)
// ──────────────────────────────────────────────────────────────────────────

/// LLM emitter — abstracted so tests inject in-memory mocks. Mirrors the
/// per-tier emitter trait shape so production wiring can install a single
/// broadcast emitter satisfying all four Tier processors (1 / 2 / 3-dream /
/// 3-imagination) via `Arc<dyn LlmCallEmitter>`.
#[async_trait]
pub trait LlmCallEmitter: Send + Sync {
    async fn emit_request(&self, request: LlmCallRequestPayload);
}

/// In-memory recording emitter (mirrors P3.4 `RecordingEmitter`).
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

/// Scripted emitter — pre-recorded results keyed by request prefix.
/// Lets tests drive a deterministic 5-layer round-trip without manual
/// `deliver_result` calls. Mirrors P3.2/P3.3/P3.4 test infrastructure
/// pattern.
pub struct ScriptedEmitter {
    /// Inner recorder.
    recorder: RecordingEmitter,
    /// Pending oneshot map — set by `bind_processor`.
    processor: Mutex<Option<Arc<ImaginationProcessor>>>,
    /// Per-layer scripted JSON responses (keyed by layer name, e.g. `"l1"`).
    /// Each layer can carry multiple responses (FIFO for L3 multi-atom).
    scripts: Mutex<HashMap<String, Vec<String>>>,
    /// W-MEMORY-EVOLUTION PR-8 — scripted external evidence delivered for
    /// `gather_evidence` tool requests. Empty (default) → tool requests resolve
    /// to an empty evidence vec (fail-soft fallback to internal refs), keeping
    /// pre-PR-8 process tests behaviorally identical.
    tool_evidence: Mutex<Vec<ToolEvidence>>,
}

impl ScriptedEmitter {
    pub fn new() -> Self {
        Self {
            recorder: RecordingEmitter::new(),
            processor: Mutex::new(None),
            scripts: Mutex::new(HashMap::new()),
            tool_evidence: Mutex::new(Vec::new()),
        }
    }

    /// Set scripted JSON responses for a given layer (`"l1"` / `"l2"` /
    /// `"l3"`). Multiple per layer FIFO.
    pub async fn set_scripts(&self, layer: &str, responses: Vec<String>) {
        self.scripts
            .lock()
            .await
            .insert(layer.to_string(), responses);
    }

    /// W-MEMORY-EVOLUTION PR-8 — set the external evidence the scripted emitter
    /// delivers in response to `gather_evidence` tool requests during a
    /// `process` round-trip.
    pub async fn set_tool_evidence(&self, evidence: Vec<ToolEvidence>) {
        *self.tool_evidence.lock().await = evidence;
    }

    pub async fn bind_processor(&self, processor: Arc<ImaginationProcessor>) {
        *self.processor.lock().await = Some(processor);
    }

    /// Recorded request payloads (delegates to the inner `RecordingEmitter`).
    pub async fn recorded(&self) -> Vec<LlmCallRequestPayload> {
        self.recorder.recorded().await
    }
}

impl Default for ScriptedEmitter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmCallEmitter for ScriptedEmitter {
    async fn emit_request(&self, request: LlmCallRequestPayload) {
        self.recorder.emit_request(request.clone()).await;
        // Pull a scripted response keyed by phase (e.g. "l1", "l2", "l3").
        let phase = request.phase.clone().unwrap_or_default();
        let response = {
            let mut scripts = self.scripts.lock().await;
            scripts.get_mut(&phase).and_then(|q| {
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            })
        };
        let processor = self.processor.lock().await.clone();
        if let Some(processor) = processor {
            let result = LlmCallResultPayload {
                req_id: request.req_id,
                response: response.or_else(|| Some("{}".to_string())),
                usage: Some(LlmUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                error: None,
            };
            processor.deliver_result(result).await;
        }
    }
}

/// W-MEMORY-EVOLUTION PR-8 — `ScriptedEmitter` also satisfies `ToolCallEmitter`
/// so `process` round-trip tests can drive the `gather_evidence` reverse-IPC
/// channel deterministically (auto-deliver the scripted `tool_evidence`,
/// defaulting to empty = fail-soft fallback to internal refs). Mirrors the
/// `LlmCallEmitter` auto-deliver pattern.
#[async_trait]
impl ToolCallEmitter for ScriptedEmitter {
    async fn emit_request(&self, request: ToolCallRequestPayload) {
        let evidence = self.tool_evidence.lock().await.clone();
        let processor = self.processor.lock().await.clone();
        if let Some(processor) = processor {
            let result = ToolCallResultPayload {
                req_id: request.req_id,
                evidence,
                error: None,
            };
            processor.deliver_tool_result(result).await;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tool evidence-gathering channel (W-MEMORY-EVOLUTION PR-7b)
//
// Imagination's "可靠最新 + 可追溯" promise: before the L1-L5 confidence pipeline
// can ground a hypothesis, it needs FRESH external evidence. The orchestrator
// emits a `memory/tier/toolCallRequest` reverse-IPC frame; the TS leader worker
// runs WebSearchTool / WebFetchTool and writes evidence (with `source_url` +
// `fetched_at_ms`) back via `memory.tier.tool_call_result`. Mirrors the LLM /
// embedding reverse-IPC contract (independent method, same pending-map shape).
//
// PR-7a landed the protocol + TUI client plumbing; PR-7b landed the orchestrator
// emit capability (`gather_evidence`) + result delivery; PR-8 wired
// `gather_evidence` into the production `process()` path (called at the L1→L2
// boundary) so L2/L3 score against real fetched evidence with traceable
// `source_url` + `fetched_at_ms`.
// ──────────────────────────────────────────────────────────────────────────

/// One tool call request (web search / web fetch / watch-scoped file probe).
/// Wire-aligned with protocol `MemoryTierToolCall` (`kind` camelCase:
/// `"webSearch"` / `"webFetch"` / `"readFile"` / `"listDir"`).
///
/// K10 (W-MEMORY-LIFECYCLE 2026-07-09): the watch-scoped kinds carry
/// `{id, kind, path, root}`; the pre-existing `webSearch` / `webFetch` entry
/// shapes are untouched (the new fields are `None` → skipped on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Tool kind (`"webSearch"` requires `query`; `"webFetch"` requires `url`;
    /// `"readFile"` / `"listDir"` require `id` + `path` + `root`).
    pub kind: ToolKind,
    /// Search query (`WebSearch` only; `None` otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Target URL (`WebFetch` only; `None` otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// K10 watch kinds only: per-call id (executor-side correlation /
    /// diagnostics; results come back as uniform `evidence` items keyed by
    /// `source_url` = the probed path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// K10 watch kinds only: ABSOLUTE target path (file for `readFile`,
    /// directory for `listDir`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// K10 watch kinds only: the watch root the `path` must stay inside. The
    /// TS executor re-validates canonically (defense in depth — SoT K10 hard
    /// limits: root-confined, ≤64KiB, plain text, ≤8 calls per run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// Tool kind discriminant (wire: camelCase, matches protocol
/// `MemoryTierToolKind`). K10 adds the two READ-ONLY watch-scoped kinds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum ToolKind {
    WebSearch,
    WebFetch,
    /// K10 — read one file inside the watch root (result: `{content}`).
    ReadFile,
    /// K10 — list one directory inside the watch root (result:
    /// `{entries: [string]}`).
    ListDir,
}

/// `memory/tier/toolCallRequest` notification payload (orchestrator → TS).
/// Mirrors protocol `MemoryTierToolCallRequestNotification`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolCallRequestPayload {
    /// Correlates request/response in the tool pending map.
    pub req_id: String,
    /// Triggering Tier (always `Dream` — imagination is Tier-3).
    pub tier: MemoryTier,
    /// Batch of tool calls (each executed independently TS-side).
    pub calls: Vec<ToolCall>,
}

/// One gathered evidence item (reliable + traceable). Mirrors protocol
/// `MemoryTierToolEvidence`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolEvidence {
    /// Source URL (traceability): search hit URL or fetched target URL.
    pub source_url: String,
    /// Fetch/retrieval timestamp (ms wall clock). Stamped by the TS proxy at
    /// execution time (the tools carry no timestamp — CLAUDE.md §15 D7).
    pub fetched_at_ms: u64,
    /// Evidence body (search snippet / processed fetch text).
    pub content: String,
    /// Optional title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `memory.tier.tool_call_result` IPC payload (TS → orchestrator).
/// Mirrors protocol `MemoryTierToolCallResultParams`.
///
/// K10 (W-MEMORY-LIFECYCLE 2026-07-09): the watch-scoped `readFile` /
/// `listDir` results ride the SAME uniform `evidence` channel as the web
/// kinds (no separate per-call result array — S2's executor contract):
/// * `readFile` → `content` = the file text; oversized files carry a plain
///   marker like `[truncated: file is N bytes, read limit 65536]`.
/// * `listDir` → `content` = `JSON.stringify({entries[, truncated]})` — the
///   orchestrator decodes it into a readable listing before scoring (see
///   `decode_watch_listing_evidence`).
///
/// `source_url` is the probed path (traceability).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ToolCallResultPayload {
    /// Matches the originating `ToolCallRequestPayload.req_id`.
    pub req_id: String,
    /// Gathered evidence (empty `[]` on failure, paired with `error`; an empty
    /// list alone is NOT a failure — there may genuinely be no hits).
    #[serde(default)]
    pub evidence: Vec<ToolEvidence>,
    /// Failure message; `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// K10 (W-MEMORY-LIFECYCLE 2026-07-09) — watch-scoped evidence context for a
/// 专项检测 target. Present only when the imagination run is anchored to a
/// watch; `gather_evidence` then derives at most 1 `listDir(root)` + 2
/// `readFile` probes (paths mechanically extracted from the hypothesis
/// statement) and folds the results into the evidence text. `None` keeps the
/// pipeline byte-identical to pre-K10 behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct WatchContext {
    /// Watch target root (absolute). Every derived probe path stays inside it
    /// (and the TS executor re-validates canonically).
    pub root: PathBuf,
    /// Optional focus text from the watch config (diagnostic / prompt use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

/// Tool-call emitter — abstracted so tests inject in-memory mocks. Mirrors the
/// `LlmCallEmitter` trait shape so production wiring can install a single
/// broadcast emitter satisfying both the LLM and the tool reverse-IPC paths.
#[async_trait]
pub trait ToolCallEmitter: Send + Sync {
    async fn emit_request(&self, request: ToolCallRequestPayload);
}

/// In-memory recording tool-call emitter (mirrors `RecordingEmitter`). Records
/// emitted tool-call requests for test inspection.
#[derive(Debug, Default, Clone)]
pub struct RecordingToolEmitter {
    inner: Arc<Mutex<Vec<ToolCallRequestPayload>>>,
}

impl RecordingToolEmitter {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn recorded(&self) -> Vec<ToolCallRequestPayload> {
        self.inner.lock().await.clone()
    }
}

#[async_trait]
impl ToolCallEmitter for RecordingToolEmitter {
    async fn emit_request(&self, request: ToolCallRequestPayload) {
        self.inner.lock().await.push(request);
    }
}

/// Evidence-gathering reverse-IPC wait timeout (mirrors the LLM call timeout).
pub const TOOL_CALL_TIMEOUT_MS: u64 = 60_000;

/// Per-evidence content truncation when threading external `ToolEvidence` into
/// the L2/L3 scoring prompts. Keeps the prompt bounded regardless of fetched
/// page size (mirrors `HYPGEN_MAX_FILE_CHARS` bounding philosophy).
pub const EVIDENCE_CONTENT_MAX_CHARS: usize = 1_200;

// ──────────────────────────────────────────────────────────────────────────
// ImaginationProcessor
// ──────────────────────────────────────────────────────────────────────────

/// Tier-3 imagination processor — owns the gate, the pending oneshot map,
/// the emitter, and a counter for `req_id` generation.
pub struct ImaginationProcessor {
    gate: Arc<ImaginationGate>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<LlmCallResultPayload>>>>,
    emitter: Arc<dyn LlmCallEmitter>,
    req_id_counter: AtomicU64,
    /// W-MEMORY-EVOLUTION PR-7b — tool-call (evidence-gathering) pending map,
    /// keyed by `req_id` (prefix `tier3-imagination-evidence-`). Independent of
    /// `pending` (LLM map) so the two reverse-IPC channels never collide.
    tool_pending: Arc<Mutex<HashMap<String, oneshot::Sender<ToolCallResultPayload>>>>,
    /// W-MEMORY-EVOLUTION PR-7b — tool-call emitter (broadcast in production,
    /// `RecordingToolEmitter` under `new()`/`default()` + tests).
    tool_emitter: Arc<dyn ToolCallEmitter>,
}

impl ImaginationProcessor {
    pub fn new(gate: Arc<ImaginationGate>, emitter: Arc<dyn LlmCallEmitter>) -> Self {
        Self::with_tool_emitter(gate, emitter, Arc::new(RecordingToolEmitter::new()))
    }

    /// Construct with an explicit tool emitter. Production wiring passes the
    /// shared broadcast emitter; `new()` defaults to a `RecordingToolEmitter`.
    pub fn with_tool_emitter(
        gate: Arc<ImaginationGate>,
        emitter: Arc<dyn LlmCallEmitter>,
        tool_emitter: Arc<dyn ToolCallEmitter>,
    ) -> Self {
        Self {
            gate,
            pending: Arc::new(Mutex::new(HashMap::new())),
            emitter,
            req_id_counter: AtomicU64::new(0),
            tool_pending: Arc::new(Mutex::new(HashMap::new())),
            tool_emitter,
        }
    }

    pub fn gate(&self) -> &Arc<ImaginationGate> {
        &self.gate
    }

    /// Deliver a reverse IPC LLM call result. Unknown `req_id` → no-op
    /// (treated as late delivery after timeout or wrong-tier delivery).
    /// Returns `true` iff the `req_id` matched a pending entry.
    pub async fn deliver_result(&self, result: LlmCallResultPayload) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(sender) = map.remove(&result.req_id) {
            let _ = sender.send(result);
            true
        } else {
            false
        }
    }

    /// W-MEMORY-EVOLUTION PR-7b — deliver a reverse-IPC tool-call result.
    /// Unknown `req_id` → no-op (late delivery after timeout). Returns `true`
    /// iff the `req_id` matched a pending tool entry. Mirrors `deliver_result`.
    pub async fn deliver_tool_result(&self, result: ToolCallResultPayload) -> bool {
        let mut map = self.tool_pending.lock().await;
        if let Some(sender) = map.remove(&result.req_id) {
            let _ = sender.send(result);
            true
        } else {
            false
        }
    }

    /// Test-only: register a pending tool oneshot under an explicit `req_id`
    /// and return the receiver, so an out-of-band caller (e.g. the IPC handler
    /// round-trip test) can deliver a result via `deliver_tool_result` and
    /// observe the resolution without driving the full `gather_evidence` await.
    #[doc(hidden)]
    pub async fn _testonly_register_pending_tool(
        &self,
        req_id: &str,
    ) -> oneshot::Receiver<ToolCallResultPayload> {
        let (tx, rx) = oneshot::channel::<ToolCallResultPayload>();
        self.tool_pending
            .lock()
            .await
            .insert(req_id.to_string(), tx);
        rx
    }

    fn next_req_id(&self, layer: &str) -> String {
        let n = self.req_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("tier3-imagination-{layer}-{n}-{}", now_ms())
    }

    /// W-MEMORY-EVOLUTION PR-7b — gather fresh external evidence for a
    /// hypothesis via the tool reverse-IPC channel.
    ///
    /// Registers a pending oneshot keyed by a `tier3-imagination-evidence-*`
    /// `req_id`, emits a `ToolCallRequestPayload` (broadcast to the TS leader
    /// worker proxy, which runs WebSearchTool / WebFetchTool), and awaits the
    /// evidence write-back (`TOOL_CALL_TIMEOUT_MS`). Fail-soft: timeout /
    /// shutdown / empty `calls` all yield an empty `Vec` (never panics, never
    /// errors). The `hypothesis_statement` argument feeds the K10 probe
    /// derivation (and logging); the web queries are supplied explicitly by
    /// the caller.
    ///
    /// Called from the production `process()` path at the L1→L2 boundary
    /// (PR-8): the returned evidence is threaded into L2 groundedness /
    /// consistency and L3 atom verification, and its `source_url` +
    /// `fetched_at_ms` are written to the imagination frontmatter.
    ///
    /// K10 (W-MEMORY-LIFECYCLE 2026-07-09): when `watch` is `Some`, the batch
    /// additionally carries mechanically-derived READ-ONLY probes against the
    /// watched tree — at most 1 `listDir(root)` + 2 `readFile` (paths from
    /// filename-shaped tokens in the hypothesis statement; none found → only
    /// the listDir). Their results come back as uniform `evidence` items
    /// (source = the probed path; listDir JSON bodies are decoded into
    /// readable listings). `None` → byte-identical pre-K10 behavior.
    pub async fn gather_evidence(
        &self,
        hypothesis_statement: &str,
        queries: Vec<ToolCall>,
        watch: Option<&WatchContext>,
    ) -> Vec<ToolEvidence> {
        let watch_calls = watch
            .map(|ctx| derive_watch_calls(hypothesis_statement, ctx))
            .unwrap_or_default();
        let mut calls = queries;
        calls.extend(watch_calls.iter().cloned());
        if calls.is_empty() {
            return Vec::new();
        }
        let req_id = self.next_req_id("evidence");
        log::debug!(
            "gather_evidence: req_id={req_id} calls={} (watch probes={}) hypothesis={:.80}",
            calls.len(),
            watch_calls.len(),
            hypothesis_statement,
        );
        let (tx, rx) = oneshot::channel::<ToolCallResultPayload>();
        {
            let mut map = self.tool_pending.lock().await;
            map.insert(req_id.clone(), tx);
        }

        let request = ToolCallRequestPayload {
            req_id: req_id.clone(),
            tier: MemoryTier::Dream,
            calls,
        };
        self.tool_emitter.emit_request(request).await;

        match tokio::time::timeout(Duration::from_millis(TOOL_CALL_TIMEOUT_MS), rx).await {
            Ok(Ok(result)) => {
                if let Some(err) = result.error.as_ref() {
                    log::warn!("gather_evidence: req_id={req_id} returned error: {err}");
                }
                let mut evidence = result.evidence;
                if !watch_calls.is_empty() {
                    // K10: the executor encodes listDir results as a JSON
                    // `{entries[, truncated]}` body — decode into a readable
                    // listing so L2/L3 score against names, not JSON syntax.
                    // readFile results are already plain text (any
                    // `[truncated: …]` marker passes through verbatim).
                    decode_watch_listing_evidence(&mut evidence);
                }
                evidence
            }
            Ok(Err(_recv_err)) => {
                self.tool_pending.lock().await.remove(&req_id);
                log::warn!("gather_evidence: req_id={req_id} channel closed (shutdown)");
                Vec::new()
            }
            Err(_elapsed) => {
                self.tool_pending.lock().await.remove(&req_id);
                log::warn!(
                    "gather_evidence: req_id={req_id} timed out after {TOOL_CALL_TIMEOUT_MS}ms"
                );
                Vec::new()
            }
        }
    }

    /// Single round-trip helper: register a pending oneshot, emit the
    /// request, await the result (timeout), validate.
    async fn call_llm(
        &self,
        layer: &str,
        messages: Vec<LlmMessage>,
        model_hint: Option<String>,
        params: LlmCallParams,
    ) -> Result<(String, String, Option<LlmUsage>), ImaginationProcessError> {
        let req_id = self.next_req_id(layer);
        let (tx, rx) = oneshot::channel::<LlmCallResultPayload>();
        {
            let mut map = self.pending.lock().await;
            map.insert(req_id.clone(), tx);
        }

        let request_payload = LlmCallRequestPayload {
            req_id: req_id.clone(),
            tier: MemoryTier::Dream,
            phase: Some(layer.to_string()),
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
                    return Err(ImaginationProcessError::Shutdown);
                }
                Err(_elapsed) => {
                    self.pending.lock().await.remove(&req_id);
                    return Err(ImaginationProcessError::Timeout(LLM_CALL_TIMEOUT_MS));
                }
            };

        if let Some(err) = result.error.as_ref() {
            return Err(ImaginationProcessError::LlmFailure(err.clone()));
        }
        let response = result
            .response
            .ok_or_else(|| ImaginationProcessError::LlmFailure("empty response".to_string()))?;

        Ok((req_id, response, result.usage))
    }

    /// Run one full Tier-3 imagination round: L1 → L2 → L3 (per atom) →
    /// L4 fuse → L5 verdict + (optionally) write.
    pub async fn process(
        &self,
        input: ImaginationProcessInput,
    ) -> Result<ImaginationProcessOutput, ImaginationProcessError> {
        let review_queue_dir = input.gate_payload.review_queue_dir.clone();
        tokio::fs::create_dir_all(&review_queue_dir).await?;

        let mut req_ids: Vec<String> = Vec::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;

        // ── L1: Self-RAG ──
        let l1_messages = build_l1_messages(&input.hypothesis);
        let (l1_req, l1_response, l1_usage) = self
            .call_llm(
                "l1",
                l1_messages,
                input.model_hint.clone(),
                input.params.clone(),
            )
            .await?;
        req_ids.push(l1_req);
        accumulate_usage(l1_usage, &mut input_tokens, &mut output_tokens);
        // D3 (W-MEMORY-LIFECYCLE 2026-07-09): track whether each layer's LLM
        // response REALLY parsed or fell back to its neutral default. Neutral
        // fallbacks are individually harmless, but when ALL layers fall back
        // the fused score is pure scaffolding (exactly 0.5 → Pending band)
        // with zero model signal behind it — see the verdict override below.
        let l1_parsed = parse_l1_json(&l1_response);
        let l1_plausibility = l1_parsed.unwrap_or(0.5);

        // ── Evidence gathering (W-MEMORY-EVOLUTION PR-8) ──
        // Fetch FRESH external evidence before L2/L3 scoring so groundedness /
        // consistency / atomic verification grade against real sources rather
        // than only the hypothesis's self-supplied (internal) evidence_refs.
        // Minimal query derivation: one web_search keyed on the hypothesis
        // statement. fail-soft: no tools / timeout / shutdown → empty vec →
        // the scoring layers transparently fall back to the internal refs.
        let external_evidence = self
            .gather_evidence(
                &input.hypothesis.statement,
                derive_evidence_queries(&input.hypothesis),
                input.watch_context.as_ref(),
            )
            .await;
        // External evidence projected into evidence-ref shape (id=source_url,
        // snippet=truncated content) for L3 reuse. Empty when no external hits.
        let external_refs = external_evidence_to_refs(&external_evidence);

        // ── L2: Four-dimension scoring ──
        let l2_messages = build_l2_messages_with_evidence(&input.hypothesis, &external_evidence);
        let (l2_req, l2_response, l2_usage) = self
            .call_llm(
                "l2",
                l2_messages,
                input.model_hint.clone(),
                input.params.clone(),
            )
            .await?;
        req_ids.push(l2_req);
        accumulate_usage(l2_usage, &mut input_tokens, &mut output_tokens);
        let l2_parsed = parse_l2_json(&l2_response);
        let l2_scores = l2_parsed
            .clone()
            .unwrap_or_else(FourDimensionScores::neutral);

        // ── L3: Atomic verification (per atom) ──
        // Verify each atom against external evidence when available; fall back
        // to the hypothesis's internal evidence_refs when no external hits
        // (preserves the pre-PR-8 behavior — no regression).
        let l3_evidence_refs: &[ImaginationEvidenceRef] = if external_refs.is_empty() {
            &input.hypothesis.evidence_refs
        } else {
            &external_refs
        };
        let mut atom_verdicts: Vec<AtomVerdict> = Vec::new();
        // D3 — the L3 layer counts as "really parsed" when at least one atom
        // verdict parsed; an empty atoms list issues no L3 call at all (the
        // 0.5 aggregate is then a pure default, i.e. a fallback).
        let mut l3_any_parsed = false;
        for atom in &input.hypothesis.atoms {
            let l3_messages = build_l3_messages(atom, l3_evidence_refs);
            let (l3_req, l3_response, l3_usage) = self
                .call_llm(
                    "l3",
                    l3_messages,
                    input.model_hint.clone(),
                    input.params.clone(),
                )
                .await?;
            req_ids.push(l3_req);
            accumulate_usage(l3_usage, &mut input_tokens, &mut output_tokens);
            let parsed = match parse_l3_json(&l3_response) {
                Some(parsed) => {
                    l3_any_parsed = true;
                    parsed
                }
                None => AtomVerdict {
                    atom: atom.clone(),
                    verdict: AtomVerdictKind::Inconclusive,
                    confidence: 0.0,
                    citing_evidence_ids: Vec::new(),
                },
            };
            atom_verdicts.push(AtomVerdict {
                atom: atom.clone(),
                ..parsed
            });
        }
        let l3_atomic_aggregate = aggregate_l3(&atom_verdicts);

        // ── L4: Weighted fusion ──
        let final_confidence = l1_plausibility * L1_WEIGHT
            + l2_scores.avg() * L2_WEIGHT
            + l3_atomic_aggregate * L3_WEIGHT;
        let final_confidence = final_confidence.clamp(0.0, 1.0);

        // ── L5: Promotion verdict ──
        // D3 (W-MEMORY-LIFECYCLE 2026-07-09): when EVERY layer fell back to its
        // neutral value, the fused 0.5 lands exactly in the Pending band and a
        // fully-unscored hypothesis used to persist as a medium-confidence
        // review-queue entry. Zero model signal → Expired (no write); the drop
        // is observable via a daily-log `all_layers_parse_failed` record.
        let all_layers_fell_back = l1_parsed.is_none() && l2_parsed.is_none() && !l3_any_parsed;
        let verdict = if all_layers_fell_back {
            log::warn!(
                "tier3 imagination: all confidence layers failed to parse — expiring \
                 hypothesis without persisting (fail-soft)"
            );
            record_all_layers_parse_failed(&input.memory_dir, &input.hypothesis.statement).await;
            PromotionVerdict::Expired
        } else if final_confidence >= PROMOTION_THRESHOLD {
            PromotionVerdict::ReviewQueueHigh
        } else if final_confidence >= EXPIRE_THRESHOLD {
            PromotionVerdict::Pending
        } else {
            PromotionVerdict::Expired
        };

        let imagined_path = match verdict {
            PromotionVerdict::Expired => {
                // W6 6d-1 — 低置信淘汰入负知识库（防 Stage-0 重想）。全层
                // 解析失败的零信号淘汰除外：没有可记的判定，不算负知识。
                if !all_layers_fell_back {
                    write_refuted_hypothesis(
                        &input.memory_dir,
                        &input.hypothesis,
                        final_confidence,
                        &atom_verdicts,
                    )
                    .await;
                }
                None
            }
            _ => {
                let confidence_label = match verdict {
                    PromotionVerdict::ReviewQueueHigh => "high",
                    PromotionVerdict::Pending => "medium",
                    PromotionVerdict::Expired => unreachable!(),
                };
                let hash = hypothesis_hash(&input.hypothesis);
                let filename = format!("imagined_{hash}.md");
                let path = review_queue_dir.join(filename);
                let body = render_imagined_md(
                    &input.hypothesis,
                    l1_plausibility,
                    &l2_scores,
                    l3_atomic_aggregate,
                    &atom_verdicts,
                    final_confidence,
                    confidence_label,
                    &external_evidence,
                );
                atomic_write(&path, body.as_bytes())
                    .await
                    .map_err(|e: BoxError| ImaginationProcessError::Write(e.to_string()))?;
                Some(path)
            }
        };

        let aggregate_usage = if input_tokens == 0 && output_tokens == 0 {
            None
        } else {
            Some(LlmUsageWire {
                input_tokens,
                output_tokens,
            })
        };

        Ok(ImaginationProcessOutput {
            l1_plausibility,
            l2_scores,
            l3_atomic_aggregate,
            l3_atom_verdicts: atom_verdicts,
            final_confidence,
            verdict,
            imagined_path,
            req_ids,
            aggregate_usage,
        })
    }

    /// Stage-0 — self-generate candidate hypotheses (W-MEMORY-EVOLUTION PR-6,
    /// the vision core).
    ///
    /// Reads the existing memory corpus (reflections + dreams + recent
    /// session) off disk, fills the hypothesis-generation prompt, issues ONE
    /// reverse-IPC LLM call (`tier3-imagination-hypgen-*` req_id prefix), and
    /// parses the response into a `Vec<ImaginationHypothesis>`.
    ///
    /// Robustness contract:
    /// - Missing files / directories degrade to empty sections (fail-soft, no
    ///   panic) — see `read_synthesis_inputs`.
    /// - Malformed / non-JSON LLM output parses to an empty Vec (fail-soft).
    /// - The returned candidates are capped at `HYPGEN_MAX_CANDIDATES`.
    ///
    /// This does NOT run the L1-L5 confidence pipeline; callers feed each
    /// candidate into `process` (or use `process_generated`, which chains).
    /// W-MEMORY-SELF-EVOLUTION B2 (2026-06-11) — generate the periodic
    /// evolution report (用户的核心诉求本体：反思→做梦→想象→**报告**).
    ///
    /// Corpus = reflections + dream insights + recent session (reused
    /// `read_synthesis_inputs`) + imagination review-queue proposals (with
    /// confidence + external evidence) + the PREVIOUS report (for the
    /// 上期采纳回顾 section — the human-in-the-loop evaluator, B3). One LLM
    /// round-trip over the same reverse-IPC channel as the imagination
    /// layers (`req_id` prefix `tier3-imagination-evolution-report-*`).
    /// Output: `<memory_dir>/reports/evolution-<yyyy-mm-dd>.md` with honest
    /// frontmatter. Same-day reruns overwrite (one report per day).
    /// W3 (2026-07-16, RC-5)：`language` 决定报告的**结构语言**（章节骨架
    /// prompt + frontmatter description）；正文行文语言由 TS 执行器统一
    /// 注入指令（见 `crate::output_language` 模块头的分工契约）。
    pub async fn generate_evolution_report(
        &self,
        memory_dir: &Path,
        model_hint: Option<String>,
        params: LlmCallParams,
        language: crate::output_language::MemoryOutputLanguage,
    ) -> Result<PathBuf, ImaginationProcessError> {
        let inputs = read_synthesis_inputs(memory_dir).await;
        let queue = read_review_queue_summary(memory_dir).await;
        let (prev_label, prev_report) = read_previous_report(memory_dir).await;

        let user_content = format!(
            "## Reflection notes\n{}\n\n## Dream insights\n{}\n\n## Recent session\n{}\n\n## Imagination proposals (review queue)\n{}\n\n## Previous report ({})\n{}",
            placeholder_if_empty(&inputs.reflections, "(no reflection notes)"),
            placeholder_if_empty(&inputs.dreams, "(no dream insights)"),
            placeholder_if_empty(&inputs.recent_session, "(no recent session)"),
            placeholder_if_empty(&queue, "(review queue empty)"),
            prev_label,
            placeholder_if_empty(&prev_report, "(no previous report)"),
        );
        let system_prompt = match language {
            crate::output_language::MemoryOutputLanguage::Zh => EVOLUTION_REPORT_SYSTEM_PROMPT,
            crate::output_language::MemoryOutputLanguage::En => EVOLUTION_REPORT_SYSTEM_PROMPT_EN,
        };
        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: user_content,
            },
        ];
        let (_req_id, response, _usage) = self
            .call_llm("evolution-report", messages, model_hint, params)
            .await?;

        let now = now_ms();
        let (year, month, day) = crate::daily_log::utc_ymd_from_unix_ms(now);
        let date = format!("{year:04}-{month:02}-{day:02}");
        let reports_dir = memory_dir.join("reports");
        tokio::fs::create_dir_all(&reports_dir)
            .await
            .map_err(|e| ImaginationProcessError::Write(e.to_string()))?;
        let path = reports_dir.join(format!("evolution-{date}.md"));
        let description = match language {
            crate::output_language::MemoryOutputLanguage::Zh => {
                format!("自我进化报告 {date}（习惯归纳/问题/外部佐证/整改建议/上期回顾）")
            }
            crate::output_language::MemoryOutputLanguage::En => {
                format!("Self-evolution report {date} (habits/issues/evidence/actions/follow-up)")
            }
        };
        let body = format!(
            "---\ntype: report\nname: evolution-{date}\ndescription: {description}\ncreated_at_ms: {now}\n---\n\n{}\n",
            response.trim()
        );
        atomic_write(&path, body.as_bytes())
            .await
            .map_err(|e: BoxError| ImaginationProcessError::Write(e.to_string()))?;

        // K2 (W-MEMORY-LIFECYCLE 2026-07-09): surface the LATEST evolution
        // report in the MEMORY.md index — the strong system-prompt injection
        // channel — so agents can actually find it (reports/ was previously
        // reachable only via weak query-time recall). Single-line upsert, not
        // append: one report link, always the newest. Fail-soft — an index
        // problem must never undo a successfully-written report — but the
        // failure is recorded in the daily log for observability.
        if let Err(e) = upsert_report_link_in_memory_md(memory_dir, &date).await {
            log::warn!("evolution report: MEMORY.md link upsert failed (fail-soft): {e}");
            record_report_index_upsert_failed(memory_dir, &date, &e.to_string()).await;
        }
        Ok(path)
    }

    pub async fn generate_hypotheses(
        &self,
        memory_dir: &Path,
        model_hint: Option<String>,
        params: LlmCallParams,
    ) -> Result<GeneratedHypotheses, ImaginationProcessError> {
        let inputs = read_synthesis_inputs(memory_dir).await;
        // 8c (W-MEMORY-HYPGEN-VARIANT-WIRE) — 选 hypgen 变体（UCB1，编译期
        // 常量族，镜像 tier3_auto_dream 的 phase3 选择块）；其 addendum 追加到
        // 生成 system prompt，variant_id 回传供 `process_generated` 按 L5
        // verdict 归因。变体本体是人审过的编译期常量（契约 #15-8），进化只动
        // 「选谁」。
        let project_state_dir = crate::dream_gate::project_state_dir_from_memory_dir(memory_dir);
        let variant = {
            let archive = crate::evolution::variants::load_archive(&project_state_dir);
            crate::evolution::variants::select_variant(
                crate::evolution::variants::HYPGEN_VARIANTS,
                &archive,
            )
        };
        let messages = build_hypgen_messages(&inputs, variant.addendum);
        let (req_id, response, usage) = self
            .call_llm("hypgen", messages, model_hint, params)
            .await?;
        let hypotheses = parse_hypgen_json(&response);
        let aggregate_usage = usage.map(|u| LlmUsageWire {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });
        Ok(GeneratedHypotheses {
            hypotheses,
            req_id,
            aggregate_usage,
            variant_id: variant.id.to_string(),
        })
    }

    /// Self-evolving entry point: generate candidate hypotheses (Stage 0) then
    /// run the existing L1-L5 confidence pipeline on each. Returns one
    /// `ImaginationProcessOutput` per candidate (empty Vec when nothing was
    /// generated). The caller-driven `process(input.hypothesis)` path is
    /// untouched and remains available for externally-supplied hypotheses.
    pub async fn process_generated(
        &self,
        input: ImaginationGeneratedInput,
    ) -> Result<ImaginationGeneratedOutput, ImaginationProcessError> {
        let generated = self
            .generate_hypotheses(
                &input.memory_dir,
                input.model_hint.clone(),
                input.params.clone(),
            )
            .await?;

        // 8c — 记住本轮当选变体（下方 into_iter 会 move `hypotheses` 字段）。
        let variant_id = generated.variant_id.clone();

        let mut outputs: Vec<ImaginationProcessOutput> = Vec::new();
        for hypothesis in generated.hypotheses.into_iter().take(HYPGEN_MAX_CANDIDATES) {
            let per_input = ImaginationProcessInput {
                memory_dir: input.memory_dir.clone(),
                gate_payload: input.gate_payload.clone(),
                hypothesis,
                model_hint: input.model_hint.clone(),
                params: input.params.clone(),
                // K10: the watch context rides into every candidate's
                // evidence-gathering hop.
                watch_context: input.watch_context.clone(),
            };
            let output = self.process(per_input).await?;
            outputs.push(output);
        }

        // W6 6d-2 — 每轮 sweep 落一行确定性元评审（下一轮 Stage-0 自校准
        // 信号；fail-soft，统计零 LLM）。
        let review = SweepMetaReview {
            ts_ms: now_ms(),
            candidates: outputs.len(),
            queued_high: outputs
                .iter()
                .filter(|o| o.verdict == PromotionVerdict::ReviewQueueHigh)
                .count(),
            queued_medium: outputs
                .iter()
                .filter(|o| o.verdict == PromotionVerdict::Pending)
                .count(),
            expired: outputs
                .iter()
                .filter(|o| o.verdict == PromotionVerdict::Expired)
                .count(),
        };
        append_meta_review(&input.memory_dir, &review).await;

        // 8c (W-MEMORY-HYPGEN-VARIANT-WIRE) — 把本轮所有候选的 L5 verdict 归因
        // 回当选 hypgen 变体：High 记胜 / Expired 记负 / Pending 中性不记。聚合成
        // (wins, losses) 单次批量 RMW（fail-soft，竞态窗口最小，理由见
        // evolution::variants 模块头）。空生成 → (0,0) → record_verdicts no-op。
        let wins = outputs
            .iter()
            .filter(|o| verdict_outcome(o.verdict) == Some(true))
            .count() as u64;
        let losses = outputs
            .iter()
            .filter(|o| verdict_outcome(o.verdict) == Some(false))
            .count() as u64;
        let project_state_dir =
            crate::dream_gate::project_state_dir_from_memory_dir(&input.memory_dir);
        crate::evolution::variants::record_verdicts(
            &project_state_dir,
            &variant_id,
            wins,
            losses,
            now_ms(),
        )
        .await;

        Ok(ImaginationGeneratedOutput {
            generation_req_id: generated.req_id,
            generation_usage: generated.aggregate_usage,
            outputs,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// L1/L2/L3 message builders + parsers
// ──────────────────────────────────────────────────────────────────────────

fn build_l1_messages(hypothesis: &ImaginationHypothesis) -> Vec<LlmMessage> {
    let context_body = if hypothesis.context.trim().is_empty() {
        "(no supporting context)".to_string()
    } else {
        hypothesis.context.clone()
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_IMAGINATION_L1_SELFRAG_PROMPT
                .replace("{{hypothesis}}", &hypothesis.statement)
                .replace("{{context}}", &context_body),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Apply the rules and return the plausibility JSON.".to_string(),
        },
    ]
}

/// Derive the minimal evidence-gathering query set from a hypothesis.
/// Minimal-start contract (W-MEMORY-EVOLUTION PR-8): a single `WebSearch`
/// keyed on the hypothesis statement. Empty statement → no queries (so
/// `gather_evidence` short-circuits to an empty vec without an emit).
fn derive_evidence_queries(hypothesis: &ImaginationHypothesis) -> Vec<ToolCall> {
    let statement = hypothesis.statement.trim();
    if statement.is_empty() {
        return Vec::new();
    }
    vec![ToolCall {
        kind: ToolKind::WebSearch,
        query: Some(statement.to_string()),
        url: None,
        id: None,
        path: None,
        root: None,
    }]
}

/// K10 — cap on `readFile` probes derived per run (plus at most 1 `listDir`;
/// with the single web search that keeps a watch run at ≤4 calls, well under
/// the SoT ≤8/run executor budget).
pub const WATCH_MAX_READFILE_PROBES: usize = 2;

/// K10 — mechanically derive the read-only watch probes for one hypothesis:
/// exactly one `listDir(root)` plus up to `WATCH_MAX_READFILE_PROBES`
/// `readFile`s for filename-shaped tokens found in the statement (none found
/// → listDir only). Every path is `root`-joined and relative-only (absolute /
/// `..` tokens are rejected here; the TS executor re-validates canonically).
fn derive_watch_calls(hypothesis_statement: &str, watch: &WatchContext) -> Vec<ToolCall> {
    let root_str = watch.root.to_string_lossy().to_string();
    let mut calls = vec![ToolCall {
        kind: ToolKind::ListDir,
        query: None,
        url: None,
        id: Some("watch-listdir-1".to_string()),
        path: Some(root_str.clone()),
        root: Some(root_str.clone()),
    }];
    for (i, rel) in derive_statement_filenames(hypothesis_statement)
        .into_iter()
        .take(WATCH_MAX_READFILE_PROBES)
        .enumerate()
    {
        let abs = watch.root.join(&rel);
        calls.push(ToolCall {
            kind: ToolKind::ReadFile,
            query: None,
            url: None,
            id: Some(format!("watch-readfile-{}", i + 1)),
            path: Some(abs.to_string_lossy().to_string()),
            root: Some(root_str.clone()),
        });
    }
    calls
}

/// K10 — filename-shaped tokens in a hypothesis statement, in order of
/// appearance, deduped. A token qualifies when it carries an extension-like
/// tail (`.` + 1-8 alphanumerics), stays RELATIVE (no leading `/` or drive
/// prefix), and contains no `..` component. Purely mechanical — misses are
/// fine (the probe read fails soft executor-side).
fn derive_statement_filenames(statement: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in statement.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '，' | '。'
                    | '、'
                    | '（'
                    | '）'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '`'
                    | '"'
                    | '\''
                    | ','
                    | ';'
                    | ':'
                    | '<'
                    | '>'
            )
    }) {
        let token = raw.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | '。' | '！' | '？'));
        if token.len() < 3 || !token.contains('.') {
            continue;
        }
        // Extension-like tail: `.` + 1-8 ascii alphanumerics.
        let Some((_, ext)) = token.rsplit_once('.') else {
            continue;
        };
        if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        // Relative-only + no traversal + no URL-ish tokens.
        let normalized = token.replace('\\', "/");
        if normalized.starts_with('/')
            || normalized.contains("://")
            || normalized
                .split('/')
                .any(|seg| seg == ".." || seg.is_empty())
            || token.chars().nth(1) == Some(':')
        {
            continue;
        }
        if !out.iter().any(|seen| seen == &normalized) {
            out.push(normalized);
        }
    }
    out
}

/// K10 — decode the executor's listDir evidence encoding in place.
///
/// S2's tool executor returns `listDir` results as a `ToolEvidence` whose
/// `content` is `JSON.stringify({entries: [names…][, truncated: true]})` and
/// whose `source_url` is the probed directory. Rewrite such items into a
/// newline listing (`(empty directory)` when no entries; a `…（清单被截断）`
/// tail when the executor clipped it) so the L2/L3 prompts show names instead
/// of JSON syntax. Anything that does not parse to that exact shape — web
/// snippets, readFile text (including its `[truncated: file is N bytes, read
/// limit 65536]` marker) — is left untouched. Only invoked on watch runs.
fn decode_watch_listing_evidence(evidence: &mut [ToolEvidence]) {
    for item in evidence.iter_mut() {
        let trimmed = item.content.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(entries) = value.get("entries").and_then(|v| v.as_array()) else {
            continue;
        };
        let names: Vec<&str> = entries.iter().filter_map(|e| e.as_str()).collect();
        let mut listing = if names.is_empty() {
            "(empty directory)".to_string()
        } else {
            names.join("\n")
        };
        if value.get("truncated").and_then(|v| v.as_bool()) == Some(true) {
            listing.push_str("\n…（清单被截断）");
        }
        item.content = listing;
    }
}

/// Truncate evidence content to `EVIDENCE_CONTENT_MAX_CHARS` (char-boundary
/// safe). Appends a single-space ellipsis marker when truncated so the LLM
/// knows the source was clipped.
fn truncate_content(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= EVIDENCE_CONTENT_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(EVIDENCE_CONTENT_MAX_CHARS).collect();
    out.push_str(" …");
    out
}

/// Project externally-gathered `ToolEvidence` into the `ImaginationEvidenceRef`
/// shape the L3 verifier consumes: `id` = `source_url` (traceable), `snippet` =
/// truncated content. Empty input → empty output (caller falls back to the
/// hypothesis's internal refs).
fn external_evidence_to_refs(evidence: &[ToolEvidence]) -> Vec<ImaginationEvidenceRef> {
    evidence
        .iter()
        .map(|e| ImaginationEvidenceRef {
            id: e.source_url.clone(),
            snippet: truncate_content(&e.content),
        })
        .collect()
}

/// Format external evidence for the L2 `{{evidence_summary}}` placeholder:
/// one `[source_url] content(truncated)` line per item, title prefixed when
/// present.
fn format_external_evidence_summary(evidence: &[ToolEvidence]) -> String {
    evidence
        .iter()
        .map(|e| {
            let body = truncate_content(&e.content);
            match e.title.as_deref() {
                Some(title) if !title.trim().is_empty() => {
                    format!("- [{}] {}: {}", e.source_url, title.trim(), body)
                }
                _ => format!("- [{}] {}", e.source_url, body),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build L2 messages, preferring fresh external evidence over the hypothesis's
/// internal `evidence_refs`. When `external_evidence` is empty, falls back to
/// the internal refs (preserving the pre-PR-8 behavior — no regression).
fn build_l2_messages_with_evidence(
    hypothesis: &ImaginationHypothesis,
    external_evidence: &[ToolEvidence],
) -> Vec<LlmMessage> {
    let evidence_body = if !external_evidence.is_empty() {
        format_external_evidence_summary(external_evidence)
    } else if hypothesis.evidence_refs.is_empty() {
        "(no evidence refs)".to_string()
    } else {
        hypothesis
            .evidence_refs
            .iter()
            .map(|e| format!("- [{}] {}", e.id, e.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_IMAGINATION_L2_FOUR_DIMENSIONS_PROMPT
                .replace("{{hypothesis}}", &hypothesis.statement)
                .replace("{{evidence_summary}}", &evidence_body),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Apply the rules and return the four-dimension JSON.".to_string(),
        },
    ]
}

fn build_l3_messages(atom: &str, evidence_refs: &[ImaginationEvidenceRef]) -> Vec<LlmMessage> {
    let evidence_body = if evidence_refs.is_empty() {
        "(no evidence refs)".to_string()
    } else {
        evidence_refs
            .iter()
            .map(|e| format!("- [{}] {}", e.id, e.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    };
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: TIER3_IMAGINATION_L3_ATOMIC_VERIFY_PROMPT
                .replace("{{atom}}", atom)
                .replace("{{evidence_refs}}", &evidence_body),
        },
        LlmMessage {
            role: "user".to_string(),
            content: "Apply the rules and return the verdict JSON.".to_string(),
        },
    ]
}

fn parse_l1_json(raw: &str) -> Option<f64> {
    let trimmed = strip_fences(raw.trim());
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let p = value.get("plausibility")?.as_f64()?;
    Some(p.clamp(0.0, 1.0))
}

fn parse_l2_json(raw: &str) -> Option<FourDimensionScores> {
    let trimmed = strip_fences(raw.trim());
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let novelty = value.get("novelty")?.as_f64()?.clamp(0.0, 1.0);
    let consistency = value.get("consistency")?.as_f64()?.clamp(0.0, 1.0);
    let groundedness = value.get("groundedness")?.as_f64()?.clamp(0.0, 1.0);
    let actionability = value.get("actionability")?.as_f64()?.clamp(0.0, 1.0);
    Some(FourDimensionScores {
        novelty,
        consistency,
        groundedness,
        actionability,
    })
}

fn parse_l3_json(raw: &str) -> Option<AtomVerdict> {
    let trimmed = strip_fences(raw.trim());
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let verdict_str = value.get("verdict")?.as_str()?;
    let verdict = match verdict_str {
        "supported" => AtomVerdictKind::Supported,
        "refuted" => AtomVerdictKind::Refuted,
        "inconclusive" => AtomVerdictKind::Inconclusive,
        _ => return None,
    };
    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let citing_evidence_ids = value
        .get("citing_evidence_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some(AtomVerdict {
        atom: String::new(), // caller substitutes
        verdict,
        confidence,
        citing_evidence_ids,
    })
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(stripped) = s.strip_prefix("```json\n") {
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
// Stage-0 hypothesis self-generation — disk synthesis readers + parser
// ──────────────────────────────────────────────────────────────────────────

/// Synthesis corpus read off disk for Stage-0 generation. Each section is a
/// pre-formatted, capped string (or a placeholder when empty / unreadable).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SynthesisInputs {
    /// Reflection memory notes (`<memory_dir>/*.md`, excluding MEMORY.md /
    /// SESSION.md / .session-*), formatted as a labeled list.
    pub reflections: String,
    /// Dream insights (`<memory_dir>/dreams/insight_*.md`, insight preferred).
    pub dreams: String,
    /// Recent session content (`SESSION.md` + optional recent transcript tail).
    pub recent_session: String,
    /// W6 6d-1 — 负知识：`imagination/refuted/` 里最近的淘汰假设一行清单
    /// （防重想）。空 = 无淘汰史。
    pub refuted: String,
    /// W6 6d-2 — 元评审：`imagination/meta-review.jsonl` 尾部若干行渲染
    /// （生成自校准信号）。空 = 首轮。
    pub meta_review: String,
}

/// B2 helper — `s` when non-empty, otherwise the placeholder.
fn placeholder_if_empty<'a>(s: &'a str, placeholder: &'a str) -> &'a str {
    if s.trim().is_empty() {
        placeholder
    } else {
        s
    }
}

/// B2 helper — summarize the imagination review queue for the evolution
/// report: per entry the frontmatter head (statement/confidence/evidence
/// fields live there) capped at `HYPGEN_MAX_FILE_CHARS`. Fail-soft.
async fn read_review_queue_summary(memory_dir: &Path) -> String {
    let queue_dir = memory_dir.join("imagination").join("review-queue");
    let mut entries = match tokio::fs::read_dir(&queue_dir).await {
        Ok(e) => e,
        Err(_) => return String::new(),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".md"))
        {
            files.push(path);
        }
    }
    files.sort();
    files.truncate(REPORT_MAX_QUEUE_FILES);
    format_labeled_files(&files).await
}

/// B2 helper — latest previous evolution report (`reports/evolution-*.md`,
/// lexicographic max = newest because the name embeds the date). Returns
/// `(label, capped content)`; both empty when none exists. Fail-soft.
async fn read_previous_report(memory_dir: &Path) -> (String, String) {
    let reports_dir = memory_dir.join("reports");
    let mut entries = match tokio::fs::read_dir(&reports_dir).await {
        Ok(e) => e,
        Err(_) => return (String::from("none"), String::new()),
    };
    let mut newest: Option<PathBuf> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_file() || !name.starts_with("evolution-") || !name.ends_with(".md") {
            continue;
        }
        if newest
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .is_none_or(|prev| name > prev)
        {
            newest = Some(path);
        }
    }
    let Some(path) = newest else {
        return (String::from("none"), String::new());
    };
    let label = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("previous")
        .to_string();
    let mut content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    if content.len() > REPORT_MAX_PREV_CHARS {
        content.truncate(REPORT_MAX_PREV_CHARS);
        content.push_str("\n…(truncated)");
    }
    (label, content)
}

/// Read the Stage-0 synthesis corpus from `memory_dir`. Every read is
/// fail-soft: a missing file / directory / unreadable entry degrades to an
/// empty (placeholder) section rather than erroring or panicking.
pub async fn read_synthesis_inputs(memory_dir: &Path) -> SynthesisInputs {
    let reflections = read_reflections(memory_dir).await;
    let dreams = read_dream_insights(memory_dir).await;
    let recent_session = read_recent_session(memory_dir).await;
    let refuted = read_refuted_summaries(memory_dir).await;
    let meta_review = read_meta_review_tail(memory_dir).await;
    SynthesisInputs {
        reflections,
        dreams,
        recent_session,
        refuted,
        meta_review,
    }
}

/// W6 6d-1 — `imagination/refuted/` 目录路径。
fn refuted_dir(memory_dir: &Path) -> PathBuf {
    memory_dir.join("imagination").join(REFUTED_DIRNAME)
}

/// W6 6d-2 — `imagination/meta-review.jsonl` 路径。
fn meta_review_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join("imagination").join(META_REVIEW_FILENAME)
}

/// W6 6d-1 — 把低置信淘汰的假设写入负知识库（fail-soft：负知识写失败绝不
/// 影响 sweep 本身）。文件形态与 review-queue 同族（frontmatter + 正文首行
/// = statement，读取端只取首行做防重想清单）。目录滚动上限，最旧先出。
pub(crate) async fn write_refuted_hypothesis(
    memory_dir: &Path,
    hypothesis: &ImaginationHypothesis,
    final_confidence: f64,
    atom_verdicts: &[AtomVerdict],
) {
    let dir = refuted_dir(memory_dir);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        log::warn!("[imagination] refuted dir create failed (fail-soft): {e}");
        return;
    }
    let hash = hypothesis_hash(hypothesis);
    let path = dir.join(format!("refuted_{hash}.md"));
    if path.exists() {
        return;
    }
    let mut body = String::new();
    body.push_str("---\n");
    body.push_str("type: imagined\n");
    body.push_str("status: refuted\n");
    body.push_str(&format!("confidence_value: {final_confidence:.2}\n"));
    body.push_str(&format!("created_at_ms: {}\n", now_ms()));
    body.push_str("---\n\n");
    body.push_str(hypothesis.statement.trim());
    body.push('\n');
    for verdict in atom_verdicts {
        let kind = match verdict.verdict {
            AtomVerdictKind::Supported => "supported",
            AtomVerdictKind::Refuted => "refuted",
            AtomVerdictKind::Inconclusive => "inconclusive",
        };
        body.push_str(&format!(
            "- [{kind}, conf {:.2}] {}\n",
            verdict.confidence, verdict.atom
        ));
    }
    if let Err(e) = atomic_write(&path, body.as_bytes()).await {
        log::warn!("[imagination] refuted write failed (fail-soft): {e}");
        return;
    }
    prune_refuted_dir(&dir).await;
}

/// 负知识目录滚动：超过 `REFUTED_MAX_FILES` 时删最旧（mtime 升序）。
async fn prune_refuted_dir(dir: &Path) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(u64, PathBuf)> = read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                return None;
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            Some((mtime, path))
        })
        .collect();
    if files.len() <= REFUTED_MAX_FILES {
        return;
    }
    files.sort_by_key(|(mtime, _)| *mtime);
    let excess = files.len() - REFUTED_MAX_FILES;
    for (_, path) in files.into_iter().take(excess) {
        let _ = tokio::fs::remove_file(&path).await;
    }
}

/// W6 6d-1 — 读负知识清单（最新优先，`- <statement>` 行，防重想注入）。
async fn read_refuted_summaries(memory_dir: &Path) -> String {
    let dir = refuted_dir(memory_dir);
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return String::new();
    };
    let mut files: Vec<(u64, PathBuf)> = read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                return None;
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            Some((mtime, path))
        })
        .collect();
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    let mut lines: Vec<String> = Vec::new();
    for (_, path) in files.into_iter().take(HYPGEN_MAX_REFUTED_LINES) {
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        // 正文首个非空行 = statement（写入端契约）。
        let statement = crate::dedup_hash::memory_body(&raw)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .to_string();
        if statement.is_empty() {
            continue;
        }
        lines.push(format!("- {}", truncate_chars(&statement, 200)));
    }
    lines.join("\n")
}

/// W6 6d-2 — sweep 元评审统计（确定性，零 LLM）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepMetaReview {
    pub ts_ms: u64,
    pub candidates: usize,
    pub queued_high: usize,
    pub queued_medium: usize,
    pub expired: usize,
}

/// 追加一行元评审并滚动截断到 `META_REVIEW_MAX_LINES`。fail-soft。
pub(crate) async fn append_meta_review(memory_dir: &Path, review: &SweepMetaReview) {
    let path = meta_review_path(memory_dir);
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            log::warn!("[imagination] meta-review dir create failed (fail-soft): {e}");
            return;
        }
    }
    let Ok(line) = serde_json::to_string(review) else {
        return;
    };
    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let mut lines: Vec<&str> = existing.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.push(&line);
    let start = lines.len().saturating_sub(META_REVIEW_MAX_LINES);
    let content = lines[start..].join("\n") + "\n";
    if let Err(e) = atomic_write(&path, content.as_bytes()).await {
        log::warn!("[imagination] meta-review append failed (fail-soft): {e}");
    }
}

/// W6 6d-2 — 渲染元评审尾部（Stage-0 注入；人可读一行一轮）。
async fn read_meta_review_tail(memory_dir: &Path) -> String {
    let path = meta_review_path(memory_dir);
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return String::new();
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(HYPGEN_META_REVIEW_TAIL);
    lines[start..]
        .iter()
        .filter_map(|line| serde_json::from_str::<SweepMetaReview>(line).ok())
        .map(|review| {
            format!(
                "- sweep@{}: {} candidates → {} high, {} medium, {} expired",
                review.ts_ms,
                review.candidates,
                review.queued_high,
                review.queued_medium,
                review.expired
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when `name` is a reflection memory note (a top-level `*.md` that is
/// neither the MEMORY.md / SESSION.md index nor a per-session snapshot).
fn is_reflection_file(name: &str) -> bool {
    if !name.ends_with(".md") {
        return false;
    }
    if name == "MEMORY.md" || name == "SESSION.md" {
        return false;
    }
    if name.starts_with(".session-") {
        return false;
    }
    true
}

/// Read top-level reflection memory notes from `memory_dir`. Sorted by
/// filename for determinism; capped at `HYPGEN_MAX_REFLECTION_FILES` files and
/// `HYPGEN_MAX_FILE_CHARS` chars per file.
async fn read_reflections(memory_dir: &Path) -> String {
    let mut entries = match tokio::fs::read_dir(memory_dir).await {
        Ok(e) => e,
        Err(_) => return String::new(),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_reflection_file(name) {
            files.push(path);
        }
    }
    files.sort();
    files.truncate(HYPGEN_MAX_REFLECTION_FILES);
    format_labeled_files(&files).await
}

/// Read dream insights (`dreams/insight_*.md`) from `memory_dir`. Insight is
/// preferred over fragment per the task spec (insights are the stronger
/// signal). Sorted by filename; capped at `HYPGEN_MAX_DREAM_FILES`.
async fn read_dream_insights(memory_dir: &Path) -> String {
    let dreams_dir = memory_dir.join("dreams");
    let mut entries = match tokio::fs::read_dir(&dreams_dir).await {
        Ok(e) => e,
        Err(_) => return String::new(),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("insight_") && name.ends_with(".md") {
            files.push(path);
        }
    }
    files.sort();
    files.truncate(HYPGEN_MAX_DREAM_FILES);
    format_labeled_files(&files).await
}

/// Read recent session content: `SESSION.md` plus, when present, the tail of
/// the most-recently-modified `.session-*.md` snapshot (a recent transcript
/// surrogate already maintained by Tier-1). Both reads are fail-soft.
async fn read_recent_session(memory_dir: &Path) -> String {
    let mut sections: Vec<String> = Vec::new();

    let session_path = memory_dir.join("SESSION.md");
    if let Ok(body) = tokio::fs::read_to_string(&session_path).await {
        let body = truncate_chars(body.trim(), HYPGEN_MAX_SESSION_CHARS);
        if !body.is_empty() {
            sections.push(format!("## SESSION.md\n\n{body}"));
        }
    }

    if let Some(recent) = most_recent_session_snapshot(memory_dir).await {
        // `read_transcript_content` is robust to non-jsonl bodies (it falls
        // through line-by-line, skipping unparseable lines → empty), but the
        // `.session-*.md` snapshots are markdown, so read them directly and
        // tail-truncate to keep the most recent context.
        if let Ok(body) = tokio::fs::read_to_string(&recent).await {
            let body = truncate_chars(body.trim(), HYPGEN_MAX_TRANSCRIPT_CHARS);
            if !body.is_empty() {
                let label = recent
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("session-snapshot");
                sections.push(format!("## Recent session snapshot ({label})\n\n{body}"));
            }
        }
    }

    sections.join("\n\n")
}

/// Find the most-recently-modified `.session-*.md` snapshot under `memory_dir`.
async fn most_recent_session_snapshot(memory_dir: &Path) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(memory_dir).await.ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !(name.starts_with(".session-") && name.ends_with(".md")) {
            continue;
        }
        let mtime = match entry.metadata().await {
            Ok(meta) => meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            Err(_) => 0,
        };
        match &best {
            Some((best_mtime, _)) if *best_mtime >= mtime => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
}

/// Read each file (fail-soft, char-capped) and concatenate as a labeled list.
async fn format_labeled_files(files: &[PathBuf]) -> String {
    let mut sections: Vec<String> = Vec::new();
    for path in files {
        let Ok(body) = tokio::fs::read_to_string(path).await else {
            continue;
        };
        let body = truncate_chars(body.trim(), HYPGEN_MAX_FILE_CHARS);
        if body.is_empty() {
            continue;
        }
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.md");
        sections.push(format!("## {label}\n\n{body}"));
    }
    sections.join("\n\n")
}

/// Truncate to at most `max` chars on a char boundary (not byte index), to
/// avoid panicking on multi-byte UTF-8.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Build the Stage-0 generation messages, substituting the three corpus
/// sections (empty sections → explicit placeholder so the model sees the gap).
fn build_hypgen_messages(inputs: &SynthesisInputs, addendum: &str) -> Vec<LlmMessage> {
    let reflections = section_or_placeholder(&inputs.reflections, "(no reflection notes)");
    let dreams = section_or_placeholder(&inputs.dreams, "(no dream insights)");
    let recent_session = section_or_placeholder(&inputs.recent_session, "(no recent session)");
    // W6 6d-1/6d-2 — 负知识 + 元评审注入（空 = 显式占位，模型看得见缺口）。
    let refuted = section_or_placeholder(&inputs.refuted, "(no refuted hypotheses yet)");
    let meta_review = section_or_placeholder(&inputs.meta_review, "(no prior sweeps)");
    // 8c (W-MEMORY-HYPGEN-VARIANT-WIRE) — 变体 addendum 追加到基线 system
    // prompt 末尾（`hypgen/v0` = 空串 no-op，与 phase3 addendum 注入同构）。
    let system = format!(
        "{}{}",
        TIER3_IMAGINATION_HYPOTHESIS_GEN_PROMPT
            .replace("{{reflections}}", &reflections)
            .replace("{{dreams}}", &dreams)
            .replace("{{recent_session}}", &recent_session)
            .replace("{{refuted}}", &refuted)
            .replace("{{meta_review}}", &meta_review),
        addendum,
    );
    vec![
        LlmMessage {
            role: "system".to_string(),
            content: system,
        },
        LlmMessage {
            role: "user".to_string(),
            content:
                "Synthesize candidate hypotheses across the three sources and return the JSON."
                    .to_string(),
        },
    ]
}

fn section_or_placeholder(body: &str, placeholder: &str) -> String {
    if body.trim().is_empty() {
        placeholder.to_string()
    } else {
        body.to_string()
    }
}

/// Parse the Stage-0 generation response into candidate hypotheses. Robust to
/// markdown fences and malformed JSON: any failure yields an empty Vec
/// (fail-soft, never panics). Each candidate's `atoms` defaults to a single
/// atom = the statement when the model omits / empties the atoms array, so the
/// downstream L3 layer always has at least one claim to check.
pub fn parse_hypgen_json(raw: &str) -> Vec<ImaginationHypothesis> {
    let trimmed = strip_fences(raw.trim());
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match value.get("hypotheses").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out: Vec<ImaginationHypothesis> = Vec::new();
    for item in arr {
        let Some(statement) = item.get("statement").and_then(|v| v.as_str()) else {
            continue;
        };
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        let mut atoms: Vec<String> = item
            .get("atoms")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if atoms.is_empty() {
            atoms.push(statement.to_string());
        }
        let evidence_refs: Vec<ImaginationEvidenceRef> = item
            .get("evidence_refs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let id = e.get("id").and_then(|v| v.as_str())?.trim().to_string();
                        let snippet = e
                            .get("snippet")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if id.is_empty() {
                            return None;
                        }
                        Some(ImaginationEvidenceRef { id, snippet })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(ImaginationHypothesis {
            statement: statement.to_string(),
            atoms,
            evidence_refs,
            context: String::new(),
        });
    }
    out
}

impl FourDimensionScores {
    /// Neutral default — used when LLM output fails to parse. 0.5 across the
    /// board signals "cannot decide", which feeds into the L4 fusion as an
    /// honest no-op rather than 0 (which would mass-fail every hypothesis).
    pub fn neutral() -> Self {
        Self {
            novelty: 0.5,
            consistency: 0.5,
            groundedness: 0.5,
            actionability: 0.5,
        }
    }
}

/// L3 aggregation rule: convert per-atom verdicts to a single 0.0-1.0
/// confidence by averaging per-atom support contribution.
///
/// Per-atom contribution:
/// - Supported → +confidence
/// - Refuted   → -confidence
/// - Inconclusive → 0 (neutral)
///
/// Final = clamp((sum / N + 1) / 2, 0.0, 1.0). The (x+1)/2 transform maps
/// `[-1, +1]` (worst → best) to `[0.0, 1.0]`. Empty atoms list → 0.5 neutral.
pub fn aggregate_l3(verdicts: &[AtomVerdict]) -> f64 {
    if verdicts.is_empty() {
        return 0.5;
    }
    let sum: f64 = verdicts
        .iter()
        .map(|v| match v.verdict {
            AtomVerdictKind::Supported => v.confidence,
            AtomVerdictKind::Refuted => -v.confidence,
            AtomVerdictKind::Inconclusive => 0.0,
        })
        .sum();
    let avg = sum / verdicts.len() as f64;
    ((avg + 1.0) / 2.0).clamp(0.0, 1.0)
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers
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

/// Append one imagination audit event to the project daily log. Fail-soft:
/// the observability hop must never fail the pipeline, so IO errors are
/// logged and dropped. The synthetic `TranscriptMeta` mirrors
/// `result_listener::append_runner_daily_log` (imagination has no real
/// transcript of its own).
async fn append_imagination_daily_log(memory_dir: &Path, kind: &str, payload: serde_json::Value) {
    let project_state_dir = crate::dream_gate::project_state_dir_from_memory_dir(memory_dir);
    let occurred_at_ms = now_ms();
    let transcript_meta = crate::daily_log::TranscriptMeta {
        session_id: "tier3-imagination".to_owned(),
        path: project_state_dir.join("tier3-imagination.jsonl"),
        mtime_ms: occurred_at_ms,
        size_bytes: 0,
        sealed: true,
    };
    let event = crate::daily_log::SessionEvent {
        event_id: format!("tier3-imagination-{kind}-{occurred_at_ms}"),
        kind: kind.to_owned(),
        occurred_at_ms,
        payload,
    };
    if let Err(e) =
        crate::daily_log::append_daily_log(&project_state_dir, &transcript_meta, &[event]).await
    {
        log::warn!("tier3 imagination: daily-log append failed (fail-soft): {e}");
    }
}

/// D3 (W-MEMORY-LIFECYCLE 2026-07-09) — record that every confidence layer
/// (L1 + L2 + all L3 atoms) failed to parse and the hypothesis was expired
/// without persisting. Payload carries a statement snippet for diagnosis.
async fn record_all_layers_parse_failed(memory_dir: &Path, statement: &str) {
    let snippet: String = statement.chars().take(160).collect();
    append_imagination_daily_log(
        memory_dir,
        "memory.imagination.all_layers_parse_failed",
        serde_json::json!({ "statement": snippet }),
    )
    .await;
}

/// K2 — record a failed MEMORY.md report-link upsert (the report itself was
/// written; only the index hop failed).
async fn record_report_index_upsert_failed(memory_dir: &Path, date: &str, error: &str) {
    append_imagination_daily_log(
        memory_dir,
        "memory.report.index_upsert_failed",
        serde_json::json!({ "date": date, "error": error }),
    )
    .await;
}

/// K2 (W-MEMORY-LIFECYCLE 2026-07-09) — upsert the "latest evolution report"
/// link line into `<memory_dir>/MEMORY.md`:
///
/// `- [进化报告 <date>](reports/evolution-<date>.md) — 做梦与想象的自我进化结论`
///
/// Exactly ONE such line is kept: an existing line starting with the
/// `- [进化报告 ` prefix is REPLACED in place (stray duplicates from older
/// states are dropped); otherwise the line is appended. The file is created
/// when missing. Mirrors `tier3_auto_dream::auto_promote_insights`'s
/// MEMORY.md indexing style (read → line-level edit → `atomic_write`).
async fn upsert_report_link_in_memory_md(memory_dir: &Path, date: &str) -> Result<(), BoxError> {
    const REPORT_LINK_PREFIX: &str = "- [进化报告 ";

    let memory_md = memory_dir.join("MEMORY.md");
    let new_line = format!(
        "{REPORT_LINK_PREFIX}{date}](reports/evolution-{date}.md) — 做梦与想象的自我进化结论"
    );

    let existing = match tokio::fs::read_to_string(&memory_md).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line.starts_with(REPORT_LINK_PREFIX) {
            if !replaced {
                lines.push(new_line.clone());
                replaced = true;
            }
            // Subsequent report-link lines are dropped (single-entry upsert —
            // never accumulate one line per day).
            continue;
        }
        lines.push(line.to_string());
    }
    if !replaced {
        lines.push(new_line);
    }

    let mut content = lines.join("\n");
    content.push('\n');
    if content == existing {
        return Ok(()); // idempotent rerun — skip the write.
    }
    atomic_write(&memory_md, content.as_bytes()).await
}

/// Stable hash of the hypothesis statement (sha256 short prefix) used as the
/// filename stem. Same statement → same hash → idempotent overwrite (the
/// atomic_write makes the overwrite race-free).
fn hypothesis_hash(hypothesis: &ImaginationHypothesis) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hypothesis.statement.as_bytes());
    let digest = hasher.finalize();
    hex_short(&digest, 16)
}

fn hex_short(digest: &[u8], n: usize) -> String {
    let mut out = String::with_capacity(n * 2);
    for byte in digest.iter().take(n) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Compute ISO 8601 expiry string for `now + REVIEW_EXPIRY_DAYS`. Format
/// approximates RFC 3339: `YYYY-MM-DDTHH:MM:SSZ` (UTC, second precision).
///
/// Implementation: avoid pulling chrono in just for this. Use the
/// Julian-day → Gregorian conversion (Meeus algorithm) on UNIX ms.
pub fn iso8601_expiry_from_now() -> String {
    iso8601_expiry_from_ms(now_ms())
}

/// Same as `iso8601_expiry_from_now` but accepts an explicit `now_ms` —
/// used by tests to assert deterministic output.
pub fn iso8601_expiry_from_ms(now_ms: u64) -> String {
    let expiry_ms = now_ms.saturating_add(REVIEW_EXPIRY_DAYS * 86_400_000);
    iso8601_utc(expiry_ms)
}

fn iso8601_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let day = secs / 86400;
    let mut remainder = secs % 86400;
    let hour = remainder / 3600;
    remainder %= 3600;
    let minute = remainder / 60;
    let second = remainder % 60;

    // UNIX epoch = 1970-01-01 (Julian Day 2440588).
    let jd = day + 2440588;
    let (year, month, dom) = jd_to_ymd(jd);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, dom, hour, minute, second
    )
}

/// Convert Julian Day Number → (year, month, day-of-month) using Meeus'
/// algorithm. Valid for all Gregorian dates we care about (post-1970).
fn jd_to_ymd(jd: u64) -> (u32, u32, u32) {
    // Use i64 math to avoid underflow during intermediate computation.
    let j = jd as i64;
    let f = j + 1401 + ((((4 * j + 274_277) / 146_097) * 3) / 4) - 38;
    let e = 4 * f + 3;
    let g = (e % 1461) / 4;
    let h = 5 * g + 2;
    let day = ((h % 153) / 5 + 1) as u32;
    let month = (((h / 153 + 2) % 12) + 1) as u32;
    let year = ((e / 1461) - 4716 + (12 + 2 - month as i64) / 12) as u32;
    (year, month, day)
}

/// Render an `imagined_*.md` body. Frontmatter carries confidence, status,
/// expiry, layer scores, and external evidence sources; the body carries the
/// hypothesis, per-atom verdicts, evidence refs, and gathered external evidence.
///
/// `external_evidence` (W-MEMORY-EVOLUTION PR-8) is the fresh external evidence
/// the L2/L3 scoring graded against. When non-empty it is emitted as an
/// `evidence_sources:` YAML list in the frontmatter so the produced artifact
/// carries traceable `source_url` + `fetched_at_ms` provenance (lets a human
/// reviewer judge trustworthiness). Empty → the field is omitted entirely.
#[allow(clippy::too_many_arguments)]
fn render_imagined_md(
    hypothesis: &ImaginationHypothesis,
    l1: f64,
    l2: &FourDimensionScores,
    l3: f64,
    atom_verdicts: &[AtomVerdict],
    final_confidence: f64,
    confidence_label: &str,
    external_evidence: &[ToolEvidence],
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    // K2 (W-MEMORY-LIFECYCLE 2026-07-09): imagined drafts carry an explicit
    // `type` so the SE indexer's type mapping can classify them (typeless
    // frontmatter was skipped as MissingType → invisible to agent recall).
    out.push_str("type: imagined\n");
    out.push_str(&format!("confidence: {confidence_label}\n"));
    out.push_str("status: pending-review\n");
    out.push_str(&format!("expiry: {}\n", iso8601_expiry_from_now()));
    out.push_str(&format!("final_confidence: {:.4}\n", final_confidence));
    out.push_str(&format!("l1_plausibility: {:.4}\n", l1));
    out.push_str(&format!("l2_novelty: {:.4}\n", l2.novelty));
    out.push_str(&format!("l2_consistency: {:.4}\n", l2.consistency));
    out.push_str(&format!("l2_groundedness: {:.4}\n", l2.groundedness));
    out.push_str(&format!("l2_actionability: {:.4}\n", l2.actionability));
    out.push_str(&format!("l3_atomic_aggregate: {:.4}\n", l3));
    if !external_evidence.is_empty() {
        // Traceable external provenance. YAML list of `{url, fetched_at_ms}`
        // maps. Each url is single-quoted to stay safe with `:` / special
        // chars in URLs; single-quotes inside the url are YAML-escaped (`''`).
        out.push_str("evidence_sources:\n");
        for e in external_evidence {
            let safe_url = e.source_url.replace('\'', "''");
            out.push_str(&format!(
                "  - url: '{}'\n    fetched_at_ms: {}\n",
                safe_url, e.fetched_at_ms
            ));
        }
    }
    out.push_str("---\n\n");
    out.push_str("# Hypothesis\n\n");
    out.push_str(hypothesis.statement.trim());
    out.push_str("\n\n");
    if !atom_verdicts.is_empty() {
        out.push_str("# Atomic verdicts\n\n");
        for v in atom_verdicts {
            let kind = match v.verdict {
                AtomVerdictKind::Supported => "supported",
                AtomVerdictKind::Refuted => "refuted",
                AtomVerdictKind::Inconclusive => "inconclusive",
            };
            out.push_str(&format!(
                "- [{kind}, conf {:.2}] {}\n",
                v.confidence, v.atom
            ));
        }
        out.push('\n');
    }
    if !hypothesis.evidence_refs.is_empty() {
        out.push_str("# Evidence refs\n\n");
        for e in &hypothesis.evidence_refs {
            out.push_str(&format!("- [{}] {}\n", e.id, e.snippet));
        }
        out.push('\n');
    }
    if !external_evidence.is_empty() {
        out.push_str("# External evidence (gathered)\n\n");
        for e in external_evidence {
            let title = e.title.as_deref().unwrap_or("").trim();
            if title.is_empty() {
                out.push_str(&format!(
                    "- {} (fetched {} ms)\n",
                    e.source_url, e.fetched_at_ms
                ));
            } else {
                out.push_str(&format!(
                    "- {} — {} (fetched {} ms)\n",
                    title, e.source_url, e.fetched_at_ms
                ));
            }
            out.push_str(&format!("  {}\n", truncate_content(&e.content)));
        }
        out.push('\n');
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── W-MEMORY-SELF-EVOLUTION B2: evolution report ──

    #[tokio::test]
    async fn evolution_report_writes_dated_file_and_feeds_previous_report() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        tokio::fs::write(memory_dir.join("reflect.md"), "用户偏好暗色主题")
            .await
            .unwrap();
        // A pre-existing older report must surface as the previous-report
        // section input (lexicographic max of evolution-*.md).
        tokio::fs::create_dir_all(memory_dir.join("reports"))
            .await
            .unwrap();
        tokio::fs::write(
            memory_dir.join("reports").join("evolution-2026-01-01.md"),
            "old report body",
        )
        .await
        .unwrap();

        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts(
                "evolution-report",
                vec!["## 用户习惯归纳\n- 偏好暗色主题".to_string()],
            )
            .await;

        let path = processor
            .generate_evolution_report(
                &memory_dir,
                None,
                LlmCallParams::default(),
                crate::output_language::MemoryOutputLanguage::Zh,
            )
            .await
            .expect("report generation");

        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("evolution-") && name.ends_with(".md"));
        assert!(path.parent().unwrap().ends_with("reports"));
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(body.starts_with("---\ntype: report\n"));
        assert!(body.contains("## 用户习惯归纳"));

        // The LLM request must carry the previous report + the corpus.
        let requests = scripted.recorded().await;
        let report_req = requests
            .iter()
            .find(|r| r.req_id.contains("evolution-report"))
            .expect("report request recorded");
        let user_msg = &report_req.messages.last().unwrap().content;
        assert!(user_msg.contains("old report body"));
        assert!(user_msg.contains("用户偏好暗色主题"));
    }

    #[tokio::test]
    async fn read_previous_report_picks_latest_and_handles_absence() {
        let tmp = TempDir::new().unwrap();
        let (label, content) = read_previous_report(tmp.path()).await;
        assert_eq!(label, "none");
        assert!(content.is_empty());

        let reports = tmp.path().join("reports");
        tokio::fs::create_dir_all(&reports).await.unwrap();
        tokio::fs::write(reports.join("evolution-2026-01-01.md"), "jan")
            .await
            .unwrap();
        tokio::fs::write(reports.join("evolution-2026-03-05.md"), "mar")
            .await
            .unwrap();
        let (label, content) = read_previous_report(tmp.path()).await;
        assert_eq!(label, "evolution-2026-03-05.md");
        assert_eq!(content, "mar");
    }

    // ── K2: evolution-report link upsert into MEMORY.md ──

    #[tokio::test]
    async fn report_link_upsert_creates_memory_md_when_missing() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();

        upsert_report_link_in_memory_md(&memory_dir, "2026-07-09")
            .await
            .expect("upsert into fresh dir");

        let index = tokio::fs::read_to_string(memory_dir.join("MEMORY.md"))
            .await
            .unwrap();
        assert_eq!(
            index,
            "- [进化报告 2026-07-09](reports/evolution-2026-07-09.md) — 做梦与想象的自我进化结论\n"
        );
    }

    #[tokio::test]
    async fn report_link_upsert_replaces_existing_line_without_accumulating() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        // Pre-seed: an index with an OLD report link sandwiched between user
        // content, plus a stray duplicate (corrupt older state).
        tokio::fs::write(
            memory_dir.join("MEMORY.md"),
            "# Memory Index\n\
             - [进化报告 2020-01-01](reports/evolution-2020-01-01.md) — 做梦与想象的自我进化结论\n\
             - [用户偏好](user_pref.md) — 偏好暗色主题\n\
             - [进化报告 2020-02-02](reports/evolution-2020-02-02.md) — 做梦与想象的自我进化结论\n",
        )
        .await
        .unwrap();

        upsert_report_link_in_memory_md(&memory_dir, "2026-07-09")
            .await
            .expect("upsert over existing index");

        let index = tokio::fs::read_to_string(memory_dir.join("MEMORY.md"))
            .await
            .unwrap();
        // The old link is replaced IN PLACE (position kept), the stray
        // duplicate is dropped, surrounding lines survive verbatim.
        assert_eq!(
            index,
            "# Memory Index\n\
             - [进化报告 2026-07-09](reports/evolution-2026-07-09.md) — 做梦与想象的自我进化结论\n\
             - [用户偏好](user_pref.md) — 偏好暗色主题\n"
        );
        assert_eq!(
            index.matches("- [进化报告 ").count(),
            1,
            "never accumulates"
        );

        // Idempotent rerun: same date → identical content (skip-write path).
        upsert_report_link_in_memory_md(&memory_dir, "2026-07-09")
            .await
            .expect("idempotent rerun");
        let again = tokio::fs::read_to_string(memory_dir.join("MEMORY.md"))
            .await
            .unwrap();
        assert_eq!(again, index);
    }

    #[tokio::test]
    async fn evolution_report_upserts_latest_link_into_memory_md() {
        // End-to-end through generate_evolution_report: the report write and
        // the MEMORY.md link land together.
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();

        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts(
                "evolution-report",
                vec!["## 用户习惯归纳\n- 无".to_string()],
            )
            .await;

        let path = processor
            .generate_evolution_report(
                &memory_dir,
                None,
                LlmCallParams::default(),
                crate::output_language::MemoryOutputLanguage::Zh,
            )
            .await
            .expect("report generation");
        let report_name = path.file_name().unwrap().to_str().unwrap().to_string();

        let index = tokio::fs::read_to_string(memory_dir.join("MEMORY.md"))
            .await
            .expect("MEMORY.md created by the upsert");
        assert!(
            index.contains(&format!("(reports/{report_name})")),
            "index must link the freshly-written report: {index}"
        );
        assert!(index.contains("- [进化报告 "));
        assert!(index.contains("做梦与想象的自我进化结论"));
    }

    fn sample_hypothesis() -> ImaginationHypothesis {
        ImaginationHypothesis {
            statement: "Users prefer markdown over JSON for in-app docs.".to_string(),
            atoms: vec![
                "Users prefer markdown.".to_string(),
                "JSON is dispreferred for in-app docs.".to_string(),
            ],
            evidence_refs: vec![ImaginationEvidenceRef {
                id: "sess-1".to_string(),
                snippet: "user says: markdown reads nicer than json".to_string(),
            }],
            context: "Recent dev preferences telemetry".to_string(),
        }
    }

    // ── Gate ──
    #[tokio::test]
    async fn gate_always_triggers_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = ImaginationGate::new();
        let decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        assert!(decision.should_trigger);
        let payload = decision.payload.unwrap();
        assert_eq!(
            payload.review_queue_dir,
            memory_dir.join("imagination").join("review-queue")
        );
    }

    #[tokio::test]
    async fn gate_skips_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let gate = ImaginationGate::new();
        let decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: tmp.path().to_path_buf(),
                enabled: false,
            })
            .await
            .unwrap();
        assert!(!decision.should_trigger);
        assert_eq!(decision.skip_reason.as_deref(), Some("feature_disabled"));
    }

    // ── L1 parser ──
    #[test]
    fn l1_parser_extracts_plausibility() {
        let raw = r#"{"plausibility": 0.75, "reasoning": "ok"}"#;
        assert_eq!(parse_l1_json(raw), Some(0.75));
    }

    #[test]
    fn l1_parser_handles_fenced_output() {
        let raw = "```json\n{\"plausibility\":0.3,\"reasoning\":\"x\"}\n```";
        assert_eq!(parse_l1_json(raw), Some(0.3));
    }

    #[test]
    fn l1_parser_clamps_out_of_range() {
        // Caller could return 1.5 / -0.1; parser clamps to [0, 1].
        let raw = r#"{"plausibility": 1.5, "reasoning": "x"}"#;
        assert_eq!(parse_l1_json(raw), Some(1.0));
        let raw_low = r#"{"plausibility": -0.1, "reasoning": "x"}"#;
        assert_eq!(parse_l1_json(raw_low), Some(0.0));
    }

    #[test]
    fn l1_parser_returns_none_on_invalid() {
        assert!(parse_l1_json("not json").is_none());
        assert!(parse_l1_json(r#"{"other":1.0}"#).is_none());
    }

    // ── L2 parser ──
    #[test]
    fn l2_parser_extracts_four_dimensions() {
        let raw = r#"{"novelty":0.8,"consistency":0.6,"groundedness":0.4,"actionability":0.9,"notes":"x"}"#;
        let s = parse_l2_json(raw).unwrap();
        assert!((s.novelty - 0.8).abs() < 1e-9);
        assert!((s.consistency - 0.6).abs() < 1e-9);
        assert!((s.groundedness - 0.4).abs() < 1e-9);
        assert!((s.actionability - 0.9).abs() < 1e-9);
    }

    #[test]
    fn l2_avg_computes_mean_of_four_dims() {
        let s = FourDimensionScores {
            novelty: 0.4,
            consistency: 0.6,
            groundedness: 0.8,
            actionability: 1.0,
        };
        assert!((s.avg() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn l2_neutral_is_half() {
        let s = FourDimensionScores::neutral();
        assert!((s.avg() - 0.5).abs() < 1e-9);
    }

    // ── L3 parser ──
    #[test]
    fn l3_parser_supported_verdict() {
        let raw = r#"{"verdict":"supported","confidence":0.9,"citing_evidence_ids":["e1","e2"]}"#;
        let v = parse_l3_json(raw).unwrap();
        assert_eq!(v.verdict, AtomVerdictKind::Supported);
        assert!((v.confidence - 0.9).abs() < 1e-9);
        assert_eq!(v.citing_evidence_ids, vec!["e1", "e2"]);
    }

    #[test]
    fn l3_parser_refuted_verdict() {
        let raw = r#"{"verdict":"refuted","confidence":0.6,"citing_evidence_ids":["e1"]}"#;
        let v = parse_l3_json(raw).unwrap();
        assert_eq!(v.verdict, AtomVerdictKind::Refuted);
    }

    #[test]
    fn l3_parser_inconclusive_verdict() {
        let raw = r#"{"verdict":"inconclusive","confidence":0.0,"citing_evidence_ids":[]}"#;
        let v = parse_l3_json(raw).unwrap();
        assert_eq!(v.verdict, AtomVerdictKind::Inconclusive);
    }

    #[test]
    fn l3_parser_rejects_unknown_verdict() {
        let raw = r#"{"verdict":"maybe","confidence":0.5}"#;
        assert!(parse_l3_json(raw).is_none());
    }

    // ── L3 aggregation ──
    #[test]
    fn l3_aggregate_empty_atoms_is_neutral_half() {
        assert!((aggregate_l3(&[]) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn l3_aggregate_all_supported_high_confidence_near_one() {
        let v = vec![
            AtomVerdict {
                atom: "a".to_string(),
                verdict: AtomVerdictKind::Supported,
                confidence: 1.0,
                citing_evidence_ids: vec![],
            },
            AtomVerdict {
                atom: "b".to_string(),
                verdict: AtomVerdictKind::Supported,
                confidence: 1.0,
                citing_evidence_ids: vec![],
            },
        ];
        assert!((aggregate_l3(&v) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn l3_aggregate_all_refuted_high_confidence_near_zero() {
        let v = vec![AtomVerdict {
            atom: "a".to_string(),
            verdict: AtomVerdictKind::Refuted,
            confidence: 1.0,
            citing_evidence_ids: vec![],
        }];
        assert!((aggregate_l3(&v) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn l3_aggregate_mixed_inconclusive_neutral() {
        let v = vec![AtomVerdict {
            atom: "a".to_string(),
            verdict: AtomVerdictKind::Inconclusive,
            confidence: 1.0,
            citing_evidence_ids: vec![],
        }];
        assert!((aggregate_l3(&v) - 0.5).abs() < 1e-9);
    }

    // ── L4 weighted fusion ──
    #[test]
    fn l4_fusion_weights_sum_to_one() {
        assert!((L1_WEIGHT + L2_WEIGHT + L3_WEIGHT - 1.0).abs() < 1e-9);
    }

    #[test]
    fn l4_fusion_formula_constants_match_doc() {
        // Documented contract: L1*0.3 + L2.avg*0.4 + L3*0.3
        assert!((L1_WEIGHT - 0.3).abs() < 1e-9);
        assert!((L2_WEIGHT - 0.4).abs() < 1e-9);
        assert!((L3_WEIGHT - 0.3).abs() < 1e-9);
    }

    #[test]
    fn l4_fusion_computes_weighted_sum() {
        let l1: f64 = 0.8;
        let l2_avg: f64 = 0.6;
        let l3: f64 = 0.4;
        let expected: f64 = l1 * 0.3 + l2_avg * 0.4 + l3 * 0.3;
        assert!((expected - 0.6).abs() < 1e-9);
    }

    // ── L5 promotion threshold ──
    #[test]
    fn l5_at_promotion_threshold_yields_review_queue_high() {
        let conf = PROMOTION_THRESHOLD; // exactly 0.7
        let v = if conf >= PROMOTION_THRESHOLD {
            PromotionVerdict::ReviewQueueHigh
        } else if conf >= EXPIRE_THRESHOLD {
            PromotionVerdict::Pending
        } else {
            PromotionVerdict::Expired
        };
        assert_eq!(v, PromotionVerdict::ReviewQueueHigh);
    }

    #[test]
    fn l5_just_above_promotion_threshold_is_high() {
        let conf = 0.71;
        let v = if conf >= PROMOTION_THRESHOLD {
            PromotionVerdict::ReviewQueueHigh
        } else if conf >= EXPIRE_THRESHOLD {
            PromotionVerdict::Pending
        } else {
            PromotionVerdict::Expired
        };
        assert_eq!(v, PromotionVerdict::ReviewQueueHigh);
    }

    #[test]
    fn l5_just_below_promotion_threshold_is_pending() {
        let conf = 0.69;
        let v = if conf >= PROMOTION_THRESHOLD {
            PromotionVerdict::ReviewQueueHigh
        } else if conf >= EXPIRE_THRESHOLD {
            PromotionVerdict::Pending
        } else {
            PromotionVerdict::Expired
        };
        assert_eq!(v, PromotionVerdict::Pending);
    }

    #[test]
    fn l5_at_expire_threshold_is_pending_not_expired() {
        let conf = EXPIRE_THRESHOLD; // exactly 0.5 → still pending (≥)
        let v = if conf >= PROMOTION_THRESHOLD {
            PromotionVerdict::ReviewQueueHigh
        } else if conf >= EXPIRE_THRESHOLD {
            PromotionVerdict::Pending
        } else {
            PromotionVerdict::Expired
        };
        assert_eq!(v, PromotionVerdict::Pending);
    }

    #[test]
    fn l5_below_expire_threshold_is_expired() {
        let conf = 0.49;
        let v = if conf >= PROMOTION_THRESHOLD {
            PromotionVerdict::ReviewQueueHigh
        } else if conf >= EXPIRE_THRESHOLD {
            PromotionVerdict::Pending
        } else {
            PromotionVerdict::Expired
        };
        assert_eq!(v, PromotionVerdict::Expired);
    }

    // ── ISO 8601 expiry ──
    #[test]
    fn iso8601_expiry_format_matches_rfc3339_basic() {
        // 2026-05-25 00:00:00 UTC = ms 1779_400_000_000.
        // +14 days = 2026-06-08 00:00:00 UTC.
        let start_ms: u64 = 1_779_408_000_000; // 2026-05-25 02:00:00 UTC approx
        let expiry = iso8601_expiry_from_ms(start_ms);
        // sanity check: looks like an ISO 8601 UTC string.
        assert!(expiry.ends_with('Z'), "expected Z suffix: {}", expiry);
        assert!(expiry.contains('T'), "expected T separator: {}", expiry);
        assert_eq!(
            expiry.len(),
            20,
            "expected RFC 3339 basic length 20: {}",
            expiry
        );
    }

    #[test]
    fn iso8601_expiry_advances_14_days_from_input() {
        // Reference: 2026-01-01 00:00:00 UTC = 1767_225_600_000 ms.
        let start_ms: u64 = 1_767_225_600_000;
        let expiry = iso8601_expiry_from_ms(start_ms);
        // +14d → 2026-01-15.
        assert!(
            expiry.starts_with("2026-01-15T"),
            "expected 2026-01-15 prefix, got {}",
            expiry
        );
    }

    // ── hypothesis hash ──
    #[test]
    fn hypothesis_hash_is_deterministic() {
        let h1 = sample_hypothesis();
        let h2 = sample_hypothesis();
        assert_eq!(hypothesis_hash(&h1), hypothesis_hash(&h2));
    }

    #[test]
    fn hypothesis_hash_differs_for_different_statements() {
        let mut h1 = sample_hypothesis();
        let mut h2 = sample_hypothesis();
        h1.statement = "A".to_string();
        h2.statement = "B".to_string();
        assert_ne!(hypothesis_hash(&h1), hypothesis_hash(&h2));
    }

    #[test]
    fn hypothesis_hash_is_16_hex_chars() {
        let h = sample_hypothesis();
        let hash = hypothesis_hash(&h);
        assert_eq!(hash.len(), 32, "hash: {}", hash);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── render_imagined_md ──
    #[test]
    fn rendered_md_contains_required_frontmatter_fields() {
        let h = sample_hypothesis();
        let scores = FourDimensionScores::neutral();
        let verdicts: Vec<AtomVerdict> = Vec::new();
        let body = render_imagined_md(&h, 0.6, &scores, 0.5, &verdicts, 0.55, "medium", &[]);
        assert!(body.starts_with("---\n"));
        // K2: imagined drafts carry an explicit type so the SE indexer's type
        // mapping can classify them (typeless frontmatter → MissingType skip).
        assert!(body.contains("type: imagined\n"));
        assert!(body.contains("confidence: medium\n"));
        assert!(body.contains("status: pending-review\n"));
        assert!(body.contains("expiry: "));
        assert!(body.contains("final_confidence: 0.5500\n"));
        assert!(body.contains("l1_plausibility: 0.6000\n"));
        assert!(body.contains("l2_novelty: 0.5000\n"));
        assert!(body.contains("l3_atomic_aggregate: 0.5000\n"));
        assert!(body.contains("# Hypothesis\n"));
        assert!(body.contains("Users prefer markdown over JSON"));
    }

    #[test]
    fn rendered_md_includes_atom_verdicts_when_present() {
        let h = sample_hypothesis();
        let scores = FourDimensionScores::neutral();
        let verdicts = vec![AtomVerdict {
            atom: "claim X".to_string(),
            verdict: AtomVerdictKind::Supported,
            confidence: 0.9,
            citing_evidence_ids: vec!["e1".to_string()],
        }];
        let body = render_imagined_md(&h, 0.6, &scores, 0.8, &verdicts, 0.7, "high", &[]);
        assert!(body.contains("# Atomic verdicts"));
        assert!(body.contains("[supported, conf 0.90] claim X"));
    }

    // ── LlmCallEmitter roundtrip ──
    #[tokio::test]
    async fn emitter_records_request_payload() {
        let emitter = Arc::new(RecordingEmitter::new());
        let req = LlmCallRequestPayload {
            req_id: "tier3-imagination-l1-1-0".to_string(),
            tier: MemoryTier::Dream,
            phase: Some("l1".to_string()),
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: "test".to_string(),
            }],
            model_hint: None,
            params: LlmCallParams::default(),
        };
        emitter.emit_request(req.clone()).await;
        let recorded = emitter.recorded().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].req_id, "tier3-imagination-l1-1-0");
    }

    // ── End-to-end via ScriptedEmitter ──
    #[tokio::test]
    async fn full_pipeline_high_confidence_writes_review_queue_high() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;

        // L1 = 0.9, L2 dims all 0.9 → avg 0.9, L3 supported with conf 1.0
        // (1 atom × supported × 1.0 → aggregate 1.0). Final = 0.9*0.3 +
        // 0.9*0.4 + 1.0*0.3 = 0.27 + 0.36 + 0.30 = 0.93 → ReviewQueueHigh.
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.9,"reasoning":"ok"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![r#"{"novelty":0.9,"consistency":0.9,"groundedness":0.9,"actionability":0.9,"notes":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![
                    r#"{"verdict":"supported","confidence":1.0,"citing_evidence_ids":["e1"]}"#
                        .to_string(),
                ],
            )
            .await;

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["single atom".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let payload = gate_decision.payload.unwrap();

        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: payload.clone(),
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process(input).await.unwrap();
        assert!((output.l1_plausibility - 0.9).abs() < 1e-9);
        assert!((output.l2_scores.avg() - 0.9).abs() < 1e-9);
        assert!((output.l3_atomic_aggregate - 1.0).abs() < 1e-9);
        assert!((output.final_confidence - 0.93).abs() < 1e-9);
        assert_eq!(output.verdict, PromotionVerdict::ReviewQueueHigh);
        let path = output.imagined_path.expect("expected write");
        assert!(path.exists());
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(body.contains("confidence: high"));
        assert!(body.contains("status: pending-review"));
    }

    #[tokio::test]
    async fn full_pipeline_low_confidence_expires_no_write() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;

        // L1 = 0.1, L2 dims all 0.1 → avg 0.1, L3 refuted with conf 1.0 →
        // aggregate 0.0. Final = 0.1*0.3 + 0.1*0.4 + 0.0*0.3 = 0.07 → Expired.
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.1,"reasoning":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![r#"{"novelty":0.1,"consistency":0.1,"groundedness":0.1,"actionability":0.1,"notes":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![
                    r#"{"verdict":"refuted","confidence":1.0,"citing_evidence_ids":["e1"]}"#
                        .to_string(),
                ],
            )
            .await;

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["single atom".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let payload = gate_decision.payload.unwrap();
        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: payload,
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process(input).await.unwrap();
        assert_eq!(output.verdict, PromotionVerdict::Expired);
        assert!(output.imagined_path.is_none());
        // No file should exist in the review queue dir.
        let queue = memory_dir.join("imagination").join("review-queue");
        let mut entries = tokio::fs::read_dir(&queue).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
        // W6 6d-1 — 低置信淘汰必须落负知识库（防 Stage-0 重想）。
        let refuted_dir = memory_dir.join("imagination").join("refuted");
        let refuted: Vec<_> = std::fs::read_dir(&refuted_dir)
            .expect("refuted dir must exist")
            .flatten()
            .collect();
        assert_eq!(refuted.len(), 1, "expired hypothesis lands in refuted/");
        let body = std::fs::read_to_string(refuted[0].path()).unwrap();
        assert!(body.contains("status: refuted"));
        assert!(body.contains("single atom") || body.contains("[refuted"));
        // 负知识注入下一轮 Stage-0 语料。
        let inputs = read_synthesis_inputs(&memory_dir).await;
        assert!(
            !inputs.refuted.is_empty(),
            "refuted summaries must feed hypgen corpus"
        );
    }

    // ── D3 (W-MEMORY-LIFECYCLE): all-layers parse fallback → Expired ──

    /// Recursively concatenate every daily-log jsonl under the project state
    /// dir (memory_dir's parent).
    fn read_daily_logs(project_state_dir: &std::path::Path) -> String {
        let logs_root = project_state_dir.join(".memory-rust-derived").join("logs");
        let mut bodies = String::new();
        for entry in walkdir::WalkDir::new(&logs_root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                bodies.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        bodies
    }

    #[tokio::test]
    async fn all_layers_parse_failure_expires_without_write_and_daily_logs() {
        let tmp = TempDir::new().unwrap();
        // memory_dir = <project_state_dir>/memory so the daily log lands in
        // <project_state_dir>/.memory-rust-derived (production layout).
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        // NO scripts installed → every layer receives `{}` → parse fails →
        // L1 0.5 / L2 neutral 0.5 / L3 inconclusive 0.5 → fused exactly 0.5.
        // Pre-D3 that persisted a medium-confidence husk; now it must expire.

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["claim one".to_string(), "claim two".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process(input).await.unwrap();
        assert!(
            (output.final_confidence - 0.5).abs() < 1e-9,
            "premise: pure-fallback fusion is exactly 0.5"
        );
        assert_eq!(
            output.verdict,
            PromotionVerdict::Expired,
            "zero model signal must not persist as Pending"
        );
        assert!(output.imagined_path.is_none());
        let queue = memory_dir.join("imagination").join("review-queue");
        let mut entries = tokio::fs::read_dir(&queue).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none(), "no write");

        // The drop is observable in the daily log.
        let bodies = read_daily_logs(tmp.path());
        assert!(
            bodies.contains(r#""kind":"memory.imagination.all_layers_parse_failed""#),
            "daily log must record the all-layers failure, got: {bodies}"
        );
        assert!(
            bodies.contains("Users prefer markdown"),
            "statement snippet logged"
        );
    }

    #[tokio::test]
    async fn single_parsed_layer_prevents_forced_expiry() {
        // Negative case: L1 parses (0.6) while L2 + L3 fall back → NOT an
        // all-layers failure; the normal L5 banding applies (0.6*0.3 +
        // 0.5*0.4 + 0.5*0.3 = 0.53 → Pending, written).
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.6,"reasoning":"x"}"#.to_string()],
            )
            .await;
        // l2 / l3 unset → `{}` → fallback.

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["a".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process(input).await.unwrap();
        assert_eq!(output.verdict, PromotionVerdict::Pending);
        assert!(
            output.imagined_path.is_some(),
            "partial signal still persists"
        );
        let bodies = read_daily_logs(tmp.path());
        assert!(
            !bodies.contains("all_layers_parse_failed"),
            "no false-positive logging when one layer parsed"
        );
    }

    #[tokio::test]
    async fn full_pipeline_mid_confidence_writes_pending() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;

        // L1=0.6, L2.avg=0.6, L3=0.6 → final = 0.6*1 = 0.6 → Pending.
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.6,"reasoning":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![r#"{"novelty":0.6,"consistency":0.6,"groundedness":0.6,"actionability":0.6,"notes":"x"}"#.to_string()],
            )
            .await;
        // For L3 = 0.6 aggregate via supported+conf 0.2 (atom yields
        // (0.2+1)/2 = 0.6 aggregate).
        scripted
            .set_scripts(
                "l3",
                vec![
                    r#"{"verdict":"supported","confidence":0.2,"citing_evidence_ids":["e1"]}"#
                        .to_string(),
                ],
            )
            .await;

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["a".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process(input).await.unwrap();
        assert_eq!(output.verdict, PromotionVerdict::Pending);
        let path = output.imagined_path.expect("expected write");
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(body.contains("confidence: medium"));
        assert!(body.contains("status: pending-review"));
    }

    // ── deliver_result behavior ──
    #[tokio::test]
    async fn deliver_result_returns_false_for_unknown_req_id() {
        let gate = Arc::new(ImaginationGate::new());
        let emitter = Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>;
        let processor = ImaginationProcessor::new(gate, emitter);
        let delivered = processor
            .deliver_result(LlmCallResultPayload {
                req_id: "unknown".to_string(),
                response: Some("{}".to_string()),
                usage: None,
                error: None,
            })
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn req_id_uses_tier3_imagination_prefix() {
        let gate = Arc::new(ImaginationGate::new());
        let emitter = Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>;
        let processor = ImaginationProcessor::new(gate, emitter);
        let req_id = processor.next_req_id("l1");
        assert!(
            req_id.starts_with("tier3-imagination-l1-"),
            "req_id={}",
            req_id
        );
    }

    // ── Stage-0 hypothesis generation: parser ──
    #[test]
    fn hypgen_parser_extracts_candidates() {
        let raw = r#"{"hypotheses":[
            {"statement":"S1","atoms":["a1","a2"],"evidence_refs":[{"id":"m1","snippet":"q1"}]},
            {"statement":"S2","atoms":["b1"],"evidence_refs":[]}
        ]}"#;
        let out = parse_hypgen_json(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].statement, "S1");
        assert_eq!(out[0].atoms, vec!["a1".to_string(), "a2".to_string()]);
        assert_eq!(out[0].evidence_refs.len(), 1);
        assert_eq!(out[0].evidence_refs[0].id, "m1");
        assert_eq!(out[1].statement, "S2");
        assert_eq!(out[1].atoms, vec!["b1".to_string()]);
        assert!(out[1].evidence_refs.is_empty());
    }

    #[test]
    fn hypgen_parser_handles_fenced_output() {
        let raw = "```json\n{\"hypotheses\":[{\"statement\":\"X\"}]}\n```";
        let out = parse_hypgen_json(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].statement, "X");
        // No atoms supplied → defaults to single atom = the statement.
        assert_eq!(out[0].atoms, vec!["X".to_string()]);
    }

    #[test]
    fn hypgen_parser_bad_json_yields_empty_no_panic() {
        assert!(parse_hypgen_json("not json").is_empty());
        assert!(parse_hypgen_json("").is_empty());
        assert!(parse_hypgen_json("{}").is_empty());
        assert!(parse_hypgen_json(r#"{"hypotheses":"oops"}"#).is_empty());
        // Empty statements skipped.
        assert!(parse_hypgen_json(r#"{"hypotheses":[{"statement":"  "}]}"#).is_empty());
    }

    #[test]
    fn hypgen_parser_skips_evidence_without_id() {
        let raw = r#"{"hypotheses":[{"statement":"S","atoms":["a"],"evidence_refs":[
            {"snippet":"orphan"},
            {"id":"ok","snippet":"good"}
        ]}]}"#;
        let out = parse_hypgen_json(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].evidence_refs.len(), 1);
        assert_eq!(out[0].evidence_refs[0].id, "ok");
    }

    // ── Stage-0 synthesis disk readers ──
    #[tokio::test]
    async fn read_synthesis_inputs_missing_dir_is_empty_no_panic() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let inputs = read_synthesis_inputs(&missing).await;
        assert!(inputs.reflections.is_empty());
        assert!(inputs.dreams.is_empty());
        assert!(inputs.recent_session.is_empty());
        // W6 6d — 新增两段同样 fail-soft 为空。
        assert!(inputs.refuted.is_empty());
        assert!(inputs.meta_review.is_empty());
    }

    #[tokio::test]
    async fn read_synthesis_inputs_reads_and_excludes_index_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // reflection notes (kept)
        tokio::fs::write(dir.join("user_topic.md"), "reflection-about-topic")
            .await
            .unwrap();
        tokio::fs::write(dir.join("another.md"), "second-reflection")
            .await
            .unwrap();
        // index + snapshot files (excluded)
        tokio::fs::write(dir.join("MEMORY.md"), "the-index")
            .await
            .unwrap();
        tokio::fs::write(dir.join("SESSION.md"), "session-current-content")
            .await
            .unwrap();
        tokio::fs::write(dir.join(".session-abc.md"), "snapshot-content")
            .await
            .unwrap();
        // dreams
        let dreams = dir.join("dreams");
        tokio::fs::create_dir_all(&dreams).await.unwrap();
        tokio::fs::write(dreams.join("insight_x.md"), "dream-insight-text")
            .await
            .unwrap();
        tokio::fs::write(dreams.join("fragment_y.md"), "weak-fragment")
            .await
            .unwrap();

        let inputs = read_synthesis_inputs(dir).await;

        assert!(inputs.reflections.contains("reflection-about-topic"));
        assert!(inputs.reflections.contains("second-reflection"));
        assert!(!inputs.reflections.contains("the-index"));
        assert!(!inputs.reflections.contains("session-current-content"));
        assert!(!inputs.reflections.contains("snapshot-content"));

        assert!(inputs.dreams.contains("dream-insight-text"));
        // Insight preferred — fragment is not pulled into the dreams section.
        assert!(!inputs.dreams.contains("weak-fragment"));

        // SESSION.md → recent_session; the .session-* snapshot is the most
        // recently written, so it also appears as a recent snapshot.
        assert!(inputs.recent_session.contains("session-current-content"));
        assert!(inputs.recent_session.contains("snapshot-content"));
    }

    // ── Stage-0 end-to-end via ScriptedEmitter ──
    #[tokio::test]
    async fn generate_hypotheses_includes_disk_corpus_and_parses_candidates() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        tokio::fs::write(dir.join("reflect_one.md"), "KNOWN_REFLECTION_MARKER")
            .await
            .unwrap();
        let dreams = dir.join("dreams");
        tokio::fs::create_dir_all(&dreams).await.unwrap();
        tokio::fs::write(dreams.join("insight_a.md"), "KNOWN_DREAM_MARKER")
            .await
            .unwrap();
        tokio::fs::write(dir.join("SESSION.md"), "KNOWN_SESSION_MARKER")
            .await
            .unwrap();

        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;

        scripted
            .set_scripts(
                "hypgen",
                vec![r#"{"hypotheses":[
                    {"statement":"Markdown beats JSON for docs","atoms":["users prefer markdown"],"evidence_refs":[{"id":"reflect_one.md","snippet":"q"}]}
                ]}"#
                .to_string()],
            )
            .await;

        let generated = processor
            .generate_hypotheses(&dir, None, LlmCallParams::default())
            .await
            .unwrap();

        // (b) parsed candidates
        assert_eq!(generated.hypotheses.len(), 1);
        assert_eq!(
            generated.hypotheses[0].statement,
            "Markdown beats JSON for docs"
        );
        assert_eq!(
            generated.hypotheses[0].atoms,
            vec!["users prefer markdown".to_string()]
        );
        assert!(generated.req_id.starts_with("tier3-imagination-hypgen-"));

        // (a) the emitted generation request carries all three disk sources,
        // with placeholders fully substituted (no leftover `{{...}}`).
        let recorded = scripted.recorded().await;
        assert_eq!(recorded.len(), 1);
        let sys = &recorded[0].messages[0].content;
        assert!(
            sys.contains("KNOWN_REFLECTION_MARKER"),
            "reflections missing"
        );
        assert!(sys.contains("KNOWN_DREAM_MARKER"), "dreams missing");
        assert!(sys.contains("KNOWN_SESSION_MARKER"), "session missing");
        assert!(!sys.contains("{{reflections}}"));
        assert!(!sys.contains("{{dreams}}"));
        assert!(!sys.contains("{{recent_session}}"));
        // W6 6d — 新增两段占位同样必须被替换（空 → 显式占位文案）。
        assert!(!sys.contains("{{refuted}}"));
        assert!(!sys.contains("{{meta_review}}"));
        assert!(sys.contains("(no refuted hypotheses yet)"));
        assert!(sys.contains("(no prior sweeps)"));
        assert_eq!(recorded[0].phase.as_deref(), Some("hypgen"));
    }

    // ── W-MEMORY-SYNERGY W6 (2026-07-16) — 负知识 + 元评审 ────────────────

    #[tokio::test]
    async fn w6_refuted_write_read_and_prune_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let mut hypothesis = sample_hypothesis();
        hypothesis.statement = "用户可能更偏好暗色主题".to_string();
        write_refuted_hypothesis(&memory_dir, &hypothesis, 0.21, &[]).await;
        // 幂等：同一假设（同 hash）重复写不产生第二个文件。
        write_refuted_hypothesis(&memory_dir, &hypothesis, 0.21, &[]).await;
        let dir = memory_dir.join("imagination").join("refuted");
        assert_eq!(std::fs::read_dir(&dir).unwrap().flatten().count(), 1);

        let summaries = read_refuted_summaries(&memory_dir).await;
        assert!(summaries.contains("- 用户可能更偏好暗色主题"));

        // 滚动上限：塞满 REFUTED_MAX_FILES + 5 个文件后收敛到上限。
        for index in 0..(REFUTED_MAX_FILES + 5) {
            std::fs::write(
                dir.join(format!("refuted_extra_{index}.md")),
                format!("---\ntype: imagined\nstatus: refuted\n---\nextra {index}\n"),
            )
            .unwrap();
        }
        prune_refuted_dir(&dir).await;
        assert!(
            std::fs::read_dir(&dir).unwrap().flatten().count() <= REFUTED_MAX_FILES,
            "refuted dir must stay capped"
        );
    }

    #[tokio::test]
    async fn w6_meta_review_appends_caps_and_renders_tail() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        for index in 0..(META_REVIEW_MAX_LINES + 10) {
            append_meta_review(
                &memory_dir,
                &SweepMetaReview {
                    ts_ms: index as u64,
                    candidates: 5,
                    queued_high: 1,
                    queued_medium: 1,
                    expired: 3,
                },
            )
            .await;
        }
        let raw =
            std::fs::read_to_string(memory_dir.join("imagination").join(META_REVIEW_FILENAME))
                .unwrap();
        assert_eq!(
            raw.lines().filter(|l| !l.trim().is_empty()).count(),
            META_REVIEW_MAX_LINES,
            "账本滚动截断到上限"
        );
        let tail = read_meta_review_tail(&memory_dir).await;
        assert_eq!(tail.lines().count(), HYPGEN_META_REVIEW_TAIL);
        assert!(tail.contains("5 candidates"));
        assert!(tail.contains("3 expired"));
    }

    #[tokio::test]
    async fn generate_hypotheses_bad_llm_json_yields_empty_no_panic() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts("hypgen", vec!["this is not json at all".to_string()])
            .await;

        let generated = processor
            .generate_hypotheses(&dir, None, LlmCallParams::default())
            .await
            .unwrap();
        assert!(generated.hypotheses.is_empty());
    }

    #[tokio::test]
    async fn process_generated_runs_pipeline_per_candidate() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        tokio::fs::write(memory_dir.join("reflect.md"), "some reflection")
            .await
            .unwrap();

        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;

        // Stage-0: one candidate with a single atom.
        scripted
            .set_scripts(
                "hypgen",
                vec![r#"{"hypotheses":[{"statement":"Generated hyp","atoms":["claim"],"evidence_refs":[{"id":"reflect.md","snippet":"q"}]}]}"#.to_string()],
            )
            .await;
        // L1-L5 → high confidence (mirrors full_pipeline_high_confidence test).
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.9,"reasoning":"ok"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![r#"{"novelty":0.9,"consistency":0.9,"groundedness":0.9,"actionability":0.9,"notes":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![r#"{"verdict":"supported","confidence":1.0,"citing_evidence_ids":["reflect.md"]}"#.to_string()],
            )
            .await;

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationGeneratedInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process_generated(input).await.unwrap();
        assert!(output
            .generation_req_id
            .starts_with("tier3-imagination-hypgen-"));
        assert_eq!(output.outputs.len(), 1);
        let per = &output.outputs[0];
        assert_eq!(per.verdict, PromotionVerdict::ReviewQueueHigh);
        let path = per.imagined_path.clone().expect("expected write");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn process_generated_empty_generation_yields_no_outputs() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts("hypgen", vec![r#"{"hypotheses":[]}"#.to_string()])
            .await;

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationGeneratedInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };
        let output = processor.process_generated(input).await.unwrap();
        assert!(output.outputs.is_empty());
    }

    // ── 8c (W-MEMORY-HYPGEN-VARIANT-WIRE) — 变体选择 + 创建时 verdict 记账 ──

    #[test]
    fn verdict_outcome_maps_high_win_expired_loss_pending_neutral() {
        assert_eq!(
            verdict_outcome(PromotionVerdict::ReviewQueueHigh),
            Some(true)
        );
        assert_eq!(verdict_outcome(PromotionVerdict::Expired), Some(false));
        assert_eq!(verdict_outcome(PromotionVerdict::Pending), None);
    }

    #[test]
    fn build_hypgen_messages_appends_variant_addendum_to_system() {
        let inputs = SynthesisInputs::default();
        // v0 空 addendum → system 就是基线（无追加尾巴）。
        let base = build_hypgen_messages(&inputs, "");
        assert_eq!(base[0].role, "system");
        let base_sys = base[0].content.clone();
        assert!(
            !base_sys.contains("TESTMARKER-XYZ"),
            "基线不含 addendum 标记"
        );
        // 非空 addendum → 追加到 system 末尾，基线前缀不变。
        let addendum = "\nStyle emphasis for this sweep: TESTMARKER-XYZ.";
        let with = build_hypgen_messages(&inputs, addendum);
        assert!(
            with[0].content.starts_with(base_sys.as_str()),
            "基线 system 前缀必须保持"
        );
        assert!(
            with[0].content.ends_with(addendum),
            "addendum 必须追加到 system 末尾"
        );
        // user 消息不受影响。
        assert_eq!(base[1].content, with[1].content);
    }

    #[tokio::test]
    async fn process_generated_records_win_verdict_to_hypgen_variant() {
        let tmp = TempDir::new().unwrap();
        // psd = memory_dir.parent() = tmp（隔离），archive 落 tmp 内，防跨测试串键。
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        tokio::fs::write(memory_dir.join("reflect.md"), "some reflection")
            .await
            .unwrap();

        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts(
                "hypgen",
                vec![r#"{"hypotheses":[{"statement":"Generated hyp","atoms":["claim"],"evidence_refs":[{"id":"reflect.md","snippet":"q"}]}]}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.9,"reasoning":"ok"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![r#"{"novelty":0.9,"consistency":0.9,"groundedness":0.9,"actionability":0.9,"notes":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![r#"{"verdict":"supported","confidence":1.0,"citing_evidence_ids":["reflect.md"]}"#.to_string()],
            )
            .await;

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationGeneratedInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };
        let output = processor.process_generated(input).await.unwrap();
        assert_eq!(output.outputs.len(), 1);
        assert_eq!(output.outputs[0].verdict, PromotionVerdict::ReviewQueueHigh);

        // 当选 hypgen 变体记 1 胜 0 负（聚合到 hypgen/* 键，稳健于 UCB1 tie-break）。
        let psd = crate::dream_gate::project_state_dir_from_memory_dir(&memory_dir);
        let archive = crate::evolution::variants::load_archive(&psd);
        let (wins, losses) = archive
            .stats
            .iter()
            .filter(|(k, _)| k.starts_with("hypgen/"))
            .fold((0u64, 0u64), |(w, l), (_, s)| (w + s.wins, l + s.losses));
        assert_eq!((wins, losses), (1, 0), "High verdict → 记 1 胜 0 负");
    }

    #[tokio::test]
    async fn process_generated_records_loss_verdict_to_hypgen_variant() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        tokio::fs::write(memory_dir.join("reflect.md"), "some reflection")
            .await
            .unwrap();

        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        scripted
            .set_scripts(
                "hypgen",
                vec![r#"{"hypotheses":[{"statement":"Weak hyp","atoms":["claim"],"evidence_refs":[]}]}"#.to_string()],
            )
            .await;
        // 全层解析成功但低分 → final_confidence 远 < 0.5 → genuine Expired
        // （非「全层回退」零信号，故记一负）。
        scripted
            .set_scripts(
                "l1",
                vec![r#"{"plausibility":0.05,"reasoning":"weak"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![r#"{"novelty":0.05,"consistency":0.05,"groundedness":0.05,"actionability":0.05,"notes":"x"}"#.to_string()],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![
                    r#"{"verdict":"refuted","confidence":1.0,"citing_evidence_ids":[]}"#
                        .to_string(),
                ],
            )
            .await;

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationGeneratedInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };
        let output = processor.process_generated(input).await.unwrap();
        assert_eq!(output.outputs.len(), 1);
        assert_eq!(output.outputs[0].verdict, PromotionVerdict::Expired);
        assert!(
            output.outputs[0].imagined_path.is_none(),
            "Expired 不写 review-queue"
        );

        let psd = crate::dream_gate::project_state_dir_from_memory_dir(&memory_dir);
        let archive = crate::evolution::variants::load_archive(&psd);
        let (wins, losses) = archive
            .stats
            .iter()
            .filter(|(k, _)| k.starts_with("hypgen/"))
            .fold((0u64, 0u64), |(w, l), (_, s)| (w + s.wins, l + s.losses));
        assert_eq!((wins, losses), (0, 1), "genuine Expired → 记 0 胜 1 负");
    }

    // ── TierGate trait impl shape ──
    #[test]
    fn imagination_gate_implements_tier_gate_trait() {
        // Compile-time check: this only compiles if `ImaginationGate` satisfies
        // `TierGate` with the declared input/output/error associated types.
        fn assert_impl<T: TierGate>(_: &T) {}
        let gate = ImaginationGate::new();
        assert_impl(&gate);
    }

    // ── W-MEMORY-EVOLUTION PR-7b: tool evidence-gathering channel ──

    fn sample_tool_calls() -> Vec<ToolCall> {
        vec![
            ToolCall {
                kind: ToolKind::WebSearch,
                query: Some("latest rust release".to_string()),
                url: None,
                id: None,
                path: None,
                root: None,
            },
            ToolCall {
                kind: ToolKind::WebFetch,
                query: None,
                url: Some("https://example.com/rust".to_string()),
                id: None,
                path: None,
                root: None,
            },
        ]
    }

    #[test]
    fn tool_call_wire_shapes_are_camel_and_snake() {
        // ToolCall.kind is camelCase (matches protocol MemoryTierToolKind).
        let call = ToolCall {
            kind: ToolKind::WebSearch,
            query: Some("q".to_string()),
            url: None,
            id: None,
            path: None,
            root: None,
        };
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["kind"], "webSearch");
        assert_eq!(v["query"], "q");
        assert!(v.get("url").is_none(), "url=None must skip");

        // Request payload is snake_case; tier is PascalCase via MemoryTier.
        let req = ToolCallRequestPayload {
            req_id: "tier3-imagination-evidence-1-9".to_string(),
            tier: MemoryTier::Dream,
            calls: vec![call],
        };
        let rv = serde_json::to_value(&req).unwrap();
        assert_eq!(rv["req_id"], "tier3-imagination-evidence-1-9");
        assert_eq!(rv["tier"], "Dream");

        // Result payload is snake_case; evidence carries source_url + ts.
        let res = ToolCallResultPayload {
            req_id: "tier3-imagination-evidence-1-9".to_string(),
            evidence: vec![ToolEvidence {
                source_url: "https://example.com".to_string(),
                fetched_at_ms: 1234,
                content: "body".to_string(),
                title: Some("t".to_string()),
            }],
            error: None,
        };
        let resv = serde_json::to_value(&res).unwrap();
        assert_eq!(resv["evidence"][0]["source_url"], "https://example.com");
        assert_eq!(resv["evidence"][0]["fetched_at_ms"], 1234);
        assert!(resv.get("error").is_none(), "error=None must skip");
        let parsed: ToolCallResultPayload = serde_json::from_value(resv).unwrap();
        assert_eq!(parsed, res);
    }

    #[tokio::test]
    async fn gather_evidence_emits_tool_request_frame() {
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        );
        // Run gather_evidence under a short timeout — we only assert the emit
        // shape; deliver happens in the next test. We don't await the result
        // here; spawn it and inspect the recorder once the emit has occurred.
        let proc = Arc::new(processor);
        let proc_clone = Arc::clone(&proc);
        let handle = tokio::spawn(async move {
            proc_clone
                .gather_evidence("hypothesis under test", sample_tool_calls(), None)
                .await
        });
        // Poll the recorder until the emit lands (emit happens before the await).
        let mut recorded = recorder.recorded().await;
        for _ in 0..100 {
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            recorded = recorder.recorded().await;
        }
        assert_eq!(recorded.len(), 1, "exactly one tool request emitted");
        let req = &recorded[0];
        assert!(req.req_id.starts_with("tier3-imagination-evidence-"));
        assert_eq!(req.tier, MemoryTier::Dream);
        assert_eq!(req.calls.len(), 2);
        assert_eq!(req.calls[0].kind, ToolKind::WebSearch);
        assert_eq!(req.calls[0].query.as_deref(), Some("latest rust release"));
        assert_eq!(req.calls[1].kind, ToolKind::WebFetch);
        assert_eq!(
            req.calls[1].url.as_deref(),
            Some("https://example.com/rust")
        );

        // Deliver a result so the spawned gather_evidence resolves (don't leak).
        let delivered = proc
            .deliver_tool_result(ToolCallResultPayload {
                req_id: req.req_id.clone(),
                evidence: Vec::new(),
                error: None,
            })
            .await;
        assert!(delivered, "deliver matched the pending req_id");
        let evidence = handle.await.unwrap();
        assert!(evidence.is_empty());
    }

    #[tokio::test]
    async fn gather_evidence_resolves_with_delivered_evidence() {
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        ));
        let proc_clone = Arc::clone(&processor);
        let handle = tokio::spawn(async move {
            proc_clone
                .gather_evidence("hyp", sample_tool_calls(), None)
                .await
        });
        // Wait for the emit, grab the req_id, deliver evidence.
        let mut recorded = recorder.recorded().await;
        for _ in 0..100 {
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            recorded = recorder.recorded().await;
        }
        let req_id = recorded[0].req_id.clone();
        let delivered = processor
            .deliver_tool_result(ToolCallResultPayload {
                req_id: req_id.clone(),
                evidence: vec![ToolEvidence {
                    source_url: "https://example.com/a".to_string(),
                    fetched_at_ms: 42,
                    content: "fresh fact".to_string(),
                    title: Some("A".to_string()),
                }],
                error: None,
            })
            .await;
        assert!(delivered);
        let evidence = handle.await.unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].source_url, "https://example.com/a");
        assert_eq!(evidence[0].fetched_at_ms, 42);
        assert_eq!(evidence[0].content, "fresh fact");
    }

    #[tokio::test]
    async fn gather_evidence_empty_calls_returns_empty_without_emit() {
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        );
        let evidence = processor.gather_evidence("hyp", Vec::new(), None).await;
        assert!(evidence.is_empty());
        assert!(
            recorder.recorded().await.is_empty(),
            "no frame emitted for empty calls"
        );
    }

    #[tokio::test]
    async fn gather_evidence_shutdown_fail_soft_returns_empty() {
        // `test-util` (tokio virtual time) is not enabled, so we exercise the
        // fail-soft path via the SHUTDOWN branch (`Ok(Err(_recv_err))`):
        // dropping the pending sender — as the orchestrator would on shutdown —
        // resolves `gather_evidence` to an empty vec without panicking. Mirrors
        // `tier1_session_memory`'s timeout-test strategy (no real 60s wait).
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        ));
        let p2 = Arc::clone(&processor);
        let task =
            tokio::spawn(async move { p2.gather_evidence("hyp", sample_tool_calls(), None).await });

        // Wait for the emit to register the pending oneshot, then drop the
        // sender to emulate orchestrator-side shutdown.
        for _ in 0..100 {
            if !recorder.recorded().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        {
            let mut map = processor.tool_pending.lock().await;
            map.clear();
        }
        let evidence = task.await.unwrap();
        assert!(evidence.is_empty(), "shutdown yields empty evidence");
    }

    // ── K10 (W-MEMORY-LIFECYCLE): watch-scoped readFile / listDir probes ──

    #[test]
    fn watch_wire_shapes_read_file_and_list_dir() {
        // Wire contract: `{id, kind: "readFile"|"listDir", path, root}`;
        // web-kind entries stay `{kind, query|url}` with NO new keys.
        let call = ToolCall {
            kind: ToolKind::ReadFile,
            query: None,
            url: None,
            id: Some("watch-readfile-1".to_string()),
            path: Some("D:/proj/src/main.rs".to_string()),
            root: Some("D:/proj".to_string()),
        };
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["kind"], "readFile");
        assert_eq!(v["id"], "watch-readfile-1");
        assert_eq!(v["path"], "D:/proj/src/main.rs");
        assert_eq!(v["root"], "D:/proj");
        assert!(v.get("query").is_none());
        assert!(v.get("url").is_none());

        let list = ToolCall {
            kind: ToolKind::ListDir,
            query: None,
            url: None,
            id: Some("watch-listdir-1".to_string()),
            path: Some("D:/proj".to_string()),
            root: Some("D:/proj".to_string()),
        };
        assert_eq!(serde_json::to_value(&list).unwrap()["kind"], "listDir");

        // Pre-K10 web frame byte-shape unchanged: no id/path/root keys.
        let web = serde_json::to_value(&sample_tool_calls()[0]).unwrap();
        assert_eq!(web["kind"], "webSearch");
        assert!(web.get("id").is_none());
        assert!(web.get("path").is_none());
        assert!(web.get("root").is_none());

        // Result frame (S2 executor contract): watch results ride the SAME
        // uniform `evidence` channel — readFile → content = file text (an
        // oversized file carries a plain `[truncated: …]` marker); listDir →
        // content = JSON.stringify({entries[, truncated]}). The decoder turns
        // the listDir JSON into a readable listing and leaves everything else
        // byte-identical.
        let mut evidence = vec![
            ToolEvidence {
                source_url: "D:/proj/src/main.rs".to_string(),
                fetched_at_ms: 1,
                content: "fn main() {}\n[truncated: file is 90000 bytes, read limit 65536]"
                    .to_string(),
                title: None,
            },
            ToolEvidence {
                source_url: "D:/proj".to_string(),
                fetched_at_ms: 2,
                content: r#"{"entries":["src/","README.md"],"truncated":true}"#.to_string(),
                title: None,
            },
            ToolEvidence {
                source_url: "https://example.com".to_string(),
                fetched_at_ms: 3,
                content: "web snippet".to_string(),
                title: None,
            },
        ];
        decode_watch_listing_evidence(&mut evidence);
        // readFile text untouched (marker passes through verbatim).
        assert!(evidence[0]
            .content
            .contains("[truncated: file is 90000 bytes"));
        // listDir JSON decoded to names + truncation tail.
        assert_eq!(evidence[1].content, "src/\nREADME.md\n…（清单被截断）");
        // Non-watch evidence untouched.
        assert_eq!(evidence[2].content, "web snippet");

        // Empty-entries listing decodes to an explicit placeholder.
        let mut empty = vec![ToolEvidence {
            source_url: "D:/proj".to_string(),
            fetched_at_ms: 4,
            content: r#"{"entries":[]}"#.to_string(),
            title: None,
        }];
        decode_watch_listing_evidence(&mut empty);
        assert_eq!(empty[0].content, "(empty directory)");
    }

    #[test]
    fn derive_statement_filenames_extracts_relative_tokens_only() {
        let names = derive_statement_filenames(
            "The loader in src/config.rs mishandles dream-config.json, see \
             https://example.com/x.md and /etc/passwd.txt plus ../../evil.rs \
             and src\\util\\helper.rs.",
        );
        assert_eq!(
            names,
            vec![
                "src/config.rs".to_string(),
                "dream-config.json".to_string(),
                "src/util/helper.rs".to_string(),
            ],
            "relative filename tokens only — URLs / absolute / traversal rejected"
        );
        assert!(derive_statement_filenames("no filenames here at all").is_empty());
    }

    #[tokio::test]
    async fn gather_evidence_with_watch_context_emits_probe_calls() {
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        ));
        let watch = WatchContext {
            root: PathBuf::from("D:/watched"),
            focus: Some("架构演进".to_string()),
        };
        let proc_clone = Arc::clone(&processor);
        let watch_clone = watch.clone();
        let handle = tokio::spawn(async move {
            proc_clone
                .gather_evidence(
                    "check src/config.rs and notes.md for drift",
                    sample_tool_calls(),
                    Some(&watch_clone),
                )
                .await
        });

        let mut recorded = recorder.recorded().await;
        for _ in 0..100 {
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            recorded = recorder.recorded().await;
        }
        let req = &recorded[0];
        // 2 web calls + 1 listDir + 2 readFile = 5 (≤ the 8/run budget).
        assert_eq!(req.calls.len(), 5, "calls: {:?}", req.calls);
        assert_eq!(req.calls[0].kind, ToolKind::WebSearch);
        assert_eq!(req.calls[1].kind, ToolKind::WebFetch);
        let listdir = &req.calls[2];
        assert_eq!(listdir.kind, ToolKind::ListDir);
        assert_eq!(listdir.id.as_deref(), Some("watch-listdir-1"));
        assert_eq!(listdir.root.as_deref(), Some("D:/watched"));
        assert_eq!(listdir.path.as_deref(), Some("D:/watched"));
        let rf1 = &req.calls[3];
        assert_eq!(rf1.kind, ToolKind::ReadFile);
        assert_eq!(rf1.id.as_deref(), Some("watch-readfile-1"));
        assert!(
            rf1.path.as_deref().unwrap().ends_with("config.rs"),
            "path: {:?}",
            rf1.path
        );
        assert_eq!(rf1.root.as_deref(), Some("D:/watched"));
        let rf2 = &req.calls[4];
        assert_eq!(rf2.kind, ToolKind::ReadFile);
        assert!(rf2.path.as_deref().unwrap().ends_with("notes.md"));

        // Deliver watch results in the S2 executor encoding — uniform
        // `evidence` items: readFile → content = file text; listDir →
        // content = JSON.stringify({entries[, truncated]}).
        let delivered = processor
            .deliver_tool_result(ToolCallResultPayload {
                req_id: req.req_id.clone(),
                evidence: vec![
                    ToolEvidence {
                        source_url: "D:/watched".to_string(),
                        fetched_at_ms: 10,
                        content: r#"{"entries":["src/","notes.md"]}"#.to_string(),
                        title: None,
                    },
                    ToolEvidence {
                        source_url: "D:/watched/src/config.rs".to_string(),
                        fetched_at_ms: 11,
                        content: "fn resolve() { /* drift */ }".to_string(),
                        title: None,
                    },
                ],
                error: None,
            })
            .await;
        assert!(delivered);
        let evidence = handle.await.unwrap();
        assert_eq!(evidence.len(), 2, "evidence: {evidence:?}");
        // listDir JSON decoded into a readable name listing for scoring.
        let listing = evidence
            .iter()
            .find(|e| e.source_url == "D:/watched")
            .expect("listDir evidence present");
        assert_eq!(listing.content, "src/\nnotes.md");
        // readFile text passes through verbatim.
        let file = evidence
            .iter()
            .find(|e| e.source_url.ends_with("config.rs"))
            .expect("readFile evidence present");
        assert!(file.content.contains("drift"));
    }

    #[tokio::test]
    async fn gather_evidence_watch_without_filenames_derives_listdir_only() {
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        ));
        let watch = WatchContext {
            root: PathBuf::from("D:/watched"),
            focus: None,
        };
        let proc_clone = Arc::clone(&processor);
        let handle = tokio::spawn(async move {
            proc_clone
                .gather_evidence("no file tokens in this statement", Vec::new(), Some(&watch))
                .await
        });
        let mut recorded = recorder.recorded().await;
        for _ in 0..100 {
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            recorded = recorder.recorded().await;
        }
        // Even with zero web queries, the watch context alone emits (listDir).
        assert_eq!(recorded[0].calls.len(), 1);
        assert_eq!(recorded[0].calls[0].kind, ToolKind::ListDir);
        let delivered = processor
            .deliver_tool_result(ToolCallResultPayload {
                req_id: recorded[0].req_id.clone(),
                evidence: Vec::new(),
                error: None,
            })
            .await;
        assert!(delivered);
        assert!(handle.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gather_evidence_without_watch_context_is_unchanged() {
        // No watch context → identical pre-K10 behavior: same single frame,
        // web-only calls, no id/path/root on the wire.
        let recorder = Arc::new(RecordingToolEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&recorder) as Arc<dyn ToolCallEmitter>,
        ));
        let proc_clone = Arc::clone(&processor);
        let handle = tokio::spawn(async move {
            proc_clone
                .gather_evidence("hyp", sample_tool_calls(), None)
                .await
        });
        let mut recorded = recorder.recorded().await;
        for _ in 0..100 {
            if !recorded.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            recorded = recorder.recorded().await;
        }
        assert_eq!(recorded[0].calls.len(), 2, "web calls only");
        let frame = serde_json::to_value(&recorded[0]).unwrap();
        for call in frame["calls"].as_array().unwrap() {
            assert!(call.get("id").is_none());
            assert!(call.get("path").is_none());
            assert!(call.get("root").is_none());
        }
        let _ = processor
            .deliver_tool_result(ToolCallResultPayload {
                req_id: recorded[0].req_id.clone(),
                evidence: Vec::new(),
                error: None,
            })
            .await;
        assert!(handle.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deliver_tool_result_unknown_req_id_is_noop() {
        let processor = ImaginationProcessor::new(
            Arc::new(ImaginationGate::new()),
            Arc::new(RecordingEmitter::new()) as Arc<dyn LlmCallEmitter>,
        );
        let delivered = processor
            .deliver_tool_result(ToolCallResultPayload {
                req_id: "no-such-req".to_string(),
                evidence: Vec::new(),
                error: None,
            })
            .await;
        assert!(!delivered, "unknown req_id is a no-op");
    }

    // ── W-MEMORY-EVOLUTION PR-8: external evidence → scoring + traceable md ──

    fn sample_external_evidence() -> Vec<ToolEvidence> {
        vec![
            ToolEvidence {
                source_url: "https://example.org/fresh".to_string(),
                fetched_at_ms: 1_700_000_000_000,
                content: "Authoritative fresh fact backing the hypothesis.".to_string(),
                title: Some("Fresh Source".to_string()),
            },
            ToolEvidence {
                source_url: "https://example.org/second".to_string(),
                fetched_at_ms: 1_700_000_000_999,
                content: "A second corroborating external snippet.".to_string(),
                title: None,
            },
        ]
    }

    #[test]
    fn derive_evidence_queries_minimal_web_search_for_statement() {
        let h = sample_hypothesis();
        let queries = derive_evidence_queries(&h);
        assert_eq!(queries.len(), 1, "minimal-start = exactly one web_search");
        assert_eq!(queries[0].kind, ToolKind::WebSearch);
        assert_eq!(queries[0].query.as_deref(), Some(h.statement.trim()));
        assert!(queries[0].url.is_none());
    }

    #[test]
    fn derive_evidence_queries_empty_statement_yields_no_queries() {
        let mut h = sample_hypothesis();
        h.statement = "   ".to_string();
        assert!(derive_evidence_queries(&h).is_empty());
    }

    #[test]
    fn external_evidence_to_refs_uses_source_url_as_id() {
        let refs = external_evidence_to_refs(&sample_external_evidence());
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].id, "https://example.org/fresh");
        assert_eq!(
            refs[0].snippet,
            "Authoritative fresh fact backing the hypothesis."
        );
        assert_eq!(refs[1].id, "https://example.org/second");
    }

    #[test]
    fn external_evidence_to_refs_truncates_long_content() {
        let long = "x".repeat(EVIDENCE_CONTENT_MAX_CHARS + 500);
        let evidence = vec![ToolEvidence {
            source_url: "https://e".to_string(),
            fetched_at_ms: 1,
            content: long,
            title: None,
        }];
        let refs = external_evidence_to_refs(&evidence);
        // Truncated to the cap + the " …" ellipsis marker.
        assert_eq!(
            refs[0].snippet.chars().count(),
            EVIDENCE_CONTENT_MAX_CHARS + 2
        );
        assert!(refs[0].snippet.ends_with(" …"));
    }

    #[test]
    fn build_l2_messages_prefers_external_evidence() {
        let h = sample_hypothesis();
        let external = sample_external_evidence();
        let msgs = build_l2_messages_with_evidence(&h, &external);
        let sys = &msgs[0].content;
        // External source urls + content present; internal ref id absent.
        assert!(sys.contains("https://example.org/fresh"));
        assert!(sys.contains("Authoritative fresh fact"));
        assert!(sys.contains("Fresh Source"), "title prefixed when present");
        assert!(!sys.contains("sess-1"), "internal ref id must be displaced");
    }

    #[test]
    fn build_l2_messages_falls_back_to_internal_refs_when_external_empty() {
        let h = sample_hypothesis();
        let msgs = build_l2_messages_with_evidence(&h, &[]);
        let sys = &msgs[0].content;
        // Pre-PR-8 behavior: internal evidence_refs used verbatim.
        assert!(sys.contains("sess-1"));
        assert!(sys.contains("markdown reads nicer than json"));
    }

    #[test]
    fn rendered_md_includes_evidence_sources_frontmatter() {
        let h = sample_hypothesis();
        let scores = FourDimensionScores::neutral();
        let verdicts: Vec<AtomVerdict> = Vec::new();
        let external = sample_external_evidence();
        let body = render_imagined_md(&h, 0.6, &scores, 0.5, &verdicts, 0.55, "medium", &external);
        assert!(body.contains("evidence_sources:"));
        assert!(body.contains("url: 'https://example.org/fresh'"));
        assert!(body.contains("fetched_at_ms: 1700000000000"));
        assert!(body.contains("url: 'https://example.org/second'"));
        assert!(body.contains("fetched_at_ms: 1700000000999"));
        // Body section also surfaces the gathered evidence for human review.
        assert!(body.contains("# External evidence (gathered)"));
        assert!(body.contains("Authoritative fresh fact"));
    }

    #[test]
    fn rendered_md_omits_evidence_sources_when_external_empty() {
        let h = sample_hypothesis();
        let scores = FourDimensionScores::neutral();
        let verdicts: Vec<AtomVerdict> = Vec::new();
        let body = render_imagined_md(&h, 0.6, &scores, 0.5, &verdicts, 0.55, "medium", &[]);
        assert!(!body.contains("evidence_sources:"));
        assert!(!body.contains("# External evidence"));
    }

    #[tokio::test]
    async fn process_with_external_evidence_feeds_scoring_and_writes_sources() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;

        // Scripted external evidence delivered to gather_evidence.
        scripted.set_tool_evidence(sample_external_evidence()).await;

        // High-confidence scripts → ReviewQueueHigh write.
        scripted
            .set_scripts("l1", vec![r#"{"plausibility":0.9}"#.to_string()])
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![
                    r#"{"novelty":0.9,"consistency":0.9,"groundedness":0.9,"actionability":0.9}"#
                        .to_string(),
                ],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![r#"{"verdict":"supported","confidence":1.0,"citing_evidence_ids":["https://example.org/fresh"]}"#.to_string()],
            )
            .await;

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["single atom".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        let output = processor.process(input).await.unwrap();
        assert_eq!(output.verdict, PromotionVerdict::ReviewQueueHigh);

        // (a) the L2 + L3 emitted messages carried the external evidence
        // content / source_url (proof the external evidence reached scoring).
        let recorded = scripted.recorded().await;
        let l2 = recorded
            .iter()
            .find(|r| r.phase.as_deref() == Some("l2"))
            .expect("l2 call recorded");
        let l2_sys = &l2.messages[0].content;
        assert!(l2_sys.contains("https://example.org/fresh"));
        assert!(l2_sys.contains("Authoritative fresh fact"));
        let l3 = recorded
            .iter()
            .find(|r| r.phase.as_deref() == Some("l3"))
            .expect("l3 call recorded");
        let l3_sys = &l3.messages[0].content;
        assert!(l3_sys.contains("https://example.org/fresh"));
        assert!(
            !l3_sys.contains("sess-1"),
            "external evidence displaces internal refs in L3"
        );

        // (b) the written review-queue md frontmatter carries evidence_sources.
        let path = output.imagined_path.expect("expected write");
        let md = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(md.contains("evidence_sources:"));
        assert!(md.contains("url: 'https://example.org/fresh'"));
        assert!(md.contains("fetched_at_ms: 1700000000000"));
    }

    #[tokio::test]
    async fn process_without_external_evidence_falls_back_no_sources_no_panic() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().to_path_buf();
        let gate = Arc::new(ImaginationGate::new());
        let scripted = Arc::new(ScriptedEmitter::new());
        let processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            Arc::clone(&gate),
            Arc::clone(&scripted) as Arc<dyn LlmCallEmitter>,
            Arc::clone(&scripted) as Arc<dyn ToolCallEmitter>,
        ));
        scripted.bind_processor(Arc::clone(&processor)).await;
        // No set_tool_evidence → gather_evidence resolves to an empty vec
        // (fail-soft, mirrors no-tools / timeout in production).

        scripted
            .set_scripts("l1", vec![r#"{"plausibility":0.6}"#.to_string()])
            .await;
        scripted
            .set_scripts(
                "l2",
                vec![
                    r#"{"novelty":0.6,"consistency":0.6,"groundedness":0.6,"actionability":0.6}"#
                        .to_string(),
                ],
            )
            .await;
        scripted
            .set_scripts(
                "l3",
                vec![
                    r#"{"verdict":"supported","confidence":0.2,"citing_evidence_ids":["sess-1"]}"#
                        .to_string(),
                ],
            )
            .await;

        let mut hypothesis = sample_hypothesis();
        hypothesis.atoms = vec!["a".to_string()];

        let gate_decision = gate
            .evaluate_gate(ImaginationGateInput {
                memory_dir: memory_dir.clone(),
                enabled: true,
            })
            .await
            .unwrap();
        let input = ImaginationProcessInput {
            memory_dir: memory_dir.clone(),
            gate_payload: gate_decision.payload.unwrap(),
            hypothesis,
            model_hint: None,
            params: LlmCallParams::default(),
            watch_context: None,
        };

        // Must not panic, must complete L1-L5.
        let output = processor.process(input).await.unwrap();
        assert_eq!(output.verdict, PromotionVerdict::Pending);

        // L3 fell back to internal refs (sess-1 present, no external url).
        let recorded = scripted.recorded().await;
        let l3 = recorded
            .iter()
            .find(|r| r.phase.as_deref() == Some("l3"))
            .expect("l3 call recorded");
        assert!(l3.messages[0].content.contains("sess-1"));

        // md written but no evidence_sources frontmatter (external empty).
        let path = output.imagined_path.expect("expected write");
        let md = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!md.contains("evidence_sources:"));
        assert!(!md.contains("# External evidence"));
    }
}
