// Phase 3B 初始实现：部分函数和类型将在 Phase 3C (permission) 中使用
#![allow(dead_code)]

//! `CrabCode` Shell 解析器（壳析模块）
//!
//! 提供 bash/powershell 命令解析、安全分析和 shell provider 抽象。
//!
//! # 模块结构
//!
//! - [`bash`]: Bash 词法分析、递归下降解析、AST 安全分析
//! - [`powershell`]: `PowerShell` 命令解析（pwsh 子进程模式）
//! - [`shell`]: Shell 发现与 provider 选择

pub mod bash;
pub mod powershell;
pub mod shell;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Core Types ───

/// Shell 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellType {
    Bash,
    PowerShell,
}

/// Tree-sitter 兼容的 AST 节点
///
/// 所有索引为 UTF-8 字节偏移量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsNode {
    /// 节点类型（如 "command", "pipeline", "word"）
    #[serde(rename = "type")]
    pub node_type: String,
    /// 节点原始文本（从源字符串按 UTF-8 字节切片）
    pub text: String,
    /// UTF-8 字节偏移：起始位置
    #[serde(rename = "startIndex")]
    pub start_index: usize,
    /// UTF-8 字节偏移：结束位置（不含）
    #[serde(rename = "endIndex")]
    pub end_index: usize,
    /// 子节点列表
    pub children: Vec<Self>,
}

impl TsNode {
    /// 创建新节点
    #[must_use]
    pub fn new(
        node_type: impl Into<String>,
        text: impl Into<String>,
        start: usize,
        end: usize,
    ) -> Self {
        Self {
            node_type: node_type.into(),
            text: text.into(),
            start_index: start,
            end_index: end,
            children: Vec::new(),
        }
    }

    /// 查找指定类型的第一个子节点
    #[must_use]
    pub fn child_by_type(&self, node_type: &str) -> Option<&Self> {
        self.children.iter().find(|c| c.node_type == node_type)
    }

    /// 查找指定类型的所有子节点
    #[must_use]
    pub fn children_by_type(&self, node_type: &str) -> Vec<&Self> {
        self.children
            .iter()
            .filter(|c| c.node_type == node_type)
            .collect()
    }

    /// 递归查找指定类型的第一个后代节点
    #[must_use]
    pub fn find_descendant(&self, node_type: &str) -> Option<&Self> {
        for child in &self.children {
            if child.node_type == node_type {
                return Some(child);
            }
            if let Some(found) = child.find_descendant(node_type) {
                return Some(found);
            }
        }
        None
    }
}

// ─── Parsed Command Types ───

/// 高级解析结果
#[derive(Debug, Clone)]
pub struct ParsedCommandData {
    /// 根 AST 节点
    pub root_node: TsNode,
    /// 前导环境变量赋值（如 `VAR=val`）
    pub env_vars: Vec<String>,
    /// 命令节点（AST 中的 command 节点）
    pub command_node: Option<TsNode>,
    /// 原始命令字符串
    pub original_command: String,
}

/// 简单命令（安全分析输出的基本单元）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleCommand {
    /// 解析后的 argv（引号已移除）
    pub argv: Vec<String>,
    /// 环境变量前缀赋值
    pub env_vars: Vec<EnvVar>,
    /// 输入/输出重定向
    pub redirects: Vec<Redirect>,
    /// 原始源文本跨度
    pub text: String,
}

/// 环境变量赋值
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// 重定向操作
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    /// 重定向操作符
    pub op: RedirectOp,
    /// 目标文件/描述符
    pub target: String,
    /// 文件描述符（可选，如 2>&1 中的 2）
    pub fd: Option<u32>,
}

/// 重定向操作符类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RedirectOp {
    /// `>`
    Write,
    /// `>>`
    Append,
    /// `<`
    Read,
    /// `<<`
    HereDoc,
    /// `>&`
    DupOutput,
    /// `>|`
    Clobber,
    /// `<&`
    DupInput,
    /// `&>`
    WriteAll,
    /// `&>>`
    AppendAll,
    /// `<<<`
    HereString,
}

impl RedirectOp {
    /// 从操作符字符串解析
    #[must_use]
    pub fn from_str_op(s: &str) -> Option<Self> {
        match s {
            ">" => Some(Self::Write),
            ">>" => Some(Self::Append),
            "<" => Some(Self::Read),
            "<<" => Some(Self::HereDoc),
            ">&" => Some(Self::DupOutput),
            ">|" => Some(Self::Clobber),
            "<&" => Some(Self::DupInput),
            "&>" => Some(Self::WriteAll),
            "&>>" => Some(Self::AppendAll),
            "<<<" => Some(Self::HereString),
            _ => None,
        }
    }

    /// 转为操作符字符串
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Write => ">",
            Self::Append => ">>",
            Self::Read => "<",
            Self::HereDoc => "<<",
            Self::DupOutput => ">&",
            Self::Clobber => ">|",
            Self::DupInput => "<&",
            Self::WriteAll => "&>",
            Self::AppendAll => "&>>",
            Self::HereString => "<<<",
        }
    }
}

// ─── Security Analysis Types ───

/// 安全分析结果（parseForSecurity 返回值）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseForSecurityResult {
    /// 成功解析为简单命令列表
    Simple { commands: Vec<SimpleCommand> },
    /// 命令过于复杂，无法安全分析
    TooComplex {
        reason: String,
        node_type: Option<String>,
    },
    /// 解析器不可用
    ParseUnavailable,
}

impl ParseForSecurityResult {
    /// 检查是否为 Simple 结果
    #[must_use]
    pub const fn is_simple(&self) -> bool {
        matches!(self, Self::Simple { .. })
    }

    /// 获取命令列表（仅当 Simple 时）
    #[must_use]
    pub fn commands(&self) -> Option<&[SimpleCommand]> {
        match self {
            Self::Simple { commands } => Some(commands),
            _ => None,
        }
    }
}

// ─── Parse Result ───

/// 解析结果
#[derive(Debug, Clone)]
pub enum ParseResult {
    /// 解析成功
    Success(TsNode),
    /// 解析中止（超时或节点预算耗尽）
    Aborted,
    /// 解析失败
    Failed,
}

// ─── Heredoc Types ───

/// Heredoc 信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeredocInfo {
    /// 完整 heredoc 文本（含 << 和正文）
    pub full_text: String,
    /// 分隔符词（去引号后）
    pub delimiter: String,
    /// `<<` 操作符在源文本中的起始字节偏移
    pub operator_start_index: usize,
    /// `<<` 操作符结束字节偏移
    pub operator_end_index: usize,
    /// 正文起始字节偏移（换行后）
    pub content_start_index: usize,
    /// 正文结束字节偏移
    pub content_end_index: usize,
}

/// Heredoc 提取结果
#[derive(Debug, Clone)]
pub struct HeredocExtractionResult {
    /// 替换后的命令（heredoc 被占位符替代）
    pub command: String,
    /// 占位符 → `HeredocInfo` 映射
    pub heredocs: HashMap<String, HeredocInfo>,
}

// ─── TreeSitter Analysis Types ───

/// 引用上下文（不同层级的引号去除结果）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuoteContext {
    /// 单引号内容已移除
    pub with_double_quotes: String,
    /// 所有引号内容已移除
    pub fully_unquoted: String,
    /// 引号区间替换为分隔符
    pub unquoted_keep_quote_chars: String,
}

/// 复合结构分析
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompoundStructure {
    pub has_compound_operators: bool,
    pub has_pipeline: bool,
    pub has_subshell: bool,
    pub has_command_group: bool,
    /// 找到的操作符列表（`&&`, `||`, `;`）
    pub operators: Vec<String>,
    /// 按操作符分割的命令段
    pub segments: Vec<String>,
}

/// 危险模式检测
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DangerousPatterns {
    pub has_command_substitution: bool,
    pub has_process_substitution: bool,
    pub has_parameter_expansion: bool,
    pub has_heredoc: bool,
    pub has_comment: bool,
}

/// 综合安全分析结构
#[derive(Debug, Clone, Default)]
pub struct TreeSitterAnalysis {
    pub quote_context: QuoteContext,
    pub compound_structure: CompoundStructure,
    /// 是否存在真正的操作符节点（非转义）
    pub has_actual_operator_nodes: bool,
    pub dangerous_patterns: DangerousPatterns,
}

// ─── Command Spec Types ───

/// 命令规格（用于自动补全和前缀提取）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcommands: Option<Vec<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<Argument>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<CmdOption>>,
}

/// 命令参数
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Argument {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub is_dangerous: bool,
    #[serde(default)]
    pub is_variadic: bool,
    #[serde(default)]
    pub is_optional: bool,
    /// 参数是一个子命令（如 timeout, sudo）
    #[serde(default)]
    pub is_command: bool,
    #[serde(default)]
    pub is_script: bool,
}

/// 命令选项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdOption {
    /// 选项名（单个或别名列表）
    pub name: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<Argument>>,
    #[serde(default)]
    pub is_required: bool,
}

// ─── Error Types ───

/// Shell 解析器错误
#[derive(Debug, thiserror::Error)]
pub enum ShellParserError {
    #[error("parse timeout ({timeout_ms}ms exceeded)")]
    Timeout { timeout_ms: u64 },

    #[error("node budget exceeded (max {max_nodes})")]
    NodeBudgetExceeded { max_nodes: usize },

    #[error("command too long ({length} > {max_length})")]
    CommandTooLong { length: usize, max_length: usize },

    #[error("powershell spawn error: {0}")]
    PwshSpawn(String),

    #[error("powershell parse error: {0}")]
    PwshParse(String),

    #[error("powershell timeout ({timeout_ms}ms)")]
    PwshTimeout { timeout_ms: u64 },

    #[error("invalid JSON from pwsh: {0}")]
    InvalidJson(String),

    #[error("shell not found: {0}")]
    ShellNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
