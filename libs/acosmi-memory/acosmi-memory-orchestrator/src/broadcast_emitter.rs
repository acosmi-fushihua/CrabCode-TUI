//! W-MEMORY-EVOLUTION PR-3 (2026-05-29) — 真 BroadcastEmitter：把 Tier policy
//! 的反向 IPC LLM / Embedding 调用请求经 `EventSink` push 给 TUI client pump，
//! 由 pump 翻成 `ServerNotification` 广播给 TS 业务侧。
//!
//! # 背景
//!
//! PR-1 建了 events 长连传输地基（`EventSink::push_frame`）；PR-3 在
//! orchestrator 侧把 5 个 Tier processor（tier1 / tier2 / tier3-dream /
//! tier3-imagination）+ SE 的 emitter 从 `RecordingEmitter`（内存空转）替换为
//! 本 `UdsBroadcastEmitter`，真正把请求推上长连。
//!
//! # Wire 形态
//!
//! 每帧 = `{"notification": <name>, "payload": <serde 序列化的 request struct>}`。
//! payload 用 serde 序列化 `LlmCallRequestPayload` / `EmbeddingRequestPayload`
//! （snake_case wire，与 `tier::mod` / `se_integration` 定义一致）。TUI client
//! 侧 `memory_events_pump::classify_frame` 按 `notification` 名翻成
//! `ServerNotification::MemoryTierLlmCallRequest` /
//! `ServerNotification::MemoryTierEmbeddingRequest`，再 `Broadcast`。
//!
//! # 与 4 个 per-tier `LlmCallEmitter` trait 的关系
//!
//! 4 个 per-tier trait（tier1 / tier2 / tier3_auto / tier3_imagination）签名
//! 完全一致（`async fn emit_request(&self, request: LlmCallRequestPayload)`），
//! 但是各自独立的 trait（每个 tier 文件重复定义以保持 tier 模块解耦）。本
//! emitter 对全部 4 个 trait + SE 的 `EmbeddingEmitter` trait 逐个 impl；实现
//! 体一致（payload 形态相同）。

#[cfg(unix)]
use std::sync::Arc;

// `async_trait` is needed cross-platform: the gate-skip emitter trait + mock
// (W-MEMORY-EVOLUTION PR-10) are not `#[cfg(unix)]`-gated (see the gate-skip
// section below), while the Tier reverse-IPC emitter impls are unix-only.
use async_trait::async_trait;

#[cfg(unix)]
use crate::event_sink::EventSink;
#[cfg(unix)]
use crate::se_integration::{EmbeddingRequestPayload, RerankRequestPayload};
#[cfg(unix)]
use crate::tier::tier3_imagination::{ToolCallEmitter, ToolCallRequestPayload};
#[cfg(unix)]
use crate::tier::LlmCallRequestPayload;

// 4 个 per-tier `LlmCallEmitter` trait（同形，独立定义）+ SE
// `EmbeddingEmitter` trait 的别名导入。
#[cfg(unix)]
use crate::se_integration::EmbeddingEmitter;
#[cfg(unix)]
use crate::tier::tier1_session_memory::LlmCallEmitter as Tier1LlmCallEmitter;
#[cfg(unix)]
use crate::tier::tier2_extract_memories::LlmCallEmitter as Tier2LlmCallEmitter;
#[cfg(unix)]
use crate::tier::tier3_auto_dream::LlmCallEmitter as Tier3LlmCallEmitter;
#[cfg(unix)]
use crate::tier::tier3_imagination::LlmCallEmitter as Tier3ImagLlmCallEmitter;

/// Notification name for the reverse-IPC LLM call request frame.
#[cfg(unix)]
pub const LLM_CALL_REQUEST_NOTIFICATION: &str = "memory/tier/llmCallRequest";
/// Notification name for the reverse-IPC embedding request frame.
#[cfg(unix)]
pub const EMBEDDING_REQUEST_NOTIFICATION: &str = "memory/tier/embeddingRequest";
/// Notification name for the reverse-IPC rerank request frame
/// (W-MEMORY-KB-UPLIFT P1). Matches the TUI client pump
/// `RERANK_REQUEST_NOTIFICATION` + protocol `MemoryTierRerankRequest` +
/// `CONTROL_WORKER_NOTIFICATION_WHITELIST` entry (22→23, CLAUDE.md §硬约束
/// #11 同 PR 修订).
#[cfg(unix)]
pub const RERANK_REQUEST_NOTIFICATION: &str = "memory/tier/rerankRequest";
/// Notification name for the reverse-IPC tool-call (evidence) request frame
/// (W-MEMORY-EVOLUTION PR-7b). Matches the TUI client pump
/// `TOOL_CALL_REQUEST_NOTIFICATION` + protocol `MemoryTierToolCallRequest`.
#[cfg(unix)]
pub const TOOL_CALL_REQUEST_NOTIFICATION: &str = "memory/tier/toolCallRequest";

/// Notification name for the gate-skip frame (W-MEMORY-EVOLUTION PR-10). The
/// orchestrator emits this whenever a Tier gate evaluates as skip (Tier-3
/// periodic dream idle / disabled / gate-declined for now). Matches the
/// TUI client pump `GATE_SKIPPED_NOTIFICATION` + protocol
/// `MemoryGateSkippedNotification` + §11 whitelist entry `memory/gate/skipped`
/// (already present from W-MEMORY-DREAM-REBUILD v7 P5.4; 0 whitelist change).
pub const GATE_SKIPPED_NOTIFICATION: &str = "memory/gate/skipped";

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-EVOLUTION PR-10 (2026-05-29) — gate-skip emit channel.
//
// Unlike the Tier LLM / embedding / tool-call reverse-IPC channels (which are
// `#[cfg(unix)]`-only because they need the real `EventSink` UDS push), the
// gate-skip *abstraction* (trait + payload + recording mock) is cross-platform
// so `IpcHandler` (which has both a `#[cfg(unix)]` `with_event_sink` ctor and a
// cross-platform `default()`) can hold a single `Arc<dyn GateSkipEmitter>`
// field. Production wiring (`with_event_sink`) installs the real
// `UdsBroadcastEmitter` (which pushes a frame); `default()` installs the
// in-memory `RecordingGateSkipEmitter` (records for unit tests / safe in any
// environment without an TUI client events sink).
// ──────────────────────────────────────────────────────────────────────────

/// Gate-skip payload (snake_case wire). The TUI client pump `build_gate_skipped`
/// reads `tier` / `gate_name` / `reason` / `skipped_at_ms` (+ optional
/// `context`) and hand-maps to the protocol `MemoryGateSkippedNotification`
/// (camelCase). Mirrors the protocol struct field set; kept independent of the
/// protocol crate (the orchestrator and TUI client crates do not depend on each
/// other — the wire contract is asserted on both ends by unit tests).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct GateSkipPayload {
    /// Tier discriminator. Stable wire values: `"tier1"` / `"tier2"` /
    /// `"tier3"`.
    pub tier: String,
    /// Specific gate that triggered the skip (e.g. `"idle"` / `"disabled"` /
    /// `"dream_gate"`).
    pub gate_name: String,
    /// Human-readable reason for UI rendering.
    pub reason: String,
    /// Optional engine-level context (opaque at the wire layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    /// Wall-clock millisecond timestamp when the skip decision was made.
    pub skipped_at_ms: i64,
}

/// Emit a gate-skip decision. Production = push a `memory/gate/skipped` frame
/// onto the `EventSink`; tests = record in memory.
#[async_trait]
pub trait GateSkipEmitter: Send + Sync {
    async fn emit_gate_skip(&self, payload: GateSkipPayload);

    /// Downcast hook so tests can recover the concrete
    /// `RecordingGateSkipEmitter` from an `Arc<dyn GateSkipEmitter>`. The
    /// production `UdsBroadcastEmitter` returns `&self` too (its downcast just
    /// fails to `RecordingGateSkipEmitter`, yielding an empty recorded set).
    fn as_any(&self) -> &dyn std::any::Any;
}

/// In-memory recording gate-skip emitter. Default for `IpcHandler::default()` /
/// `new()` and any environment without a hooked-up events sink.
#[derive(Debug, Default, Clone)]
pub struct RecordingGateSkipEmitter {
    inner: std::sync::Arc<tokio::sync::Mutex<Vec<GateSkipPayload>>>,
}

impl RecordingGateSkipEmitter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of recorded gate-skip payloads (test / diagnostic).
    pub async fn recorded(&self) -> Vec<GateSkipPayload> {
        self.inner.lock().await.clone()
    }
}

#[async_trait]
impl GateSkipEmitter for RecordingGateSkipEmitter {
    async fn emit_gate_skip(&self, payload: GateSkipPayload) {
        self.inner.lock().await.push(payload);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Real broadcast emitter: pushes a reverse-IPC request frame onto the
/// `EventSink` long-connection. Shared as `Arc<Self>` so a single emitter can
/// back every Tier processor + SE.
#[cfg(unix)]
pub struct UdsBroadcastEmitter {
    event_sink: Arc<EventSink>,
}

#[cfg(unix)]
impl UdsBroadcastEmitter {
    #[must_use]
    pub fn new(event_sink: Arc<EventSink>) -> Self {
        Self { event_sink }
    }

    /// Push an LLM-call-request frame. `payload` is the serde-serialized
    /// `LlmCallRequestPayload` (snake_case wire). Serialization failure is
    /// logged + dropped (mirrors `EventSink::push_frame` fail-soft contract).
    async fn push_llm_request(&self, request: &LlmCallRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize LlmCallRequestPayload; dropping: {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": LLM_CALL_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    /// Push an embedding-request frame. `payload` is the serde-serialized
    /// `EmbeddingRequestPayload` (snake_case wire).
    async fn push_embedding_request(&self, request: &EmbeddingRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize EmbeddingRequestPayload; dropping: \
                     {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": EMBEDDING_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    /// Push a rerank-request frame (W-MEMORY-KB-UPLIFT P1). `payload` is the
    /// serde-serialized `RerankRequestPayload` (snake_case wire).
    async fn push_rerank_request(&self, request: &RerankRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize RerankRequestPayload; dropping: {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": RERANK_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    /// Push a tool-call-request frame. `payload` is the serde-serialized
    /// `ToolCallRequestPayload`. The TUI client pump `build_tool_call_request`
    /// reads `req_id` / `tier` (PascalCase) / `calls[].{kind, query?, url?}`
    /// (`kind` camelCase: `webSearch` / `webFetch`).
    async fn push_tool_request(&self, request: &ToolCallRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize ToolCallRequestPayload; dropping: \
                     {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": TOOL_CALL_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    /// Push a gate-skip frame (W-MEMORY-EVOLUTION PR-10). The TUI client pump
    /// `build_gate_skipped` reads the snake_case `payload` fields (`tier` /
    /// `gate_name` / `reason` / `skipped_at_ms` / `context?`) and hand-maps to
    /// the camelCase protocol `MemoryGateSkippedNotification`.
    async fn push_gate_skip(&self, payload: &GateSkipPayload) {
        let payload = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize GateSkipPayload; dropping: {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": GATE_SKIPPED_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }
}

#[cfg(unix)]
#[async_trait]
impl GateSkipEmitter for UdsBroadcastEmitter {
    async fn emit_gate_skip(&self, payload: GateSkipPayload) {
        self.push_gate_skip(&payload).await;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// 4 个 per-tier `LlmCallEmitter` trait impl（同形，实现体一致）。

#[cfg(unix)]
#[async_trait]
impl Tier1LlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: LlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(unix)]
#[async_trait]
impl Tier2LlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: LlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(unix)]
#[async_trait]
impl Tier3LlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: LlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(unix)]
#[async_trait]
impl Tier3ImagLlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: LlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

// SE `EmbeddingEmitter` trait impl.

#[cfg(unix)]
#[async_trait]
impl EmbeddingEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: EmbeddingRequestPayload) {
        self.push_embedding_request(&request).await;
    }

    async fn emit_rerank_request(&self, request: RerankRequestPayload) {
        self.push_rerank_request(&request).await;
    }
}

// Tier-3 imagination `ToolCallEmitter` trait impl (W-MEMORY-EVOLUTION PR-7b).

#[cfg(unix)]
#[async_trait]
impl ToolCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: ToolCallRequestPayload) {
        self.push_tool_request(&request).await;
    }
}

// ══════════════════════════════════════════════════════════════════════════
// W-MEMORY-EVOLUTION W11 PR-5 (2026-05-29) — Windows `UdsBroadcastEmitter`
// sibling. A byte-for-byte structural parallel of the `#[cfg(unix)]` emitter +
// its 7 trait impls above. The only platform difference is which `EventSink`
// type the emitter holds (the Windows `EventSink` holds Named Pipe write-halves,
// the Unix one holds UDS write-halves — both expose the same `push_frame` API).
// Removing every `#[cfg(windows)]` item leaves the Unix emitter verbatim.
//
// (The name `UdsBroadcastEmitter` is reused — "Uds" is a slight misnomer on
// Windows, but keeping the name identical lets `ipc_handler::with_event_sink`
// reference one symbol across both platforms; the two definitions are under
// mutually-exclusive cfg gates so there is no clash.)
// ══════════════════════════════════════════════════════════════════════════

// `#[cfg(windows)]` duplicates of the three `#[cfg(unix)]` notification-name
// constants above (lines 60-69). Identical values; separate items so the unix
// consts stay verbatim. `GATE_SKIPPED_NOTIFICATION` is already ungated.
#[cfg(windows)]
const LLM_CALL_REQUEST_NOTIFICATION: &str = "memory/tier/llmCallRequest";
#[cfg(windows)]
const EMBEDDING_REQUEST_NOTIFICATION: &str = "memory/tier/embeddingRequest";
#[cfg(windows)]
const RERANK_REQUEST_NOTIFICATION: &str = "memory/tier/rerankRequest";
#[cfg(windows)]
const TOOL_CALL_REQUEST_NOTIFICATION: &str = "memory/tier/toolCallRequest";

#[cfg(windows)]
use std::sync::Arc as WindowsArc;

#[cfg(windows)]
use crate::event_sink::EventSink as WindowsEventSink;
#[cfg(windows)]
use crate::se_integration::EmbeddingRequestPayload as WindowsEmbeddingRequestPayload;
#[cfg(windows)]
use crate::se_integration::RerankRequestPayload as WindowsRerankRequestPayload;
#[cfg(windows)]
use crate::tier::tier3_imagination::{
    ToolCallEmitter as WindowsToolCallEmitter,
    ToolCallRequestPayload as WindowsToolCallRequestPayload,
};
#[cfg(windows)]
use crate::tier::LlmCallRequestPayload as WindowsLlmCallRequestPayload;

#[cfg(windows)]
use crate::se_integration::EmbeddingEmitter as WindowsEmbeddingEmitter;
#[cfg(windows)]
use crate::tier::tier1_session_memory::LlmCallEmitter as WindowsTier1LlmCallEmitter;
#[cfg(windows)]
use crate::tier::tier2_extract_memories::LlmCallEmitter as WindowsTier2LlmCallEmitter;
#[cfg(windows)]
use crate::tier::tier3_auto_dream::LlmCallEmitter as WindowsTier3LlmCallEmitter;
#[cfg(windows)]
use crate::tier::tier3_imagination::LlmCallEmitter as WindowsTier3ImagLlmCallEmitter;

/// Real broadcast emitter (Windows Named Pipe). See unix sibling for docs.
#[cfg(windows)]
pub struct UdsBroadcastEmitter {
    event_sink: WindowsArc<WindowsEventSink>,
}

#[cfg(windows)]
impl UdsBroadcastEmitter {
    #[must_use]
    pub fn new(event_sink: WindowsArc<WindowsEventSink>) -> Self {
        Self { event_sink }
    }

    async fn push_llm_request(&self, request: &WindowsLlmCallRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize LlmCallRequestPayload; dropping: {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": LLM_CALL_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    async fn push_embedding_request(&self, request: &WindowsEmbeddingRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize EmbeddingRequestPayload; dropping: \
                     {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": EMBEDDING_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    async fn push_rerank_request(&self, request: &WindowsRerankRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize RerankRequestPayload; dropping: {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": RERANK_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    async fn push_tool_request(&self, request: &WindowsToolCallRequestPayload) {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize ToolCallRequestPayload; dropping: \
                     {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": TOOL_CALL_REQUEST_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }

    async fn push_gate_skip(&self, payload: &GateSkipPayload) {
        let payload = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "broadcast_emitter: failed to serialize GateSkipPayload; dropping: {err}"
                );
                return;
            }
        };
        let frame = serde_json::json!({
            "notification": GATE_SKIPPED_NOTIFICATION,
            "payload": payload,
        });
        self.event_sink.push_frame(&frame).await;
    }
}

#[cfg(windows)]
#[async_trait]
impl GateSkipEmitter for UdsBroadcastEmitter {
    async fn emit_gate_skip(&self, payload: GateSkipPayload) {
        self.push_gate_skip(&payload).await;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(windows)]
#[async_trait]
impl WindowsTier1LlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: WindowsLlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(windows)]
#[async_trait]
impl WindowsTier2LlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: WindowsLlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(windows)]
#[async_trait]
impl WindowsTier3LlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: WindowsLlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(windows)]
#[async_trait]
impl WindowsTier3ImagLlmCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: WindowsLlmCallRequestPayload) {
        self.push_llm_request(&request).await;
    }
}

#[cfg(windows)]
#[async_trait]
impl WindowsEmbeddingEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: WindowsEmbeddingRequestPayload) {
        self.push_embedding_request(&request).await;
    }

    async fn emit_rerank_request(&self, request: WindowsRerankRequestPayload) {
        self.push_rerank_request(&request).await;
    }
}

#[cfg(windows)]
#[async_trait]
impl WindowsToolCallEmitter for UdsBroadcastEmitter {
    async fn emit_request(&self, request: WindowsToolCallRequestPayload) {
        self.push_tool_request(&request).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tier::{LlmCallParams, LlmMessage, MemoryTier};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixStream;

    fn sample_llm_request() -> LlmCallRequestPayload {
        LlmCallRequestPayload {
            req_id: "tier2-req-1".to_string(),
            tier: MemoryTier::Extract,
            phase: None,
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            model_hint: None,
            params: LlmCallParams {
                temperature: Some(0.5),
                max_tokens: Some(256),
            },
        }
    }

    fn sample_embedding_request() -> EmbeddingRequestPayload {
        EmbeddingRequestPayload {
            req_id: "se-embed-1".to_string(),
            texts: vec!["chunk a".to_string(), "chunk b".to_string()],
            text_keys: vec!["k0".to_string(), "k1".to_string()],
            model_hint: None,
        }
    }

    #[tokio::test]
    async fn emit_llm_request_pushes_expected_frame() {
        let sink = EventSink::new();
        let (client, server) = UnixStream::pair().expect("pair");
        let (_r, server_write) = server.into_split();
        sink.register(server_write).await;

        let emitter = UdsBroadcastEmitter::new(Arc::clone(&sink));
        // Call through the per-tier trait to exercise the trait dispatch.
        Tier2LlmCallEmitter::emit_request(&emitter, sample_llm_request()).await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read line");
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(parsed["notification"], LLM_CALL_REQUEST_NOTIFICATION);
        // payload is snake_case wire (matches tier::LlmCallRequestPayload serde).
        assert_eq!(parsed["payload"]["req_id"], "tier2-req-1");
        assert_eq!(parsed["payload"]["tier"], "Extract");
        assert_eq!(parsed["payload"]["messages"][0]["role"], "user");
        assert_eq!(parsed["payload"]["params"]["max_tokens"], 256);
    }

    #[tokio::test]
    async fn emit_tool_request_pushes_expected_frame() {
        use crate::tier::tier3_imagination::{ToolCall, ToolCallRequestPayload, ToolKind};

        let sink = EventSink::new();
        let (client, server) = UnixStream::pair().expect("pair");
        let (_r, server_write) = server.into_split();
        sink.register(server_write).await;

        let emitter = UdsBroadcastEmitter::new(Arc::clone(&sink));
        let request = ToolCallRequestPayload {
            req_id: "tier3-imagination-evidence-1-123".to_string(),
            tier: MemoryTier::Dream,
            calls: vec![
                ToolCall {
                    kind: ToolKind::WebSearch,
                    query: Some("rust async".to_string()),
                    url: None,
                    id: None,
                    path: None,
                    root: None,
                },
                ToolCall {
                    kind: ToolKind::WebFetch,
                    query: None,
                    url: Some("https://example.com".to_string()),
                    id: None,
                    path: None,
                    root: None,
                },
            ],
        };
        ToolCallEmitter::emit_request(&emitter, request).await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read line");
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(parsed["notification"], TOOL_CALL_REQUEST_NOTIFICATION);
        assert_eq!(
            parsed["payload"]["req_id"],
            "tier3-imagination-evidence-1-123"
        );
        // `tier` is PascalCase (matches protocol MemoryTierKind round-trip).
        assert_eq!(parsed["payload"]["tier"], "Dream");
        // `kind` is camelCase (matches protocol MemoryTierToolKind).
        assert_eq!(parsed["payload"]["calls"][0]["kind"], "webSearch");
        assert_eq!(parsed["payload"]["calls"][0]["query"], "rust async");
        assert!(parsed["payload"]["calls"][0].get("url").is_none());
        assert_eq!(parsed["payload"]["calls"][1]["kind"], "webFetch");
        assert_eq!(parsed["payload"]["calls"][1]["url"], "https://example.com");
    }

    #[tokio::test]
    async fn emit_gate_skip_pushes_expected_frame() {
        let sink = EventSink::new();
        let (client, server) = UnixStream::pair().expect("pair");
        let (_r, server_write) = server.into_split();
        sink.register(server_write).await;

        let emitter = UdsBroadcastEmitter::new(Arc::clone(&sink));
        GateSkipEmitter::emit_gate_skip(
            &emitter,
            GateSkipPayload {
                tier: "tier3".to_string(),
                gate_name: "dream_gate".to_string(),
                reason: "session_count_unmet".to_string(),
                context: None,
                skipped_at_ms: 1_700_300_000_000,
            },
        )
        .await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read line");
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(parsed["notification"], GATE_SKIPPED_NOTIFICATION);
        assert_eq!(parsed["payload"]["tier"], "tier3");
        assert_eq!(parsed["payload"]["gate_name"], "dream_gate");
        assert_eq!(parsed["payload"]["reason"], "session_count_unmet");
        assert_eq!(parsed["payload"]["skipped_at_ms"], 1_700_300_000_000_i64);
        // `context` is None → skipped on the wire.
        assert!(parsed["payload"].get("context").is_none());
    }

    #[tokio::test]
    async fn recording_gate_skip_emitter_records() {
        let emitter = RecordingGateSkipEmitter::new();
        GateSkipEmitter::emit_gate_skip(
            &emitter,
            GateSkipPayload {
                tier: "tier3".to_string(),
                gate_name: "idle".to_string(),
                reason: "foreground active".to_string(),
                context: None,
                skipped_at_ms: 42,
            },
        )
        .await;
        let recorded = emitter.recorded().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].gate_name, "idle");
        assert_eq!(recorded[0].skipped_at_ms, 42);
    }

    #[tokio::test]
    async fn emit_embedding_request_pushes_expected_frame() {
        let sink = EventSink::new();
        let (client, server) = UnixStream::pair().expect("pair");
        let (_r, server_write) = server.into_split();
        sink.register(server_write).await;

        let emitter = UdsBroadcastEmitter::new(Arc::clone(&sink));
        EmbeddingEmitter::emit_request(&emitter, sample_embedding_request()).await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read line");
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(parsed["notification"], EMBEDDING_REQUEST_NOTIFICATION);
        assert_eq!(parsed["payload"]["req_id"], "se-embed-1");
        assert_eq!(parsed["payload"]["texts"][0], "chunk a");
        assert_eq!(parsed["payload"]["text_keys"][1], "k1");
    }
}
