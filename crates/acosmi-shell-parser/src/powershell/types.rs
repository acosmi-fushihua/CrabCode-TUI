//! `PowerShell` 解析器类型
//!
//! 等价于 TS powershell/parser.ts 中的类型定义。

use serde::{Deserialize, Serialize};

/// `PowerShell` 解析结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedPowerShellCommand {
    pub valid: bool,
    pub errors: Vec<ParseError>,
    pub statements: Vec<ParsedStatement>,
    pub variables: Vec<ParsedVariable>,
    pub has_stop_parsing: bool,
    pub original_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_literals: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_using_statements: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_script_requirements: Option<bool>,
}

/// 管道元素类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineElementType {
    CommandAst,
    CommandExpressionAst,
    ParenExpressionAst,
}

/// 命令元素类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandElementType {
    ScriptBlock,
    SubExpression,
    ExpandableString,
    MemberInvocation,
    Variable,
    StringConstant,
    Parameter,
    Other,
}

/// 语句类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatementType {
    PipelineAst,
    PipelineChainAst,
    AssignmentStatementAst,
    IfStatementAst,
    ForStatementAst,
    ForEachStatementAst,
    WhileStatementAst,
    DoWhileStatementAst,
    DoUntilStatementAst,
    SwitchStatementAst,
    TryStatementAst,
    TrapStatementAst,
    FunctionDefinitionAst,
    DataStatementAst,
    UnknownStatementAst,
}

/// 解析后的语句
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedStatement {
    pub statement_type: StatementType,
    pub commands: Vec<ParsedCommandElement>,
    pub redirections: Vec<ParsedRedirection>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_commands: Option<Vec<ParsedCommandElement>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_patterns: Option<SecurityPatterns>,
}

/// 安全模式标记
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityPatterns {
    #[serde(default)]
    pub has_member_invocations: bool,
    #[serde(default)]
    pub has_sub_expressions: bool,
    #[serde(default)]
    pub has_expandable_strings: bool,
    #[serde(default)]
    pub has_script_blocks: bool,
}

/// 命令元素
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedCommandElement {
    pub name: String,
    pub name_type: CommandNameType,
    pub element_type: PipelineElementType,
    pub args: Vec<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element_types: Option<Vec<CommandElementType>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Option<Vec<CommandElementChild>>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirections: Option<Vec<ParsedRedirection>>,
}

/// 命令元素子节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandElementChild {
    #[serde(rename = "type")]
    pub child_type: CommandElementType,
    pub text: String,
}

/// 命令名类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandNameType {
    Cmdlet,
    Application,
    Unknown,
}

/// 重定向
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedRedirection {
    pub operator: String,
    pub target: String,
    pub is_merging: bool,
}

/// 变量
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedVariable {
    pub path: String,
    pub is_splatted: bool,
}

/// 解析错误
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseError {
    pub message: String,
    pub error_id: String,
}

/// 安全标记
#[derive(Debug, Clone, Default)]
pub struct SecurityFlags {
    pub has_sub_expressions: bool,
    pub has_script_blocks: bool,
    pub has_splatting: bool,
    pub has_expandable_strings: bool,
    pub has_member_invocations: bool,
    pub has_assignments: bool,
    pub has_stop_parsing: bool,
}

// ─── Raw Types (from pwsh JSON output) ───

#[derive(Debug, Clone, Deserialize)]
pub struct RawParseResult {
    pub valid: Option<bool>,
    pub errors: Option<Vec<RawParseError>>,
    pub statements: Option<Vec<RawStatement>>,
    pub variables: Option<Vec<RawVariable>>,
    #[serde(rename = "hasStopParsing")]
    pub has_stop_parsing: Option<bool>,
    #[serde(rename = "typeLiterals")]
    pub type_literals: Option<Vec<String>>,
    #[serde(rename = "hasUsingStatements")]
    pub has_using_statements: Option<bool>,
    #[serde(rename = "hasScriptRequirements")]
    pub has_script_requirements: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawParseError {
    pub message: Option<String>,
    #[serde(rename = "errorId")]
    pub error_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawStatement {
    #[serde(rename = "type")]
    pub stmt_type: Option<String>,
    pub text: Option<String>,
    pub elements: Option<Vec<RawPipelineElement>>,
    #[serde(rename = "nestedCommands")]
    pub nested_commands: Option<Vec<RawPipelineElement>>,
    pub redirections: Option<Vec<RawRedirection>>,
    #[serde(rename = "securityPatterns")]
    pub security_patterns: Option<SecurityPatterns>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawPipelineElement {
    #[serde(rename = "type")]
    pub elem_type: Option<String>,
    pub text: Option<String>,
    #[serde(rename = "commandElements")]
    pub command_elements: Option<Vec<RawCommandElement>>,
    pub redirections: Option<Vec<RawRedirection>>,
    #[serde(rename = "expressionType")]
    pub expression_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawCommandElement {
    #[serde(rename = "type")]
    pub elem_type: Option<String>,
    pub text: Option<String>,
    pub value: Option<String>,
    #[serde(rename = "expressionType")]
    pub expression_type: Option<String>,
    pub children: Option<Vec<RawChildElement>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawChildElement {
    #[serde(rename = "type")]
    pub child_type: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRedirection {
    #[serde(rename = "type")]
    pub redir_type: Option<String>,
    pub append: Option<bool>,
    #[serde(rename = "fromStream")]
    pub from_stream: Option<String>,
    #[serde(rename = "locationText")]
    pub location_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawVariable {
    pub path: Option<String>,
    #[serde(rename = "isSplatted")]
    pub is_splatted: Option<bool>,
}

/// 命令名分类
#[must_use]
pub fn classify_command_name(name: &str) -> CommandNameType {
    // SECURITY: Non-ASCII characters in cmdlet position are inherently suspicious.
    // .NET OrdinalIgnoreCase folds U+017F (ſ) → S and U+0131 (ı) → I, so
    // PowerShell resolves `ſtart-proceſſ` → Start-Process at runtime. Rust
    // .to_lowercase() does NOT fold these, so every downstream name comparison
    // misses. Force 'application' to gate auto-allow. Finding #31.
    if name.chars().any(|c| c as u32 >= 0x0080) {
        return CommandNameType::Application;
    }
    // Cmdlet pattern: Verb-Noun (entire string must match ^[A-Za-z]+-[A-Za-z][A-Za-z0-9_]*$)
    if is_cmdlet_pattern(name) {
        return CommandNameType::Cmdlet;
    }
    // Application pattern: has extension or is known external
    if name.contains('.') || name.contains('/') || name.contains('\\') {
        return CommandNameType::Application;
    }
    CommandNameType::Unknown
}

/// Check if name matches the Verb-Noun cmdlet pattern: ^[A-Za-z]+-[A-Za-z][A-Za-z0-9_]*$
fn is_cmdlet_pattern(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    // Find the first hyphen
    let hyphen_pos = match bytes.iter().position(|&b| b == b'-') {
        Some(pos) => pos,
        None => return false,
    };
    // Verb part: at least one ASCII letter before the hyphen
    if hyphen_pos == 0 {
        return false;
    }
    if !bytes[..hyphen_pos].iter().all(|&b| b.is_ascii_alphabetic()) {
        return false;
    }
    // Noun part: at least one char after hyphen, first must be ASCII letter
    let noun = &bytes[hyphen_pos + 1..];
    if noun.is_empty() {
        return false;
    }
    if !noun[0].is_ascii_alphabetic() {
        return false;
    }
    // Rest of noun: ASCII alphanumeric or underscore, no additional hyphens
    noun[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// 去除模块前缀
#[must_use]
pub fn strip_module_prefix(name: &str) -> String {
    let idx = match name.rfind('\\') {
        Some(pos) => pos,
        None => return name.to_string(),
    };
    // Don't strip file paths: drive letters (C:\...), UNC paths (\\server\...), or relative paths (.\, ..\)
    let bytes = name.as_bytes();
    // Drive letter pattern: e.g. C:\
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return name.to_string();
    }
    // UNC path: \\
    if name.starts_with("\\\\") {
        return name.to_string();
    }
    // Relative path: .\ or ..\
    if name.starts_with(".\\") || name.starts_with("..\\") {
        return name.to_string();
    }
    name[idx + 1..].to_string()
}

/// 映射语句类型
#[must_use]
pub fn map_statement_type(raw_type: &str) -> StatementType {
    match raw_type {
        "PipelineAst" => StatementType::PipelineAst,
        "PipelineChainAst" => StatementType::PipelineChainAst,
        "AssignmentStatementAst" => StatementType::AssignmentStatementAst,
        "IfStatementAst" => StatementType::IfStatementAst,
        "ForStatementAst" => StatementType::ForStatementAst,
        "ForEachStatementAst" => StatementType::ForEachStatementAst,
        "WhileStatementAst" => StatementType::WhileStatementAst,
        "DoWhileStatementAst" => StatementType::DoWhileStatementAst,
        "DoUntilStatementAst" => StatementType::DoUntilStatementAst,
        "SwitchStatementAst" => StatementType::SwitchStatementAst,
        "TryStatementAst" => StatementType::TryStatementAst,
        "TrapStatementAst" => StatementType::TrapStatementAst,
        "FunctionDefinitionAst" => StatementType::FunctionDefinitionAst,
        "DataStatementAst" => StatementType::DataStatementAst,
        _ => StatementType::UnknownStatementAst,
    }
}

/// 映射元素类型
#[must_use]
pub fn map_element_type(raw_type: &str, expression_type: Option<&str>) -> CommandElementType {
    match raw_type {
        "ScriptBlockExpressionAst" => CommandElementType::ScriptBlock,
        "SubExpressionAst" | "ArrayExpressionAst" => {
            // SECURITY: ArrayExpressionAst (@()) is a sibling of SubExpressionAst,
            // not a subclass. Both evaluate arbitrary pipelines with side effects:
            // Get-ChildItem @(Remove-Item ./data) runs Remove-Item inside @().
            // Map both to SubExpression so hasSubExpressions fires.
            CommandElementType::SubExpression
        }
        "ParenExpressionAst" => CommandElementType::SubExpression,
        "ExpandableStringExpressionAst" => CommandElementType::ExpandableString,
        "InvokeMemberExpressionAst" | "MemberExpressionAst" => CommandElementType::MemberInvocation,
        "VariableExpressionAst" => CommandElementType::Variable,
        "StringConstantExpressionAst" | "ConstantExpressionAst" => {
            // ConstantExpressionAst covers numeric literals (5, 3.14). For
            // permission purposes a numeric literal is as safe as a string
            // literal — it's an inert value, not code. Always map to StringConstant.
            CommandElementType::StringConstant
        }
        "CommandParameterAst" => CommandElementType::Parameter,
        "CommandExpressionAst" => {
            // Delegate to the wrapped expression type so we catch SubExpressionAst,
            // ExpandableStringExpressionAst, ScriptBlockExpressionAst, etc.
            // without maintaining a manual list. Falls through to 'Other' if the
            // inner type is unrecognised.
            if let Some(et) = expression_type {
                map_element_type(et, None)
            } else {
                CommandElementType::Other
            }
        }
        _ => CommandElementType::Other,
    }
}
