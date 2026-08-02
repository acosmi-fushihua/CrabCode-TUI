//! 核心权限类型
//!
//! 等价于 TS PermissionMode.ts, PermissionRule.ts, PermissionResult.ts

use serde::{Deserialize, Serialize};

// ─── Permission Mode ───

/// 权限模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// 正常提示模式
    Default,
    /// 计划/预览模式
    Plan,
    /// 自动批准编辑
    AcceptEdits,
    /// 跳过所有权限检查
    BypassPermissions,
    /// 拒绝所有操作
    DontAsk,
    /// 自动批准（内部，分类器驱动）
    Auto,
    /// 内部状态标志（代理委派）
    Bubble,
}

// ─── Permission Behavior ───

/// 权限行为
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

// ─── Permission Rule ───

/// 权限规则来源
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionRuleSource {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    FlagSettings,
    PolicySettings,
    CliArg,
    Command,
    Session,
}

/// 权限规则值
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// 工具名（如 "Bash", "`PowerShell`", "Edit"）
    pub tool_name: String,
    /// 规则内容（如 "npm:*", "git commit"）
    pub rule_content: Option<String>,
}

/// 权限规则
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub source: PermissionRuleSource,
    #[serde(rename = "ruleBehavior")]
    pub behavior: PermissionBehavior,
    #[serde(rename = "ruleValue")]
    pub value: PermissionRuleValue,
}

// ─── Permission Result ───

/// 权限决策原因
#[derive(Debug, Clone)]
pub enum PermissionDecisionReason {
    Rule {
        rule: PermissionRule,
    },
    Mode {
        mode: PermissionMode,
    },
    SafetyCheck {
        reason: String,
        classifier_approvable: bool,
    },
    Other {
        reason: String,
    },
}

/// 权限决策结果
#[derive(Debug, Clone)]
pub enum PermissionResult {
    Allow {
        updated_input: Option<String>,
        reason: Option<PermissionDecisionReason>,
    },
    Ask {
        message: String,
        suggestions: Vec<PermissionUpdate>,
        blocked_path: Option<String>,
    },
    Deny {
        message: String,
        reason: PermissionDecisionReason,
    },
    Passthrough,
}

impl PermissionResult {
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow {
            updated_input: None,
            reason: None,
        }
    }

    #[must_use]
    pub const fn allow_with_reason(reason: PermissionDecisionReason) -> Self {
        Self::Allow {
            updated_input: None,
            reason: Some(reason),
        }
    }

    pub fn ask(message: impl Into<String>) -> Self {
        Self::Ask {
            message: message.into(),
            suggestions: vec![],
            blocked_path: None,
        }
    }

    pub fn deny(message: impl Into<String>, reason: PermissionDecisionReason) -> Self {
        Self::Deny {
            message: message.into(),
            reason,
        }
    }

    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
    #[must_use]
    pub const fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }
    #[must_use]
    pub const fn is_ask(&self) -> bool {
        matches!(self, Self::Ask { .. })
    }
}

/// 权限更新建议
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionUpdate {
    pub behavior: PermissionBehavior,
    pub value: PermissionRuleValue,
    pub source: PermissionRuleSource,
}

// ─── Security Types ───

/// 安全违规
#[derive(Debug, Clone)]
pub struct SecurityViolation {
    /// 检查 ID（用于分析）
    pub check_id: u32,
    /// 检查名称
    pub check_name: String,
    /// 违规描述
    pub message: String,
    /// 是否为解析差异导致的问题
    pub is_misparsing: bool,
}

/// 安全检查结果
#[derive(Debug, Clone)]
pub enum SecurityCheckResult {
    /// 通过（继续后续检查）
    Passthrough,
    /// 安全（可提前退出）
    Allow,
    /// 需要用户确认
    Ask {
        message: String,
        is_misparsing: bool,
        check_id: u32,
    },
}

/// 文件操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperationType {
    Read,
    Write,
    Create,
}

/// 路径检查结果
#[derive(Debug, Clone)]
pub struct PathCheckResult {
    pub allowed: bool,
    pub reason: Option<PermissionDecisionReason>,
}

/// Shell 权限规则类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellPermissionRule {
    /// 精确匹配
    Exact { command: String },
    /// 前缀匹配（旧版 :* 语法）
    Prefix { prefix: String },
    /// 通配符匹配
    Wildcard { pattern: String },
}

/// 权限检查上下文
#[derive(Debug, Clone)]
pub struct PermissionContext {
    /// 当前工作目录
    pub cwd: String,
    /// 原始工作目录
    pub original_cwd: Option<String>,
    /// 权限模式
    pub mode: PermissionMode,
    /// 权限规则列表
    pub rules: Vec<PermissionRule>,
    /// 工具名
    pub tool_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_result_helpers() {
        assert!(PermissionResult::allow().is_allow());
        assert!(PermissionResult::ask("test").is_ask());
        assert!(
            PermissionResult::deny(
                "test",
                PermissionDecisionReason::Other { reason: "x".into() }
            )
            .is_deny()
        );
    }
}
