//! OpenTelemetry tracing 初始化与 Correlation ID 工具
//!
//! 提供统一的 tracing subscriber 配置，支持 JSON / text 格式输出，
//! 以及 W3C `TraceContext` 上下文在 IPC 消息中的注入/提取。

use serde::{Deserialize, Serialize};

/// 可观测性配置（对应 observability-config.schema.json 的子集）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelConfig {
    pub log_level: String,
    pub log_format: LogFormat,
    pub otel_enabled: bool,
    pub otel_endpoint: Option<String>,
    pub otel_sample_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

/// `ObservabilityContext` — 跨语言 IPC 消息中透传的 trace 上下文
/// 对应 observability-context.schema.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityContext {
    pub trace_id: String,
    pub span_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baggage: Option<std::collections::HashMap<String, String>>,
    pub sampled: bool,
}

impl ObservabilityContext {
    /// 创建新的上下文，自动生成 `trace_id` 和 `span_id`
    #[must_use]
    pub fn new(session_id: String) -> Self {
        Self {
            trace_id: generate_trace_id(),
            span_id: generate_span_id(),
            session_id,
            task_id: None,
            tool_use_id: None,
            baggage: None,
            sampled: true,
        }
    }

    /// 在当前 trace 下创建子 span 上下文
    #[must_use]
    pub fn child_span(&self) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: generate_span_id(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            tool_use_id: self.tool_use_id.clone(),
            baggage: self.baggage.clone(),
            sampled: self.sampled,
        }
    }

    /// 转换为 W3C traceparent 头格式
    #[must_use]
    pub fn to_traceparent(&self) -> String {
        let flags = if self.sampled { "01" } else { "00" };
        format!("00-{}-{}-{flags}", self.trace_id, self.span_id)
    }
}

/// 生成 128-bit trace ID (32 hex chars)
///
/// 使用时间戳 + 原子计数器 + 线程 ID 混合，避免并发碰撞
fn generate_trace_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let thread_id = thread_id_u64();

    // 高 64 bit = 时间戳，低 64 bit = 计数器混淆 + 线程 ID
    let high = now;
    let low = count
        .wrapping_mul(0x517c_c1b7_2722_0a95)
        .wrapping_add(thread_id);
    let trace_id = (u128::from(high) << 64) | u128::from(low);
    format!("{trace_id:032x}")
}

/// 生成 64-bit span ID (16 hex chars)
///
/// 使用时间戳 + 原子计数器混合，避免并发碰撞。
///
/// W-TELE-P2-FU-5 (2026-04-25 Wave 5)：从 `fn` 升级为 `pub fn`，让
/// `acosmi-supervisor::ipc::generate_resp_span_id` 直接复用，避免两个 crate
/// 各自维护算法略不同的实现。
pub fn generate_span_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = now.wrapping_mul(0x517c_c1b7_2722_0a95).wrapping_add(count);
    format!("{mixed:016x}")
}

/// 从线程 ID 提取数值部分
fn thread_id_u64() -> u64 {
    let id = format!("{:?}", std::thread::current().id());
    id.chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observability_context_new() {
        let ctx = ObservabilityContext::new("test-session".to_string());
        assert_eq!(ctx.trace_id.len(), 32);
        assert_eq!(ctx.span_id.len(), 16);
        assert_eq!(ctx.session_id, "test-session");
        assert!(ctx.sampled);
    }

    #[test]
    fn test_child_span_preserves_trace_id() {
        let parent = ObservabilityContext::new("sess-1".to_string());
        let child = parent.child_span();
        assert_eq!(parent.trace_id, child.trace_id);
        assert_ne!(parent.span_id, child.span_id);
    }

    #[test]
    fn test_traceparent_format() {
        let ctx = ObservabilityContext {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".to_string(),
            span_id: "00f067aa0ba902b7".to_string(),
            session_id: "sess-1".to_string(),
            task_id: None,
            tool_use_id: None,
            baggage: None,
            sampled: true,
        };
        assert_eq!(
            ctx.to_traceparent(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }
}
