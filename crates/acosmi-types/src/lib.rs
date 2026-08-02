//! Acosmi 统一类型库
//!
//! 合并自三个源 crate：
//! - oa-types（配置、会话、健康、状态类型）
//! - crabcode-common（跨语言错误合约、能力协议、执行类型、观测）
//! - oa-runtime（运行时环境抽象）

// ── 基础模块（无内部依赖或仅依赖 common） ──
pub mod common;
pub mod queue;
pub mod sandbox;
pub mod tts;

// ── 基础类型（依赖 common） ──
pub mod auth;
pub mod base;

// ── 领域模块（依赖 base/common/sandbox） ──
pub mod approvals;
pub mod browser;
pub mod cron;
pub mod gateway;
pub mod hooks;
pub mod memory;
pub mod models;
pub mod node_host;
pub mod plugins;
pub mod skills;
pub mod tools;

// ── 高层模块（依赖领域模块） ──
pub mod agent_defaults;
pub mod agents;
pub mod channels;
pub mod messages;

// ── 顶层配置（依赖所有模块） ──
pub mod config;

// ── 运行时类型 ──
pub mod health;
pub mod session;
pub mod status;

// ── 跨语言协议与执行类型（原 crabcode-common） ──
pub mod error;
pub mod exec_types;
pub mod otel;
pub mod protocol;

// ── 运行时环境抽象（原 oa-runtime） ──
pub mod runtime;

// ── 便捷 re-export：配置类型 ──
pub use common::ChatType;
pub use common::JsonOutput;
pub use config::ConfigFileSnapshot;
pub use config::ConfigValidationIssue;
pub use config::CrabClawConfig;
pub use config::LegacyConfigIssue;
pub use health::HealthResult;
pub use session::SessionEntry;
pub use status::SystemStatus;

// ── 便捷 re-export：跨语言协议类型 ──
pub use error::CrabError;
pub use exec_types::{ExecReq, ExecResult};
pub use otel::{ObservabilityContext, OtelConfig};
pub use protocol::{CapabilityReq, CapabilityResp};

// ── 便捷 re-export：运行时 ──
pub use runtime::{DefaultRuntime, RuntimeEnv};
