// Phase 3C: Permission engine — many functions will be consumed by the TS bridge layer
#![allow(dead_code)]

//! `CrabCode` 权限引擎（安核模块）
//!
//! 提供命令权限判定、安全扫描、只读验证和路径策略。
//!
//! # 模块结构
//!
//! - [`types`] — 核心类型（PermissionMode, Rule, Result）
//! - [`rule_parser`] — 规则解析 "Bash(npm:*)"
//! - [`rule_matching`] — 通配符匹配
//! - [`path_policy`] — 路径约束验证
//! - [`security_scanner`] — Bash 安全扫描器（24 项）
//! - [`readonly_validator`] — 只读命令验证
//! - [`sed_validator`] — sed 安全检查
//! - [`permission_engine`] — 权限决策引擎
//! - [`powershell_security`] — `PowerShell` 安全扫描器（24 项）
//! - [`constants`] — 共享常量集

pub mod constants;
pub mod path_extraction;
pub mod path_policy;
pub mod permission_engine;
pub mod powershell_security;
pub mod readonly_validator;
pub mod rule_matching;
pub mod rule_parser;
pub mod security_scanner;
pub mod sed_validator;
pub mod types;

// Re-export key types
pub use permission_engine::{check_bash_permission, check_powershell_permission};
pub use rule_parser::parse_permission_rule;
pub use security_scanner::{BashSecurityResult, bash_command_is_safe, bash_security_check};
pub use types::*;
