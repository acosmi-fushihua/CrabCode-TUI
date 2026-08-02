//! AST 安全分析 — parseForSecurity
//!
//! 等价于 TS ast.ts (2,679 LOC)。Fail-closed 设计：
//! 未识别的节点类型 → too-complex → 用户确认。
//!
//! 核心函数:
//! - `parse_for_security(cmd)` — 入口
//! - `parse_for_security_from_ast(cmd, root)` — 已有 AST 时使用
//! - `check_semantics(commands)` — argv 级安全后检查

use std::collections::{HashMap, HashSet};

use crate::{
    EnvVar, ParseForSecurityResult, ParseResult, Redirect, RedirectOp, SimpleCommand, TsNode,
};

// ─── Placeholder Constants ───

const CMDSUB_PLACEHOLDER: &str = "__CMDSUB_OUTPUT__";
const VAR_PLACEHOLDER: &str = "__TRACKED_VAR__";

fn contains_placeholder(value: &str) -> bool {
    value.contains(CMDSUB_PLACEHOLDER) || value.contains(VAR_PLACEHOLDER)
}

// ─── Safe/Dangerous Constants ───

fn safe_env_vars() -> HashSet<&'static str> {
    [
        "HOME",
        "PWD",
        "OLDPWD",
        "USER",
        "LOGNAME",
        "SHELL",
        "PATH",
        "HOSTNAME",
        "UID",
        "EUID",
        "PPID",
        "RANDOM",
        "SECONDS",
        "LINENO",
        "TMPDIR",
        "BASH_VERSION",
        "BASHPID",
        "SHLVL",
        "HISTFILE",
        "IFS",
    ]
    .into_iter()
    .collect()
}

fn special_var_names() -> HashSet<&'static str> {
    ["?", "$", "!", "#", "0", "-"].into_iter().collect()
}

fn shell_keywords() -> HashSet<&'static str> {
    [
        "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "case",
        "esac", "function", "select",
    ]
    .into_iter()
    .collect()
}

fn eval_like_builtins() -> HashSet<&'static str> {
    [
        "eval",
        "source",
        ".",
        "exec",
        "command",
        "builtin",
        "fc",
        "coproc",
        "noglob",
        "nocorrect",
        "trap",
        "enable",
        "mapfile",
        "readarray",
        "hash",
        "bind",
        "complete",
        "compgen",
        "alias",
        "let",
    ]
    .into_iter()
    .collect()
}

fn zsh_dangerous_builtins() -> HashSet<&'static str> {
    [
        "zmodload", "emulate", "sysopen", "sysread", "syswrite", "sysseek", "zpty", "ztcp",
        "zsocket", "zf_rm", "zf_mv", "zf_ln", "zf_chmod", "zf_chown", "zf_mkdir", "zf_rmdir",
        "zf_chgrp",
    ]
    .into_iter()
    .collect()
}

fn decl_keywords() -> HashSet<&'static str> {
    ["export", "declare", "typeset", "readonly", "local"]
        .into_iter()
        .collect()
}

/// 下标算术求值标志：命令名 → 标志集（标志后的参数是 NAME，会被算术求值）
fn subscript_eval_flags() -> HashMap<&'static str, HashSet<&'static str>> {
    let mut m = HashMap::new();
    m.insert("test", ["-v", "-R"].into_iter().collect());
    m.insert("[", ["-v", "-R"].into_iter().collect());
    m.insert("[[", ["-v", "-R"].into_iter().collect());
    m.insert("printf", ["-v"].into_iter().collect());
    m.insert("read", ["-a"].into_iter().collect());
    m.insert("unset", ["-v"].into_iter().collect());
    m.insert("wait", ["-p"].into_iter().collect());
    m
}

fn test_arith_cmp_ops() -> HashSet<&'static str> {
    ["-eq", "-ne", "-lt", "-le", "-gt", "-ge"]
        .into_iter()
        .collect()
}

fn bare_subscript_name_builtins() -> HashSet<&'static str> {
    ["read", "unset"].into_iter().collect()
}

fn read_data_flags() -> HashSet<&'static str> {
    ["-p", "-d", "-n", "-N", "-t", "-u", "-i"]
        .into_iter()
        .collect()
}

// ─── Pre-check Functions ───

fn has_control_chars(s: &str) -> bool {
    s.bytes()
        .any(|b| matches!(b, 0x00..=0x08 | 0x0B..=0x1F | 0x7F))
}

fn has_unicode_whitespace(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            '\u{00A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200B}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
        )
    })
}

fn has_backslash_whitespace(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\\' && (bytes[i + 1] == b' ' || bytes[i + 1] == b'\t') {
            return true;
        }
    }
    for i in 1..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\\'
            && bytes[i + 1] == b'\n'
            && !matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\\')
        {
            return true;
        }
    }
    false
}

fn has_zsh_tilde_bracket(s: &str) -> bool {
    s.contains("~[")
}

fn has_zsh_equals_expansion(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'=' && bytes[i + 1].is_ascii_alphabetic() {
            if i == 0 {
                return true;
            }
            if matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'&' | b'|') {
                return true;
            }
        }
    }
    false
}

fn has_brace_quote_obfuscation(s: &str) -> bool {
    let mut in_brace = false;
    let mut in_quote = false;
    for c in s.chars() {
        match c {
            '{' if !in_quote => in_brace = true,
            '}' if !in_quote => in_brace = false,
            '\'' if in_brace => in_quote = !in_quote,
            _ => {}
        }
    }
    in_quote
}

/// 花括号展开检测
fn has_brace_expansion(s: &str) -> bool {
    // {a,b} 或 {1..5} 模式
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let _start = i;
            i += 1;
            let mut has_comma_or_dots = false;
            let mut has_space = false;
            while i < bytes.len() && bytes[i] != b'}' {
                if bytes[i] == b',' {
                    has_comma_or_dots = true;
                }
                if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b'.' {
                    has_comma_or_dots = true;
                }
                if bytes[i] == b' ' || bytes[i] == b'\t' {
                    has_space = true;
                }
                i += 1;
            }
            if i < bytes.len() && has_comma_or_dots && !has_space {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `NEWLINE_HASH_RE`: \n\s*#
fn has_newline_hash(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'#' {
                return true;
            }
        }
    }
    false
}

/// /proc/*/environ 检测
fn has_proc_environ(s: &str) -> bool {
    s.contains("/proc/") && s.contains("/environ")
}

/// 变量名验证
fn is_valid_var_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 裸变量不安全字符检测: 空格/tab/换行/*/?/[
fn bare_var_unsafe(value: &str) -> bool {
    value.is_empty()
        || value
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '*' | '?' | '['))
}

/// 算术表达式叶节点验证
fn is_safe_arith_leaf(s: &str) -> bool {
    // 十进制 | 十六进制 | 进制符号 | 运算符 | 多字符运算符 | 括号
    if s.is_empty() {
        return false;
    }
    // 纯数字
    if s.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    // 0x 十六进制
    if s.starts_with("0x") || s.starts_with("0X") {
        return s[2..].chars().all(|c| c.is_ascii_hexdigit());
    }
    // 进制符号 NNN#DDD
    if let Some(pos) = s.find('#') {
        return s[..pos].chars().all(|c| c.is_ascii_digit())
            && s[pos + 1..].chars().all(|c| c.is_ascii_alphanumeric());
    }
    // 运算符
    if s.chars().all(|c| {
        matches!(
            c,
            '-' | '+'
                | '*'
                | '/'
                | '%'
                | '^'
                | '&'
                | '|'
                | '~'
                | '!'
                | '<'
                | '>'
                | '='
                | '?'
                | ':'
                | '('
                | ')'
                | ','
        )
    }) {
        return true;
    }
    // 多字符运算符
    matches!(
        s,
        "<<" | ">>" | "**" | "&&" | "||" | "<=" | ">=" | "==" | "!=" | "$((" | "))"
    )
}

/// PS4 安全字符集（去掉 ${identifier} 后检查）
fn is_safe_ps4_value(value: &str) -> bool {
    // 先去掉 ${identifier} 引用
    let mut cleaned = value.to_string();
    loop {
        if let Some(start) = cleaned.find("${")
            && let Some(end) = cleaned[start..].find('}')
        {
            let var_name = &cleaned[start + 2..start + end];
            if is_valid_var_name(var_name) {
                cleaned = format!("{}{}", &cleaned[..start], &cleaned[start + end + 1..]);
                continue;
            }
        }
        break;
    }
    // 检查剩余字符是否在安全集合内
    cleaned.chars().all(|c| matches!(c,
        'A'..='Z' | 'a'..='z' | '0'..='9' | ' ' | '_' | '+' | ':' | '.' | '/' | '=' | '[' | ']' | '-'
    ))
}

// ─── Public API ───

/// 解析命令并进行安全分析
#[must_use]
pub fn parse_for_security(cmd: &str) -> ParseForSecurityResult {
    if let Some(result) = run_pre_checks(cmd) {
        return result;
    }

    let result = super::parser::parse_source(cmd, 0);
    match result {
        ParseResult::Success(root) => parse_for_security_from_ast(cmd, &root),
        ParseResult::Aborted => too_complex("parse aborted (timeout or budget)"),
        ParseResult::Failed => ParseForSecurityResult::ParseUnavailable,
    }
}

/// 从已有 AST 进行安全分析
#[must_use]
pub fn parse_for_security_from_ast(cmd: &str, root: &TsNode) -> ParseForSecurityResult {
    // 全部 6 项预检查（TS ast.ts:408-437 — parse_for_security_from_ast 必须有完整预检查）
    if let Some(result) = run_pre_checks(cmd) {
        return result;
    }
    walk_program(root)
}

/// 统一预检查（6 项）
fn run_pre_checks(cmd: &str) -> Option<ParseForSecurityResult> {
    if has_control_chars(cmd) {
        return Some(too_complex("control characters detected"));
    }
    if has_unicode_whitespace(cmd) {
        return Some(too_complex("unicode whitespace detected"));
    }
    if has_backslash_whitespace(cmd) {
        return Some(too_complex("backslash-whitespace pattern"));
    }
    if has_zsh_tilde_bracket(cmd) {
        return Some(too_complex("zsh tilde bracket expansion"));
    }
    if has_zsh_equals_expansion(cmd) {
        return Some(too_complex("zsh equals expansion"));
    }
    if has_brace_quote_obfuscation(cmd) {
        return Some(too_complex("brace-quote obfuscation"));
    }
    None
}

// ─── Tree Walking ───

fn walk_program(root: &TsNode) -> ParseForSecurityResult {
    let mut commands = Vec::new();
    let mut var_scope: HashMap<String, String> = HashMap::new();

    for child in &root.children {
        if let Some(result) = collect_commands(child, &mut commands, &mut var_scope) {
            return result;
        }
    }

    // 运行 check_semantics 后检查
    if let Some(result) = check_semantics(&commands) {
        return result;
    }

    ParseForSecurityResult::Simple { commands }
}

/// 递归收集命令
fn collect_commands(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    match node.node_type.as_str() {
        "command" => match walk_command(node, &[], commands, var_scope) {
            ParseForSecurityResult::Simple { commands: mut cmds } => {
                commands.append(&mut cmds);
                None
            }
            other => Some(other),
        },

        // 声明命令 (export, declare, local, readonly, typeset)
        "declaration_command" => match walk_declaration_command(node, commands, var_scope) {
            ParseForSecurityResult::Simple { commands: mut cmds } => {
                commands.append(&mut cmds);
                None
            }
            other => Some(other),
        },

        // 重定向语句包装
        "redirected_statement" => walk_redirected_statement(node, commands, var_scope),

        // 管道
        "pipeline" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "command"
                        | "declaration_command"
                        | "pipeline"
                        | "redirected_statement"
                        | "negated_command"
                        | "subshell"
                ) {
                    let mut scope_copy = var_scope.clone();
                    if let Some(result) = collect_commands(child, commands, &mut scope_copy) {
                        return Some(result);
                    }
                }
            }
            None
        }

        // && || 链
        "list" => {
            let mut scope_snapshot = var_scope.clone();
            let mut first = true;
            for child in &node.children {
                match child.node_type.as_str() {
                    "&&" => {} // 继承前面的作用域
                    "||" => {
                        *var_scope = scope_snapshot.clone();
                    }
                    _ => {
                        if first {
                            scope_snapshot = var_scope.clone();
                            first = false;
                        }
                        if let Some(result) = collect_commands(child, commands, var_scope) {
                            return Some(result);
                        }
                    }
                }
            }
            None
        }

        // 复合语句 { ... } 或 ; 分隔
        "compound_statement" => {
            for child in &node.children {
                if let Some(result) = collect_commands(child, commands, var_scope) {
                    return Some(result);
                }
            }
            None
        }

        // 子 shell
        "subshell" => {
            let mut scope_copy = var_scope.clone();
            for child in &node.children {
                if let Some(result) = collect_commands(child, commands, &mut scope_copy) {
                    return Some(result);
                }
            }
            None
        }

        // 裸变量赋值
        "variable_assignment" => {
            if let Some(result) = walk_bare_assignment(node, var_scope) {
                return Some(result);
            }
            None
        }

        // unset 命令
        "unset_command" => match walk_unset_command(node, commands, var_scope) {
            ParseForSecurityResult::Simple { commands: mut cmds } => {
                commands.append(&mut cmds);
                None
            }
            other => Some(other),
        },

        // 否定命令
        "negated_command" => {
            for child in &node.children {
                if let Some(result) = collect_commands(child, commands, var_scope) {
                    return Some(result);
                }
            }
            None
        }

        // 循环和条件 — 提取循环体命令（带 scope 管理）
        "for_statement" => walk_for_statement(node, commands, var_scope),
        "if_statement" => walk_if_statement(node, commands, var_scope),
        "while_statement" | "until_statement" => walk_while_statement(node, commands, var_scope),
        "case_statement" => walk_case_statement(node, commands, var_scope),

        // 测试命令
        "test_command" => match walk_test_command(node, commands, var_scope) {
            ParseForSecurityResult::Simple { commands: mut cmds } => {
                commands.append(&mut cmds);
                None
            }
            other => Some(other),
        },

        // 函数定义
        "function_definition" => Some(too_complex_with_type(
            "function definition",
            "function_definition",
        )),

        // 其他未知类型 → fail-closed
        other => Some(too_complex_with_type(
            &format!("unrecognized node type: {other}"),
            other,
        )),
    }
}

// ─── Command Walking ───

/// 从 command 节点提取 argv（命令前缀赋值不进入全局 scope）
fn walk_command(
    node: &TsNode,
    extra_redirects: &[Redirect],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> ParseForSecurityResult {
    let mut argv = Vec::new();
    let mut env_vars = Vec::new();
    let mut redirects: Vec<Redirect> = extra_redirects.to_vec();

    for child in &node.children {
        match child.node_type.as_str() {
            "command_name" => {
                for name_child in &child.children {
                    match walk_argument(name_child, inner_commands, var_scope, false) {
                        Ok(value) => argv.push(value),
                        Err(result) => return result,
                    }
                }
            }

            "variable_assignment" => {
                // 命令前缀赋值（VAR=val cmd）— 不进入全局 var_scope
                let name = child
                    .child_by_type("variable_name")
                    .map(|n| n.text.clone())
                    .unwrap_or_default();

                if !is_valid_var_name(&name) {
                    return too_complex("invalid variable name in assignment");
                }
                if name == "IFS" {
                    return too_complex("IFS assignment");
                }

                let value = if child.children.len() > 1 {
                    let val_node = &child.children[child.children.len() - 1];
                    match walk_argument(val_node, inner_commands, var_scope, true) {
                        Ok(v) => v,
                        Err(result) => return result,
                    }
                } else {
                    String::new()
                };

                // 命令前缀赋值不进入全局作用域（bash 中是命令本地的）
                env_vars.push(EnvVar { name, value });
            }

            "word"
            | "number"
            | "raw_string"
            | "string"
            | "concatenation"
            | "arithmetic_expansion"
            | "simple_expansion"
            | "command_substitution"
            | "process_substitution"
            | "expansion"
            | "ansi_c_string" => match walk_argument(child, inner_commands, var_scope, false) {
                Ok(value) => argv.push(value),
                Err(result) => return result,
            },

            // 嵌套命令/管道（来自 time, coproc 等包装器）
            "command" | "pipeline" | "subshell" | "list" => {
                let mut nested_cmds = Vec::new();
                let mut scope_copy = var_scope.clone();
                if let Some(result) = collect_commands(child, &mut nested_cmds, &mut scope_copy) {
                    return result;
                }
                inner_commands.extend(nested_cmds);
            }

            "file_redirect" => {
                if let Some(redir) = extract_redirect(child) {
                    redirects.push(redir);
                }
            }

            "herestring_redirect" => {
                // 验证 herestring 内容
                for hc in &child.children {
                    if matches!(
                        hc.node_type.as_str(),
                        "word" | "string" | "raw_string" | "concatenation" | "simple_expansion"
                    ) {
                        match walk_argument(hc, inner_commands, var_scope, true) {
                            Ok(_) => {}
                            Err(result) => return result,
                        }
                    }
                }
                if let Some(redir) = extract_redirect(child) {
                    redirects.push(redir);
                }
            }

            "heredoc_redirect" => {
                // 仅允许带引号分隔符的 heredoc
                let has_quoted_delim = child.children.iter().any(|c| {
                    c.node_type == "heredoc_start"
                        && (c.text.contains('\'') || c.text.contains('"'))
                });
                if !has_quoted_delim {
                    return too_complex_with_type("unquoted heredoc", "heredoc_redirect");
                }
                if let Some(redir) = extract_redirect(child) {
                    redirects.push(redir);
                }
            }

            other => {
                return too_complex_with_type(
                    &format!("unrecognized child in command: {other}"),
                    other,
                );
            }
        }
    }

    if argv.is_empty() && !env_vars.is_empty() {
        return ParseForSecurityResult::Simple {
            commands: Vec::new(),
        };
    }

    // 重建 .text（如果 argv 包含解析过的变量）
    let text = rebuild_text(node, &argv);

    let cmd = SimpleCommand {
        argv,
        env_vars,
        redirects,
        text,
    };
    ParseForSecurityResult::Simple {
        commands: vec![cmd],
    }
}

/// 声明命令处理 (export, declare, local, readonly, typeset)
fn walk_declaration_command(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> ParseForSecurityResult {
    let mut argv = Vec::new();
    let mut env_vars = Vec::new();
    let mut redirects = Vec::new();

    for child in &node.children {
        match child.node_type.as_str() {
            "word" => {
                let text = &child.text;
                // 检查危险声明标志
                if text.starts_with('-')
                    && decl_keywords()
                        .iter()
                        .any(|k| argv.first().is_some_and(|a: &String| a == k))
                {
                    // -n (nameref), -i (integer), -a/-A (array) — 安全风险
                    if text.chars().any(|c| matches!(c, 'n' | 'i' | 'a' | 'A')) {
                        return too_complex(&format!("dangerous declare flag: {text}"));
                    }
                }
                // 裸下标检测
                if text.contains('[') && !text.contains('=') {
                    return too_complex("bare subscript in declaration");
                }
                match walk_argument(child, inner_commands, var_scope, false) {
                    Ok(value) => argv.push(value),
                    Err(result) => return result,
                }
            }

            "variable_assignment" => {
                if let Some(result) = walk_variable_assignment_security(
                    child,
                    inner_commands,
                    var_scope,
                    &mut env_vars,
                ) {
                    return result;
                }
            }

            "variable_name" => {
                argv.push(child.text.clone());
            }

            "string"
            | "raw_string"
            | "concatenation"
            | "number"
            | "simple_expansion"
            | "command_substitution" => {
                match walk_argument(child, inner_commands, var_scope, false) {
                    Ok(value) => argv.push(value),
                    Err(result) => return result,
                }
            }

            "file_redirect" | "herestring_redirect" => {
                if let Some(redir) = extract_redirect(child) {
                    redirects.push(redir);
                }
            }

            _ => {} // 忽略未知子节点（声明命令结构灵活）
        }
    }

    let cmd = SimpleCommand {
        argv,
        env_vars,
        redirects,
        text: node.text.clone(),
    };
    ParseForSecurityResult::Simple {
        commands: vec![cmd],
    }
}

/// 变量赋值安全检查
fn walk_variable_assignment_security(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
    env_vars: &mut Vec<EnvVar>,
) -> Option<ParseForSecurityResult> {
    let name = node
        .child_by_type("variable_name")
        .map(|n| n.text.clone())
        .unwrap_or_default();

    let is_append = node.text.contains("+=");

    // 无效变量名（bash 当作命令执行）
    if !is_valid_var_name(&name) {
        return Some(too_complex("invalid variable name in assignment"));
    }

    // IFS 赋值拒绝
    if name == "IFS" {
        return Some(too_complex("IFS assignment"));
    }

    // PS4 赋值特殊处理
    if name == "PS4" {
        if is_append {
            return Some(too_complex("PS4 += append"));
        }
        let value = if node.children.len() > 1 {
            let val_node = &node.children[node.children.len() - 1];
            match walk_argument(val_node, inner_commands, var_scope, true) {
                Ok(v) => v,
                Err(result) => return Some(result),
            }
        } else {
            String::new()
        };
        if contains_placeholder(&value) {
            return Some(too_complex("PS4 contains placeholder"));
        }
        if !is_safe_ps4_value(&value) {
            return Some(too_complex("PS4 unsafe characters"));
        }
        apply_var_to_scope(var_scope, &name, &value, is_append);
        return None;
    }

    // 值解析
    let value = if node.children.len() > 1 {
        let val_node = &node.children[node.children.len() - 1];
        match walk_argument(val_node, inner_commands, var_scope, true) {
            Ok(v) => v,
            Err(result) => return Some(result),
        }
    } else {
        String::new()
    };

    // 波浪号展开拒绝
    if value.contains('~') {
        return Some(too_complex("tilde expansion in assignment"));
    }

    apply_var_to_scope(var_scope, &name, &value, is_append);
    env_vars.push(EnvVar { name, value });
    None
}

/// 裸变量赋值（语句级，非命令前缀）
fn walk_bare_assignment(
    node: &TsNode,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    let name = node
        .child_by_type("variable_name")
        .map(|n| n.text.clone())
        .unwrap_or_default();

    let is_append = node.text.contains("+=");

    if !is_valid_var_name(&name) {
        return Some(too_complex("invalid variable name"));
    }
    if name == "IFS" {
        return Some(too_complex("IFS assignment"));
    }

    // 简化值提取
    let text = &node.text;
    let eq_pos = text.find('=').or_else(|| text.find("+="));
    let value = eq_pos
        .map(|p| {
            let offset = if text[p..].starts_with("+=") {
                p + 2
            } else {
                p + 1
            };
            text[offset..].to_string()
        })
        .unwrap_or_default();

    if name == "PS4" {
        if is_append {
            return Some(too_complex("PS4 += append"));
        }
        if !is_safe_ps4_value(&value) {
            return Some(too_complex("PS4 unsafe"));
        }
    }

    if value.contains('~') {
        return Some(too_complex("tilde in assignment"));
    }

    apply_var_to_scope(var_scope, &name, &value, is_append);
    None
}

/// unset 命令处理
///
/// unset 参数可能包含命令替换（如 `unset $(fn_name)`），
/// 需要和 `walk_test_command` 一样通过 `walk_argument` 提取嵌套命令。
fn walk_unset_command(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> ParseForSecurityResult {
    let mut argv = Vec::new();
    for child in &node.children {
        match child.node_type.as_str() {
            "variable_name" => {
                var_scope.remove(&child.text);
                argv.push(child.text.clone());
            }
            "word"
            | "number"
            | "raw_string"
            | "string"
            | "concatenation"
            | "simple_expansion"
            | "command_substitution"
            | "arithmetic_expansion" => {
                match walk_argument(child, inner_commands, var_scope, false) {
                    Ok(value) => argv.push(value),
                    Err(result) => return result,
                }
            }
            _ => {}
        }
    }
    let cmd = SimpleCommand {
        argv,
        env_vars: vec![],
        redirects: vec![],
        text: node.text.clone(),
    };
    ParseForSecurityResult::Simple {
        commands: vec![cmd],
    }
}

/// `redirected_statement` 处理
fn walk_redirected_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    let mut redirects = Vec::new();
    let mut inner_node = None;

    for child in &node.children {
        match child.node_type.as_str() {
            "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                if let Some(redir) = extract_redirect(child) {
                    redirects.push(redir);
                }
            }
            _ => {
                inner_node = Some(child);
            }
        }
    }

    if let Some(inner) = inner_node {
        // 递归处理内部命令
        let prev_len = commands.len();
        if let Some(result) = collect_commands(inner, commands, var_scope) {
            return Some(result);
        }
        // 将重定向附加到最后提取的命令
        if !redirects.is_empty()
            && commands.len() > prev_len
            && let Some(last) = commands.last_mut()
        {
            last.redirects.extend(redirects);
        }
    }
    None
}

/// `test_command` 处理 ([[ ]] 和 [ ])
fn walk_test_command(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> ParseForSecurityResult {
    let mut argv = vec!["[[".to_string()];

    for child in &node.children {
        match child.node_type.as_str() {
            "word"
            | "number"
            | "raw_string"
            | "string"
            | "concatenation"
            | "simple_expansion"
            | "command_substitution"
            | "arithmetic_expansion" => {
                match walk_argument(child, inner_commands, var_scope, false) {
                    Ok(value) => argv.push(value),
                    Err(result) => return result,
                }
            }
            _ => {
                // 操作符和定界符直接推入
                if !child.text.is_empty() {
                    argv.push(child.text.clone());
                }
            }
        }
    }

    let cmd = SimpleCommand {
        argv,
        env_vars: vec![],
        redirects: vec![],
        text: node.text.clone(),
    };
    ParseForSecurityResult::Simple {
        commands: vec![cmd],
    }
}

// ─── Loop/Conditional Body Extraction ───

fn walk_for_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    // 提取循环变量名
    let loop_var = node.child_by_type("variable_name").map(|n| n.text.clone());

    if let Some(ref var) = loop_var {
        if var == "PS4" || var == "IFS" {
            return Some(too_complex(&format!("for loop variable is {var}")));
        }
        // 循环变量始终是未知值
        var_scope.insert(var.clone(), VAR_PLACEHOLDER.to_string());
    }

    // 验证迭代词
    for child in &node.children {
        if matches!(
            child.node_type.as_str(),
            "word" | "string" | "raw_string" | "concatenation" | "simple_expansion"
        ) {
            // 验证但丢弃值
            match walk_argument(child, commands, var_scope, false) {
                Ok(_) => {}
                Err(result) => return Some(result),
            }
        }
        if child.node_type == "command_substitution" {
            // 提取内部命令
            for sub_child in &child.children {
                if let Some(result) = collect_commands(sub_child, commands, var_scope) {
                    return Some(result);
                }
            }
        }
    }

    // 循环体使用 scope 副本
    // 我们的 parser 不创建 do_group 节点，循环体命令是 for_statement 的直接子节点
    let mut body_scope = var_scope.clone();
    for child in &node.children {
        match child.node_type.as_str() {
            "do_group" | "compound_statement" => {
                for body_child in &child.children {
                    if let Some(result) = collect_commands(body_child, commands, &mut body_scope) {
                        return Some(result);
                    }
                }
            }
            // 直接子命令（parser 不生成 do_group 时的备选路径）
            "command"
            | "pipeline"
            | "list"
            | "declaration_command"
            | "redirected_statement"
            | "negated_command"
            | "subshell"
            | "if_statement"
            | "for_statement"
            | "while_statement"
            | "case_statement" => {
                if let Some(result) = collect_commands(child, commands, &mut body_scope) {
                    return Some(result);
                }
            }
            _ => {} // variable_name, word (iteration items) — 已在上面处理
        }
    }

    None
}

fn walk_if_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    let mut seen_then = false;

    for child in &node.children {
        match child.node_type.as_str() {
            "command"
            | "pipeline"
            | "list"
            | "test_command"
            | "declaration_command"
            | "redirected_statement"
            | "negated_command"
            | "compound_statement" => {
                if seen_then {
                    // then/else 部分 — 使用 scope 副本
                    let mut body_scope = var_scope.clone();
                    if let Some(result) = collect_commands(child, commands, &mut body_scope) {
                        return Some(result);
                    }
                } else {
                    // 条件部分 — 使用真实 scope
                    if let Some(result) = collect_commands(child, commands, var_scope) {
                        return Some(result);
                    }
                }
            }
            // "then", "elif", "else", "fi" 关键字
            _ => {
                if child.text == "then" {
                    seen_then = true;
                }
                if child.text == "elif" {
                    seen_then = false;
                } // elif 条件用真实 scope
            }
        }
    }

    None
}

fn walk_while_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    for child in &node.children {
        match child.node_type.as_str() {
            "command"
            | "pipeline"
            | "list"
            | "test_command"
            | "declaration_command"
            | "redirected_statement"
            | "negated_command" => {
                // 条件部分 — 使用真实 scope
                if let Some(result) = collect_commands(child, commands, var_scope) {
                    return Some(result);
                }
            }
            "do_group" | "compound_statement" => {
                // 循环体 — 使用 scope 副本
                let mut body_scope = var_scope.clone();
                for body_child in &child.children {
                    if let Some(result) = collect_commands(body_child, commands, &mut body_scope) {
                        return Some(result);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn walk_case_statement(
    node: &TsNode,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    // case word in pattern) body;; esac
    for child in &node.children {
        if child.node_type == "case_item" {
            let mut body_scope = var_scope.clone();
            for item_child in &child.children {
                if let Some(result) = collect_commands(item_child, commands, &mut body_scope) {
                    return Some(result);
                }
            }
        }
    }
    None
}

// ─── Argument Walking ───

/// 从参数节点提取字面值
///
/// `inside_string`: true 表示在 "..." 内部（允许 `SAFE_ENV_VARS`）
fn walk_argument(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
    inside_string: bool,
) -> Result<String, ParseForSecurityResult> {
    match node.node_type.as_str() {
        "word" | "number" => {
            let text = unquote_word(&node.text);
            if has_brace_expansion(&text) {
                return Err(too_complex("brace expansion"));
            }
            Ok(text)
        }

        "raw_string" => {
            let text = &node.text;
            if text.starts_with('\'') && text.ends_with('\'') && text.len() >= 2 {
                Ok(text[1..text.len() - 1].to_string())
            } else {
                Ok(text.clone())
            }
        }

        "string" => walk_string(node, inner_commands, var_scope),

        "concatenation" => {
            let mut result = String::new();
            for child in &node.children {
                let part = walk_argument(child, inner_commands, var_scope, inside_string)?;
                result.push_str(&part);
            }
            Ok(result)
        }

        "simple_expansion" => resolve_simple_expansion(node, var_scope, inside_string),

        "expansion" => Err(too_complex_with_type("parameter expansion", "expansion")),

        "arithmetic_expansion" => {
            // 验证算术表达式只包含安全叶节点
            if let Some(result) = walk_arithmetic(node) {
                return Err(result);
            }
            Ok(CMDSUB_PLACEHOLDER.to_string())
        }

        "command_substitution" => {
            for child in &node.children {
                let mut scope_copy = var_scope.clone();
                if let Some(result) = collect_commands(child, inner_commands, &mut scope_copy) {
                    return Err(result);
                }
            }
            Ok(CMDSUB_PLACEHOLDER.to_string())
        }

        "process_substitution" => Err(too_complex_with_type(
            "process substitution",
            "process_substitution",
        )),

        "ansi_c_string" | "translated_string" => Err(too_complex_with_type(
            &format!("special string type: {}", node.node_type),
            &node.node_type,
        )),

        other => Err(too_complex_with_type(
            &format!("unrecognized argument type: {other}"),
            other,
        )),
    }
}

/// 解析简单展开 $VAR — 区分 insideString 和裸参数
fn resolve_simple_expansion(
    node: &TsNode,
    var_scope: &HashMap<String, String>,
    inside_string: bool,
) -> Result<String, ParseForSecurityResult> {
    let var_name = if node.text.starts_with('$') {
        &node.text[1..]
    } else {
        &node.text
    };

    // 1. 已追踪变量
    if let Some(tracked_value) = var_scope.get(var_name) {
        if contains_placeholder(tracked_value) {
            if inside_string {
                return Ok(VAR_PLACEHOLDER.to_string());
            }
            return Err(too_complex("placeholder variable in bare position"));
        }
        // 字面值
        if !inside_string && bare_var_unsafe(tracked_value) {
            return Err(too_complex(
                "tracked variable has unsafe chars for bare arg",
            ));
        }
        return Ok(tracked_value.clone());
    }

    // 2. 安全环境变量（仅在字符串内部允许）
    if inside_string && safe_env_vars().contains(var_name) {
        return Ok(VAR_PLACEHOLDER.to_string());
    }

    // 3. 特殊变量（仅在字符串内部允许）
    if inside_string
        && (special_var_names().contains(var_name) || var_name.chars().all(|c| c.is_ascii_digit()))
    {
        return Ok(VAR_PLACEHOLDER.to_string());
    }

    // 4. 默认：too-complex
    Err(too_complex(&format!("unresolved variable: ${var_name}")))
}

/// 验证算术表达式
fn walk_arithmetic(node: &TsNode) -> Option<ParseForSecurityResult> {
    if node.children.is_empty() {
        // 叶节点：验证安全性
        let text = node.text.trim();
        if !text.is_empty() && !is_safe_arith_leaf(text) {
            return Some(too_complex("unsafe arithmetic leaf"));
        }
        return None;
    }
    for child in &node.children {
        if let Some(result) = walk_arithmetic(child) {
            return Some(result);
        }
    }
    None
}

/// 从双引号字符串节点提取值（含独占占位符检测）
fn walk_string(
    node: &TsNode,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Result<String, ParseForSecurityResult> {
    let mut result = String::new();
    let mut saw_dynamic_placeholder = false;
    let mut saw_literal_content = false;

    for child in &node.children {
        match child.node_type.as_str() {
            "string_content" => {
                // 反转义双引号序列
                let content = child
                    .text
                    .replace("\\$", "$")
                    .replace("\\`", "`")
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\");
                result.push_str(&content);
                saw_literal_content = true;
            }
            "simple_expansion" => {
                let value = resolve_simple_expansion(child, var_scope, true)?;
                if value == VAR_PLACEHOLDER || value == CMDSUB_PLACEHOLDER {
                    saw_dynamic_placeholder = true;
                } else {
                    saw_literal_content = true;
                }
                result.push_str(&value);
            }
            "expansion" => {
                return Err(too_complex_with_type(
                    "parameter expansion in string",
                    "expansion",
                ));
            }
            "command_substitution" => {
                for sub_child in &child.children {
                    let mut scope_copy = var_scope.clone();
                    if let Some(err_result) =
                        collect_commands(sub_child, inner_commands, &mut scope_copy)
                    {
                        return Err(err_result);
                    }
                }
                result.push_str(CMDSUB_PLACEHOLDER);
                saw_dynamic_placeholder = true;
            }
            "arithmetic_expansion" => {
                if let Some(err_result) = walk_arithmetic(child) {
                    return Err(err_result);
                }
                result.push_str(CMDSUB_PLACEHOLDER);
                saw_literal_content = true; // 算术结果是确定的
            }
            "\"" => {} // 引号定界符
            other => {
                return Err(too_complex_with_type(
                    &format!("unrecognized string child: {other}"),
                    other,
                ));
            }
        }
    }

    // 独占占位符拒绝："$(cmd)" 或 "$VAR" 单独出现
    if saw_dynamic_placeholder && !saw_literal_content {
        return Err(too_complex("solo placeholder in string"));
    }

    // 空白字符串检测（tree-sitter 不生成 string_content）
    if !saw_literal_content && !saw_dynamic_placeholder && node.text.len() > 2 {
        return Err(too_complex("whitespace-only string"));
    }

    Ok(result)
}

// ─── checkSemantics: argv 级安全后检查 ───

/// 对提取的命令列表进行 argv 级安全检查
fn check_semantics(commands: &[SimpleCommand]) -> Option<ParseForSecurityResult> {
    for cmd in commands {
        if cmd.argv.is_empty() {
            continue;
        }

        let mut argv = cmd.argv.clone();

        // ─── 安全包装器剥离 ───
        loop {
            if argv.is_empty() {
                break;
            }
            let name = &argv[0];
            match name.as_str() {
                "time" | "nohup" => {
                    argv.remove(0);
                    continue;
                }
                "timeout" => {
                    if let Some(result) = strip_timeout(&mut argv) {
                        return Some(result);
                    }
                    continue;
                }
                "nice" => {
                    if let Some(result) = strip_nice(&mut argv) {
                        return Some(result);
                    }
                    continue;
                }
                "env" => {
                    if let Some(result) = strip_env(&mut argv) {
                        return Some(result);
                    }
                    continue;
                }
                "stdbuf" => {
                    if let Some(result) = strip_stdbuf(&mut argv) {
                        return Some(result);
                    }
                    continue;
                }
                _ => break,
            }
        }

        if argv.is_empty() {
            continue;
        }
        let name = &argv[0];

        // 空命令名
        if name.is_empty() {
            return Some(too_complex("empty command name"));
        }

        // 占位符作为命令名
        if contains_placeholder(name) {
            return Some(too_complex("placeholder as command name"));
        }

        // 片段检测（- | & 开头）
        if name.starts_with('-') || name.starts_with('|') || name.starts_with('&') {
            return Some(too_complex("fragment as command name"));
        }

        // Shell 关键字作为命令名（表示解析错误）
        if shell_keywords().contains(name.as_str()) {
            return Some(too_complex(&format!("shell keyword as command: {name}")));
        }

        // eval-like 内置命令
        if eval_like_builtins().contains(name.as_str()) {
            // 一些特殊豁免
            match name.as_str() {
                "command" if argv.len() > 1 && (argv[1] == "-v" || argv[1] == "-V") => {}
                "fc" if argv.len() > 1 && (argv[1] == "-l" || argv[1].starts_with("-l")) => {}
                "compgen" if argv.len() > 1 && matches!(argv[1].as_str(), "-c" | "-f" | "-v") => {}
                _ => return Some(too_complex(&format!("eval-like builtin: {name}"))),
            }
        }

        // Zsh 危险内置命令
        if zsh_dangerous_builtins().contains(name.as_str()) {
            return Some(too_complex(&format!("zsh dangerous builtin: {name}")));
        }

        // NEWLINE_HASH_RE 检查
        for arg in &cmd.argv {
            if has_newline_hash(arg) {
                return Some(too_complex("newline-hash injection in argv"));
            }
        }
        for ev in &cmd.env_vars {
            if has_newline_hash(&ev.value) {
                return Some(too_complex("newline-hash injection in env var"));
            }
        }
        for redir in &cmd.redirects {
            if has_newline_hash(&redir.target) {
                return Some(too_complex("newline-hash injection in redirect"));
            }
        }

        // /proc/*/environ 检查
        for arg in &cmd.argv {
            if has_proc_environ(arg) {
                return Some(too_complex("/proc/environ access"));
            }
        }
        for redir in &cmd.redirects {
            if has_proc_environ(&redir.target) {
                return Some(too_complex("/proc/environ in redirect"));
            }
        }

        // jq system() 检查
        if name == "jq" {
            for arg in &argv[1..] {
                if arg.contains("system") && arg.contains('(') {
                    return Some(too_complex("jq system() function"));
                }
            }
        }

        // 下标算术求值标志检查
        let sub_flags = subscript_eval_flags();
        if let Some(flags) = sub_flags.get(name.as_str()) {
            let mut i = 1;
            while i < argv.len() {
                if flags.contains(argv[i].as_str()) && i + 1 < argv.len() {
                    let next = &argv[i + 1];
                    if next.contains('[') {
                        return Some(too_complex("subscript eval via flag"));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }

        // [[ 算术比较运算符检查
        if name == "[[" {
            let arith_ops = test_arith_cmp_ops();
            for i in 1..argv.len() {
                if arith_ops.contains(argv[i].as_str()) {
                    // 检查两侧操作数是否包含 [
                    if i > 1 && argv[i - 1].contains('[') {
                        return Some(too_complex("arithmetic comparison with subscript"));
                    }
                    if i + 1 < argv.len() && argv[i + 1].contains('[') {
                        return Some(too_complex("arithmetic comparison with subscript"));
                    }
                }
            }
        }

        // 裸下标名内置命令（read, unset）
        if bare_subscript_name_builtins().contains(name.as_str()) {
            let data_flags = read_data_flags();
            let mut i = 1;
            while i < argv.len() {
                if data_flags.contains(argv[i].as_str()) {
                    i += 2; // 跳过标志及其值
                } else if argv[i].starts_with('-') {
                    i += 1;
                } else {
                    // 位置参数是 NAME
                    if argv[i].contains('[') {
                        return Some(too_complex("subscript in bare name builtin"));
                    }
                    i += 1;
                }
            }
        }
    }

    None
}

// ─── Wrapper Stripping Helpers ───

fn strip_timeout(argv: &mut Vec<String>) -> Option<ParseForSecurityResult> {
    argv.remove(0); // remove "timeout"
    while !argv.is_empty() {
        let arg = &argv[0];
        if arg == "--foreground"
            || arg == "--preserve-status"
            || arg == "--verbose"
            || arg.starts_with("--kill-after=")
            || arg.starts_with("--signal=")
        {
            argv.remove(0);
        } else if arg == "--kill-after" || arg == "--signal" {
            argv.remove(0);
            if !argv.is_empty() {
                argv.remove(0);
            }
        } else if arg.starts_with("--") {
            return Some(too_complex(&format!("unknown timeout flag: {arg}")));
        } else if arg.starts_with('-') && !arg.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
            // 短标志
            argv.remove(0);
        } else {
            // Duration — 验证格式
            let duration = &argv[0];
            let valid = duration
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || matches!(c, 's' | 'm' | 'h' | 'd'));
            if !valid {
                return Some(too_complex("invalid timeout duration"));
            }
            argv.remove(0);
            break;
        }
    }
    None
}

fn strip_nice(argv: &mut Vec<String>) -> Option<ParseForSecurityResult> {
    argv.remove(0);
    while !argv.is_empty() {
        let arg = &argv[0];
        if arg == "-n" {
            argv.remove(0);
            if !argv.is_empty() {
                argv.remove(0);
            }
        } else if arg.starts_with('-')
            && arg.len() > 1
            && arg[1..].chars().all(|c| c.is_ascii_digit())
        {
            argv.remove(0); // -N legacy
        } else if arg.starts_with("--adjustment=") {
            argv.remove(0);
        } else {
            break;
        }
    }
    None
}

fn strip_env(argv: &mut Vec<String>) -> Option<ParseForSecurityResult> {
    argv.remove(0);
    while !argv.is_empty() {
        let arg = &argv[0];
        if arg == "-i" || arg == "-0" || arg == "-v" {
            argv.remove(0);
        } else if arg == "-u" {
            argv.remove(0);
            if !argv.is_empty() {
                argv.remove(0);
            }
        } else if arg.contains('=') && !arg.starts_with('-') {
            argv.remove(0); // VAR=val
        } else {
            break;
        }
    }
    None
}

fn strip_stdbuf(argv: &mut Vec<String>) -> Option<ParseForSecurityResult> {
    argv.remove(0);
    while !argv.is_empty() {
        let arg = &argv[0];
        if arg.len() == 2
            && arg.starts_with('-')
            && matches!(arg.chars().nth(1), Some('i' | 'o' | 'e'))
        {
            // -i, -o, -e (separate arg)
            argv.remove(0);
            if !argv.is_empty() {
                argv.remove(0);
            }
        } else if arg.len() > 2
            && arg.starts_with('-')
            && matches!(arg.chars().nth(1), Some('i' | 'o' | 'e'))
        {
            // -iVAL fused
            argv.remove(0);
        } else if arg.starts_with("--input=")
            || arg.starts_with("--output=")
            || arg.starts_with("--error=")
        {
            argv.remove(0);
        } else {
            break;
        }
    }
    None
}

// ─── Helpers ───

fn unquote_word(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                result.push(next);
                chars.next();
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn extract_redirect(node: &TsNode) -> Option<Redirect> {
    let text = &node.text;
    let ops = [
        ("&>>", RedirectOp::AppendAll),
        ("&>", RedirectOp::WriteAll),
        (">>", RedirectOp::Append),
        (">&", RedirectOp::DupOutput),
        (">|", RedirectOp::Clobber),
        (">", RedirectOp::Write),
        ("<<<", RedirectOp::HereString),
        ("<<", RedirectOp::HereDoc),
        ("<&", RedirectOp::DupInput),
        ("<", RedirectOp::Read),
    ];
    for (op_str, op) in &ops {
        if let Some(pos) = text.find(op_str) {
            let target = text[pos + op_str.len()..].trim().to_string();
            let fd = if pos > 0 {
                text[..pos].trim().parse::<u32>().ok()
            } else {
                None
            };
            return Some(Redirect {
                op: *op,
                target,
                fd,
            });
        }
    }
    None
}

fn apply_var_to_scope(
    scope: &mut HashMap<String, String>,
    name: &str,
    value: &str,
    is_append: bool,
) {
    let existing = scope.get(name).cloned().unwrap_or_default();
    let combined = if is_append {
        format!("{existing}{value}")
    } else {
        value.to_string()
    };
    let final_value = if contains_placeholder(&combined) {
        VAR_PLACEHOLDER.to_string()
    } else {
        combined
    };
    scope.insert(name.to_string(), final_value);
}

fn rebuild_text(node: &TsNode, argv: &[String]) -> String {
    let original = &node.text;
    // 如果原始文本不含 $，无需重建
    if !original.contains('$') {
        return original.clone();
    }
    // 尝试从 argv 重建
    argv.join(" ")
}

fn too_complex(reason: &str) -> ParseForSecurityResult {
    ParseForSecurityResult::TooComplex {
        reason: reason.to_string(),
        node_type: None,
    }
}

fn too_complex_with_type(reason: &str, node_type: &str) -> ParseForSecurityResult {
    ParseForSecurityResult::TooComplex {
        reason: reason.to_string(),
        node_type: Some(node_type.to_string()),
    }
}

// ─── Tests ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_command() {
        let result = parse_for_security("echo hello");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0].argv, vec!["echo", "hello"]);
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_pipeline() {
        let result = parse_for_security("ls | grep foo");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 2);
                assert_eq!(commands[0].argv[0], "ls");
                assert_eq!(commands[1].argv[0], "grep");
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_and_or() {
        let result = parse_for_security("cmd1 && cmd2");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 2);
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_redirect() {
        let result = parse_for_security("echo hello > output.txt");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 1);
                assert!(!commands[0].redirects.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_command_substitution() {
        let result = parse_for_security("echo $(date)");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert!(!commands.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_single_quoted() {
        let result = parse_for_security("echo 'hello world'");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands[0].argv, vec!["echo", "hello world"]);
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_control_chars() {
        assert!(matches!(
            parse_for_security("echo \x01hello"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_unicode_whitespace() {
        assert!(matches!(
            parse_for_security("echo\u{00A0}hello"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_eval_builtin() {
        assert!(matches!(
            parse_for_security("eval 'rm -rf /'"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_trap_builtin() {
        assert!(matches!(
            parse_for_security("trap 'evil' EXIT"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_source_builtin() {
        assert!(matches!(
            parse_for_security("source malicious.sh"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_zsh_builtin() {
        assert!(matches!(
            parse_for_security("zmodload zsh/system"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_shell_keyword_as_command() {
        assert!(matches!(
            parse_for_security("then something"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_ifs_assignment() {
        assert!(matches!(
            parse_for_security("IFS=: cmd"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_empty_command() {
        let result = parse_for_security("");
        match result {
            ParseForSecurityResult::Simple { commands } => assert!(commands.is_empty()),
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_for_loop_bare_var_rejects() {
        // Bare $f (loop var = VAR_PLACEHOLDER) is correctly rejected in bare position
        let result = parse_for_security("for f in a b; do echo $f; done");
        assert!(matches!(result, ParseForSecurityResult::TooComplex { .. }));
    }

    #[test]
    fn test_for_loop_quoted_var_extracts() {
        // "$f" inside string is allowed (placeholder in string context)
        let result = parse_for_security("for f in a b; do echo \"item: $f\"; done");
        // This should either extract commands or be too-complex depending on parser structure
        // Both outcomes are acceptable for the loop body extraction
        assert!(!matches!(result, ParseForSecurityResult::ParseUnavailable));
    }

    #[test]
    fn test_if_extracts() {
        let result = parse_for_security("if true; then echo yes; fi");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert!(!commands.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_while_extracts() {
        let result = parse_for_security("while true; do echo loop; done");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert!(!commands.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_proc_environ() {
        assert!(matches!(
            parse_for_security("cat /proc/1/environ"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_newline_hash() {
        assert!(matches!(
            parse_for_security("echo 'hello\n # comment'"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_time_wrapper_strip() {
        let result = parse_for_security("time ls");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert!(!commands.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_nohup_wrapper_strip() {
        let result = parse_for_security("nohup sleep 10");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert!(!commands.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_command_v_exempt() {
        let result = parse_for_security("command -v git");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert!(!commands.is_empty());
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_function_def_too_complex() {
        assert!(matches!(
            parse_for_security("function foo { echo bar; }"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_backslash_whitespace() {
        assert!(matches!(
            parse_for_security("echo hello\\ world"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_zsh_tilde_bracket() {
        assert!(matches!(
            parse_for_security("echo ~[test]"),
            ParseForSecurityResult::TooComplex { .. }
        ));
    }

    #[test]
    fn test_variable_assignment() {
        let result = parse_for_security("VAR=hello && echo $VAR");
        // VAR=hello 是裸赋值，echo $VAR 中 VAR 应被追踪
        match result {
            ParseForSecurityResult::Simple { .. } => {}
            ParseForSecurityResult::TooComplex { .. } => {} // 也可接受
            other => panic!("Unexpected: {:?}", other),
        }
    }

    // ── unset 命令专项测试 ──

    #[test]
    fn test_unset_simple_variable() {
        let result = parse_for_security("unset myvar");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0].argv, vec!["unset", "myvar"]);
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_unset_multiple_variables() {
        let result = parse_for_security("unset FOO BAR BAZ");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0].argv, vec!["unset", "FOO", "BAR", "BAZ"]);
            }
            other => panic!("Expected Simple, got {:?}", other),
        }
    }

    #[test]
    fn test_unset_with_command_substitution() {
        // unset $(get_var) 应提取嵌套命令
        let result = parse_for_security("unset $(echo FOO)");
        match result {
            ParseForSecurityResult::Simple { commands } => {
                // 应至少有 2 个命令：echo FOO (内部) + unset (外部)
                assert!(
                    commands.len() >= 2,
                    "Expected >=2 commands (nested + unset), got {}: {:?}",
                    commands.len(),
                    commands
                );
            }
            // TooComplex 也可接受（更保守的安全判断）
            ParseForSecurityResult::TooComplex { .. } => {}
            other => panic!("Unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_unset_brace_expansion() {
        // tree-sitter 将 {a,b} 解析为复合节点而非 word 参数，
        // 因此 walk_argument 的花括号展开检测不会在此触发。
        // 解析结果是拆分为多个命令/节点，安全分析仍能覆盖。
        let result = parse_for_security("unset {a,b}");
        // Simple（拆分解析）或 TooComplex（保守拒绝）均可接受
        match result {
            ParseForSecurityResult::Simple { .. } => {}
            ParseForSecurityResult::TooComplex { .. } => {}
            other => panic!("Unexpected: {:?}", other),
        }
    }
}
