//! `TreeSitter` 兼容分析结构
//!
//! 等价于 TS treeSitterAnalysis.ts

use crate::{CompoundStructure, DangerousPatterns, QuoteContext, TreeSitterAnalysis, TsNode};

/// 从 AST 提取综合安全分析数据
#[must_use]
pub fn analyze_command(root: &TsNode, command: &str) -> TreeSitterAnalysis {
    TreeSitterAnalysis {
        quote_context: extract_quote_context(root, command),
        compound_structure: extract_compound_structure(root, command),
        has_actual_operator_nodes: has_actual_operator_nodes(root),
        dangerous_patterns: extract_dangerous_patterns(root),
    }
}

/// 提取引用上下文
#[must_use]
pub fn extract_quote_context(_root: &TsNode, command: &str) -> QuoteContext {
    let mut with_double_quotes = String::new();
    let mut fully_unquoted = String::new();
    let mut unquoted_keep_quote_chars = String::new();

    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                if in_single {
                    in_single = false;
                    unquoted_keep_quote_chars.push('\'');
                } else {
                    in_single = true;
                    unquoted_keep_quote_chars.push('\'');
                    // 跳过单引号内容（从 with_double_quotes 中移除）
                    let mut content = String::new();
                    while let Some(&next) = chars.peek() {
                        if next == '\'' {
                            in_single = false;
                            chars.next();
                            unquoted_keep_quote_chars.push('\'');
                            break;
                        }
                        content.push(next);
                        chars.next();
                    }
                    // 单引号内容不出现在 with_double_quotes 和 fully_unquoted 中
                }
                continue;
            }
            '"' if !in_single => {
                in_double = !in_double;
                with_double_quotes.push('"');
                unquoted_keep_quote_chars.push('"');
                continue;
            }
            '\\' if !in_single => {
                if let Some(&next) = chars.peek() {
                    with_double_quotes.push('\\');
                    with_double_quotes.push(next);
                    fully_unquoted.push(next);
                    unquoted_keep_quote_chars.push('\\');
                    unquoted_keep_quote_chars.push(next);
                    chars.next();
                    continue;
                }
            }
            _ => {}
        }

        if !in_single {
            with_double_quotes.push(c);
        }
        if !in_single && !in_double {
            fully_unquoted.push(c);
        }
        unquoted_keep_quote_chars.push(c);
    }

    QuoteContext {
        with_double_quotes,
        fully_unquoted,
        unquoted_keep_quote_chars,
    }
}

/// 提取复合结构
#[must_use]
pub fn extract_compound_structure(root: &TsNode, command: &str) -> CompoundStructure {
    let mut structure = CompoundStructure::default();

    visit_node(root, &mut |node| {
        match node.node_type.as_str() {
            "pipeline" => structure.has_pipeline = true,
            "subshell" => structure.has_subshell = true,
            "compound_statement" => structure.has_command_group = true,
            "list" => {
                // 查找操作符
                for child in &node.children {
                    match child.node_type.as_str() {
                        "&&" | "||" => {
                            structure.has_compound_operators = true;
                            structure.operators.push(child.node_type.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    });

    // 提取段
    structure.segments = super::commands::split_command_with_operators(command);

    structure
}

/// 检查是否存在真正的操作符节点（非转义）
#[must_use]
pub fn has_actual_operator_nodes(root: &TsNode) -> bool {
    let mut found = false;
    visit_node(root, &mut |node| {
        if matches!(node.node_type.as_str(), "&&" | "||" | "list" | "pipeline") {
            found = true;
        }
    });
    found
}

/// 提取危险模式
#[must_use]
pub fn extract_dangerous_patterns(root: &TsNode) -> DangerousPatterns {
    let mut patterns = DangerousPatterns::default();

    visit_node(root, &mut |node| match node.node_type.as_str() {
        "command_substitution" => patterns.has_command_substitution = true,
        "process_substitution" => patterns.has_process_substitution = true,
        "expansion" | "simple_expansion" => patterns.has_parameter_expansion = true,
        "heredoc_redirect" => patterns.has_heredoc = true,
        "comment" => patterns.has_comment = true,
        _ => {}
    });

    patterns
}

/// 递归访问所有节点
fn visit_node(node: &TsNode, visitor: &mut impl FnMut(&TsNode)) {
    visitor(node);
    for child in &node.children {
        visit_node(child, visitor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParseResult;
    use crate::bash::parser::parse_source;

    #[test]
    fn test_analyze_simple_command() {
        if let ParseResult::Success(root) = parse_source("echo hello", 0) {
            let analysis = analyze_command(&root, "echo hello");
            assert!(!analysis.compound_structure.has_pipeline);
            assert!(!analysis.compound_structure.has_compound_operators);
            assert!(!analysis.dangerous_patterns.has_command_substitution);
        }
    }

    #[test]
    fn test_analyze_pipeline() {
        if let ParseResult::Success(root) = parse_source("ls | grep foo", 0) {
            let analysis = analyze_command(&root, "ls | grep foo");
            assert!(analysis.compound_structure.has_pipeline);
            assert!(analysis.has_actual_operator_nodes);
        }
    }

    #[test]
    fn test_quote_context() {
        let ctx =
            extract_quote_context(&TsNode::new("program", "", 0, 0), "echo 'hello' \"world\"");
        assert!(!ctx.fully_unquoted.contains("hello"));
        assert!(ctx.with_double_quotes.contains("world"));
    }
}
