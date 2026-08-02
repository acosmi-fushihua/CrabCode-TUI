//! W-MEMORY-DREAM-REBUILD v7 P3.1 — Tier1/2/3 policy 共用基础设施。
//!
//! Phase 3 起手 PR：立 Rust 共用基础设施 + 反向 IPC LLM 调用契约。
//! **本 PR 不实施具体 Tier policy**（留 P3.2 SessionMemory / P3.3
//! ExtractMemories / P3.4 AutoDream / P3.5 Imagination）。
//!
//! # 架构契约（CLAUDE.md §硬约束 #15 第 5 条）
//!
//! 反思 / 做梦 / 想象的**调度 + 数据管理 + 锁 + 分级 + 写盘隔离**在 Rust
//! (本模块及未来 P3.2-P3.5 实现)；**LLM 调用本身在 TS 业务侧**（反向 IPC：
//! orchestrator emit `memory/tier/llmCallRequest` notification → TS 跑 SDK →
//! `memory/tier/llmCallResult` request 回写）。
//!
//! # 反向 IPC 时序
//!
//! ```text
//! orchestrator (Rust)                TUI client (Rust)              TS Business
//!     │                                  │                              │
//!     │ ─── direct UDS notify ─────────► │                              │
//!     │   memory.tier.llm_call_request   │                              │
//!     │                                  │ ── broadcast notification ─► │
//!     │                                  │   memory/tier/llmCallRequest │
//!     │                                  │                              │
//!     │                                  │                              │  (SDK call,
//!     │                                  │                              │   fork agent,
//!     │                                  │                              │   cache-safe)
//!     │                                  │ ◄── request ───────────────  │
//!     │                                  │  memory/tier/llmCallResult   │
//!     │                                  │                              │
//!     │ ◄── direct UDS request ───────── │                              │
//!     │   memory.tier.llm_call_result    │                              │
//!     │                                  │                              │
//! ```
//!
//! `req_id` 关联 request/response；orchestrator 内部用 oneshot channel +
//! pending map 等待匹配的 result（具体实现归 P3.2-P3.5 Tier policy）。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25) — Tier-1 SessionMemory policy.
// Implements `TierGate` trait + reverse IPC LLM driver + SESSION.md writer.
pub mod tier1_session_memory;

// W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 ExtractMemories policy.
// Implements TierGate trait + 6 gates + reverse IPC LLM driver +
// two-step write + adapter::parse_ts_memory + dedup_hash::memory_body_hash.
pub mod tier2_extract_memories;

// W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 AutoDream policy.
// Implements TierGate trait + 4 gates (24h interval / 10min scan throttle /
// session count >=5 / PID lock) + 5-phase pipeline (Phase 0 Self-RAG
// reflection + Phase 1 Orient + Phase 2 Gather + Phase 3 Consolidate +
// Phase 4 Prune) over reverse IPC LLM driver. Phase 5 Imagination stays
// out of scope (P3.5).
pub mod tier3_auto_dream;

// W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination policy
// (5-layer confidence pipeline). Independent processor (not a phase of
// tier3_auto_dream); invoked via `memory.tier3.imagination.process` IPC.
// L1 Self-RAG + L2 four-dimension + L3 atomic verify + L4 weighted fusion +
// L5 promotion threshold over reverse IPC LLM driver. Writes to
// `imagination/review-queue/imagined_<hash>.md`; promotion requires manual
// confirm (P5.3 UI). Phase 3 closeout PR.
pub mod tier3_imagination;

/// 文件分级 3 级（借鉴 CrabClaw Tier1/Tier2/Tier3 设计；Rust 端重写）。
///
/// * `Session` — Tier-1: 会话级（per-LLM-sample）—— SESSION.md / .session-*.md
/// * `Extract` — Tier-2: 提取级（per-query）—— MEMORY.md +
///   user/feedback/project/reference *.md
/// * `Dream`   — Tier-3: 做梦级（per-24h）—— dreams/insight_*.md +
///   dreams/imagined_*.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum MemoryTier {
    /// Tier-1: 会话级（per-LLM-sample）—— SESSION.md / .session-*.md
    Session,
    /// Tier-2: 提取级（per-query）—— MEMORY.md + user/feedback/project/reference *.md
    Extract,
    /// Tier-3: 做梦级（per-24h）—— dreams/insight_*.md + dreams/imagined_*.md
    Dream,
}

/// Deterministic 门控决策。Tier policy 在 `evaluate_gate` 返回此结构表达
/// 「是否触发 + 触发 payload + 跳过原因」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDecision<T> {
    /// 是否触发后续 LLM 调用 + 写盘流程。
    pub should_trigger: bool,
    /// 触发时的 payload（如待 LLM 处理的会话切片）。`None` 表示未触发。
    pub payload: Option<T>,
    /// 未触发时的可选跳过原因（如 `"lock_held"` / `"cursor_insufficient"` /
    /// `"feature_disabled"`）；触发时通常 `None`。
    pub skip_reason: Option<String>,
}

/// Tier policy 通用接口。每个 Tier (P3.2 Session / P3.3 Extract / P3.4 Dream /
/// P3.5 Imagination) 实现此 trait 时定义自己的 GateInput / GateOutput / Error
/// 类型。
///
/// 本 trait 不绑定 LLM 调用本身；LLM 通过反向 IPC 出去（详 mod 头）。
#[async_trait]
pub trait TierGate {
    /// 门控输入（如 cursor 位置 + 计数 + KAIROS 信号）。
    type GateInput: Send;
    /// 门控输出（如待 LLM 处理的会话切片）。
    type GateOutput: Send;
    /// 评估错误（IO / 配置 / lock 失败等）。
    type Error: Send + std::fmt::Debug;

    /// deterministic 门控：返回是否触发 + 触发 payload。
    async fn evaluate_gate(
        &self,
        input: Self::GateInput,
    ) -> Result<GateDecision<Self::GateOutput>, Self::Error>;
}

// ──────────────────────────────────────────────────────────────────────────
// 反向 IPC LLM 调用契约
// ──────────────────────────────────────────────────────────────────────────

/// 反向 IPC LLM 调用 request 形态（orchestrator → TS via TUI client
/// notification）。
///
/// 通过 `memory/tier/llmCallRequest` notification 广播；TS 端拿到 `req_id` 后
/// 执行 SDK 调用（具体形态由 TS 业务侧决定，本契约只锁 wire shape），结果
/// 通过 `memory/tier/llmCallResult` request 回写到 orchestrator。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct LlmCallRequestPayload {
    /// 关联 request/response 的唯一 id。orchestrator 端用此 id 在 pending
    /// map 里匹配回写的 result。
    pub req_id: String,
    /// 触发本次 LLM 调用的 Tier。
    pub tier: MemoryTier,
    /// 仅 Tier-3 (Dream) 的多 phase 场景使用（如 `"insight"` / `"imagined"`）；
    /// 其他 Tier 为 `None`。schema-opaque 允许后续扩展不动 wire。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// 待 LLM 处理的消息列表。orchestrator 已按 Tier policy 拼装好（system
    /// prompt 不在此处 —— Tier prompt 模板在 orchestrator 内嵌 Rust 字符串
    /// 常量，详 CLAUDE.md §硬约束 #15 第 8 条）。
    pub messages: Vec<LlmMessage>,
    /// 可选 model hint。`None` = TS 端用 mainLoopModel；非空字符串才透传。
    /// **不写品牌字面**（CLAUDE.md §硬约束 #1 模型品牌字面零容忍）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    /// LLM 调用参数。
    pub params: LlmCallParams,
}

/// LLM 消息（role + content）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmMessage {
    /// `"system"` / `"user"` / `"assistant"`。schema-opaque 允许未来扩展。
    pub role: String,
    pub content: String,
}

/// LLM 调用参数。`None` = TS 端使用 SDK 默认。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmCallParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// 反向 IPC LLM 调用 response 形态（TS → orchestrator via TUI client request）。
///
/// 通过 `memory/tier/llmCallResult` request 回写；dispatcher 转发到
/// orchestrator IPC `memory.tier.llm_call_result`，orchestrator 内部用 `req_id`
/// 匹配 pending 的 oneshot channel。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct LlmCallResultPayload {
    /// 与 request 的 `req_id` 一致。
    pub req_id: String,
    /// LLM 返回的文本。`None` = LLM 调用失败（paired with `error`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Token 用量。可选；TS 端 SDK 返回时填充。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsage>,
    /// 失败时的错误信息。成功时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// LLM token 用量。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_tier_serializes_pascal_case() {
        let json_session = serde_json::to_string(&MemoryTier::Session).expect("ser");
        assert_eq!(json_session, "\"Session\"");
        let json_extract = serde_json::to_string(&MemoryTier::Extract).expect("ser");
        assert_eq!(json_extract, "\"Extract\"");
        let json_dream = serde_json::to_string(&MemoryTier::Dream).expect("ser");
        assert_eq!(json_dream, "\"Dream\"");
    }

    #[test]
    fn llm_call_request_payload_roundtrip_snake_case_wire() {
        let payload = LlmCallRequestPayload {
            req_id: "req-1".to_string(),
            tier: MemoryTier::Extract,
            phase: None,
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            model_hint: None,
            params: LlmCallParams {
                // 用 f32 精确表示的小数（0.5）避免 f32→f64 升宽误差被
                // `serde_json::Value` 比较卡住。LLM temperature 实测取值
                // 离散点（0.0 / 0.2 / 0.7 / 1.0），不要求高精度。
                temperature: Some(0.5),
                max_tokens: Some(1024),
            },
        };
        let json = serde_json::to_value(&payload).expect("ser");
        // wire shape 必须 snake_case + 跳过 None 字段
        assert_eq!(json["req_id"], "req-1");
        assert_eq!(json["tier"], "Extract");
        assert!(json.get("phase").is_none(), "phase=None must skip");
        assert!(
            json.get("model_hint").is_none(),
            "model_hint=None must skip"
        );
        assert_eq!(json["params"]["temperature"], 0.5);
        assert_eq!(json["params"]["max_tokens"], 1024);
        let parsed: LlmCallRequestPayload = serde_json::from_value(json).expect("de");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn llm_call_result_payload_roundtrip_with_error() {
        let payload = LlmCallResultPayload {
            req_id: "req-2".to_string(),
            response: None,
            usage: None,
            error: Some("rate limited".to_string()),
        };
        let json = serde_json::to_value(&payload).expect("ser");
        assert_eq!(json["req_id"], "req-2");
        assert!(json.get("response").is_none(), "response=None must skip");
        assert!(json.get("usage").is_none(), "usage=None must skip");
        assert_eq!(json["error"], "rate limited");
        let parsed: LlmCallResultPayload = serde_json::from_value(json).expect("de");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn llm_call_result_payload_roundtrip_success() {
        let payload = LlmCallResultPayload {
            req_id: "req-3".to_string(),
            response: Some("ok".to_string()),
            usage: Some(LlmUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
            error: None,
        };
        let json = serde_json::to_value(&payload).expect("ser");
        assert_eq!(json["response"], "ok");
        assert_eq!(json["usage"]["input_tokens"], 12);
        assert_eq!(json["usage"]["output_tokens"], 34);
        assert!(json.get("error").is_none(), "error=None must skip");
        let parsed: LlmCallResultPayload = serde_json::from_value(json).expect("de");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn gate_decision_with_payload_serializes() {
        let decision: GateDecision<String> = GateDecision {
            should_trigger: true,
            payload: Some("session-slice".to_string()),
            skip_reason: None,
        };
        let json = serde_json::to_value(&decision).expect("ser");
        assert_eq!(json["should_trigger"], true);
        assert_eq!(json["payload"], "session-slice");
    }
}
