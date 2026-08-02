//! 跨语言错误合约
//!
//! 错误码分类：
//! - 1xxx: 基础设施
//! - 2xxx: 认证
//! - 3xxx: 模型调用
//! - 4xxx: 工具执行
//! - 5xxx: 协议
//! - 6xxx: 外部服务

use serde::{Deserialize, Serialize};

/// 错误码有效范围
const CODE_RANGE_MIN: u32 = 1000;
const CODE_RANGE_MAX: u32 = 6999;

/// 跨语言统一错误类型
#[derive(Debug, Serialize, Deserialize)]
pub struct CrabError {
    /// 错误码（1xxx-6xxx 分类）
    pub code: u32,
    /// 错误消息
    pub message: String,
    /// 是否可重试
    pub retryable: bool,
    /// 用户安全消息（可直接展示给用户）
    pub user_safe_message: String,
    /// 来源层
    pub source_layer: SourceLayer,
    /// 调试详情
    pub debug_detail: String,
    /// 链式错误源（不序列化）
    #[serde(skip)]
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

/// 错误来源层
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceLayer {
    Rust,
    Go,
    Ts,
}

/// 错误码分类
pub mod codes {
    /// 基础设施错误 (1xxx)
    pub const INFRA_BASE: u32 = 1000;
    pub const INFRA_PROCESS_SPAWN_FAILED: u32 = 1001;
    pub const INFRA_PROCESS_TIMEOUT: u32 = 1002;
    pub const INFRA_PROCESS_KILLED: u32 = 1003;
    pub const INFRA_IPC_CONNECT_FAILED: u32 = 1010;
    pub const INFRA_IPC_PROTOCOL_ERROR: u32 = 1011;

    /// 工具执行错误 (4xxx)
    pub const EXEC_BASE: u32 = 4000;
    pub const EXEC_COMMAND_NOT_FOUND: u32 = 4001;
    pub const EXEC_PERMISSION_DENIED: u32 = 4002;
    pub const EXEC_SANDBOX_VIOLATION: u32 = 4003;
    pub const EXEC_TIMEOUT: u32 = 4004;
    pub const EXEC_OUTPUT_TOO_LARGE: u32 = 4005;

    /// 协议错误 (5xxx)
    pub const PROTOCOL_BASE: u32 = 5000;
    pub const PROTOCOL_VERSION_MISMATCH: u32 = 5001;
    pub const PROTOCOL_INVALID_REQUEST: u32 = 5002;
    pub const PROTOCOL_CAPABILITY_DENIED: u32 = 5003;
}

impl std::fmt::Display for CrabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {}: {}",
            self.source_layer_str(),
            self.code,
            self.message
        )
    }
}

impl std::error::Error for CrabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl CrabError {
    const fn source_layer_str(&self) -> &'static str {
        match self.source_layer {
            SourceLayer::Rust => "rust",
            SourceLayer::Go => "go",
            SourceLayer::Ts => "ts",
        }
    }

    /// 校验错误码是否在有效范围内 (1000-6999)
    ///
    /// `debug_assert` 在开发期捕获 bug；release 模式下记录警告但不 panic，
    /// 因为错误处理代码本身不应因无效码而崩溃。
    fn validate_code(code: u32) -> u32 {
        if !(CODE_RANGE_MIN..=CODE_RANGE_MAX).contains(&code) {
            debug_assert!(
                false,
                "CrabError code {code} out of valid range ({CODE_RANGE_MIN}-{CODE_RANGE_MAX})"
            );
            tracing::warn!(
                code,
                "CrabError code out of valid range ({CODE_RANGE_MIN}-{CODE_RANGE_MAX})"
            );
        }
        code
    }

    /// 创建 Rust 层基础设施错误
    pub fn infra(code: u32, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            code: Self::validate_code(code),
            user_safe_message: format!("内部错误 ({code})"),
            message: message.clone(),
            retryable: false,
            source_layer: SourceLayer::Rust,
            debug_detail: message,
            source: None,
        }
    }

    /// 创建 Rust 层基础设施错误（带错误源）
    pub fn infra_with_source(
        code: u32,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let message = message.into();
        Self {
            code: Self::validate_code(code),
            user_safe_message: format!("内部错误 ({code})"),
            message: message.clone(),
            retryable: false,
            source_layer: SourceLayer::Rust,
            debug_detail: message,
            source: Some(Box::new(source)),
        }
    }

    /// 创建 Rust 层执行错误
    pub fn exec(code: u32, message: impl Into<String>, user_msg: impl Into<String>) -> Self {
        Self {
            code: Self::validate_code(code),
            message: message.into(),
            retryable: false,
            source_layer: SourceLayer::Rust,
            user_safe_message: user_msg.into(),
            debug_detail: String::new(),
            source: None,
        }
    }

    /// 创建 Rust 层执行错误（带错误源）
    pub fn exec_with_source(
        code: u32,
        message: impl Into<String>,
        user_msg: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            code: Self::validate_code(code),
            message: message.into(),
            retryable: false,
            source_layer: SourceLayer::Rust,
            user_safe_message: user_msg.into(),
            debug_detail: String::new(),
            source: Some(Box::new(source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infra_error_display() {
        let err = CrabError::infra(codes::INFRA_PROCESS_SPAWN_FAILED, "spawn failed");
        assert_eq!(err.to_string(), "[rust] 1001: spawn failed");
        assert_eq!(err.code, 1001);
        assert_eq!(err.source_layer, SourceLayer::Rust);
        assert!(!err.retryable);
    }

    #[test]
    fn test_exec_error_display() {
        let err = CrabError::exec(codes::EXEC_TIMEOUT, "timed out", "命令超时");
        assert_eq!(err.to_string(), "[rust] 4004: timed out");
        assert_eq!(err.user_safe_message, "命令超时");
    }

    #[test]
    fn test_error_source_chain() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err =
            CrabError::infra_with_source(codes::INFRA_PROCESS_SPAWN_FAILED, "cannot spawn", io_err);
        let source = std::error::Error::source(&err).expect("should have source");
        assert_eq!(source.to_string(), "file not found");
    }

    #[test]
    fn test_error_without_source() {
        let err = CrabError::infra(codes::INFRA_PROCESS_TIMEOUT, "timeout");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_error_serialization_skips_source() {
        let io_err = std::io::Error::other("disk full");
        let err =
            CrabError::infra_with_source(codes::INFRA_IPC_CONNECT_FAILED, "connect failed", io_err);
        let json = serde_json::to_string(&err).expect("serialize");
        // source 字段（错误链）不应出现在 JSON 中，但 source_layer 应保留
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(v.get("source").is_none(), "source field should be skipped");
        assert!(json.contains("1010"));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "out of valid range")]
    fn test_invalid_code_panics_in_debug() {
        let _ = CrabError::infra(999, "bad code");
    }

    #[test]
    fn test_valid_code_boundaries() {
        let _ = CrabError::infra(1000, "min valid");
        let _ = CrabError::exec(6999, "max valid", "ok");
    }
}
