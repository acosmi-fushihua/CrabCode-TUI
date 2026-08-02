//! 权限规则解析
//!
//! 等价于 TS permissionRuleParser.ts
//! 解析 "Bash(npm:*)" 格式的规则字符串

use crate::types::PermissionRuleValue;

/// 解析权限规则字符串为 `PermissionRuleValue`
///
/// 格式: "Tool" 或 "Tool(content)"
/// 支持转义: \( \) \\
#[must_use]
pub fn parse_permission_rule(rule_str: &str) -> Option<PermissionRuleValue> {
    let rule = rule_str.trim();
    if rule.is_empty() {
        return None;
    }

    // 查找第一个未转义的 (
    let paren_start = find_first_unescaped(rule, '(');

    if let Some(start) = paren_start {
        // 查找最后一个未转义的 )
        let paren_end = find_last_unescaped(rule, ')');
        if let Some(end) = paren_end
            && end > start
        {
            let tool_name = normalize_tool_name(rule[..start].trim());
            let raw_content = &rule[start + 1..end];
            let content = unescape_rule_content(raw_content);
            return Some(PermissionRuleValue {
                tool_name,
                rule_content: if content.is_empty() {
                    None
                } else {
                    Some(content)
                },
            });
        }
    }

    // 无括号 → 纯工具名
    Some(PermissionRuleValue {
        tool_name: normalize_tool_name(rule),
        rule_content: None,
    })
}

/// 将 `PermissionRuleValue` 序列化为字符串
#[must_use]
pub fn permission_rule_to_string(value: &PermissionRuleValue) -> String {
    match &value.rule_content {
        Some(content) => {
            let escaped = escape_rule_content(content);
            format!("{}({})", value.tool_name, escaped)
        }
        None => value.tool_name.clone(),
    }
}

/// 转义规则内容（顺序关键：先 \ 再 ()）
#[must_use]
pub fn escape_rule_content(content: &str) -> String {
    content
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

/// 反转义规则内容（顺序反转：先 () 再 \）
#[must_use]
pub fn unescape_rule_content(content: &str) -> String {
    content
        .replace("\\(", "(")
        .replace("\\)", ")")
        .replace("\\\\", "\\")
}

/// 查找第一个未转义的字符
fn find_first_unescaped(s: &str, ch: char) -> Option<usize> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == ch as u8 {
            // 计算前面的反斜杠数量
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            // 偶数个反斜杠 = 未转义
            if backslashes % 2 == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// 查找最后一个未转义的字符
fn find_last_unescaped(s: &str, ch: char) -> Option<usize> {
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == ch as u8 {
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// 规范化旧版工具名
fn normalize_tool_name(name: &str) -> String {
    match name {
        "Task" => "Agent".to_string(),
        "KillShell" => "TaskStop".to_string(),
        "AgentOutputTool" | "BashOutputTool" => "TaskOutput".to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_tool() {
        let result = parse_permission_rule("Bash").unwrap();
        assert_eq!(result.tool_name, "Bash");
        assert!(result.rule_content.is_none());
    }

    #[test]
    fn test_tool_with_content() {
        let result = parse_permission_rule("Bash(npm install)").unwrap();
        assert_eq!(result.tool_name, "Bash");
        assert_eq!(result.rule_content.as_deref(), Some("npm install"));
    }

    #[test]
    fn test_tool_with_prefix() {
        let result = parse_permission_rule("Bash(npm:*)").unwrap();
        assert_eq!(result.tool_name, "Bash");
        assert_eq!(result.rule_content.as_deref(), Some("npm:*"));
    }

    #[test]
    fn test_escaped_parens() {
        let result = parse_permission_rule("Bash(echo \\(hello\\))").unwrap();
        assert_eq!(result.rule_content.as_deref(), Some("echo (hello)"));
    }

    #[test]
    fn test_legacy_tool_name() {
        let result = parse_permission_rule("Task").unwrap();
        assert_eq!(result.tool_name, "Agent");
    }

    #[test]
    fn test_roundtrip() {
        let value = PermissionRuleValue {
            tool_name: "Bash".to_string(),
            rule_content: Some("npm:*".to_string()),
        };
        let s = permission_rule_to_string(&value);
        let parsed = parse_permission_rule(&s).unwrap();
        assert_eq!(parsed.tool_name, value.tool_name);
        assert_eq!(parsed.rule_content, value.rule_content);
    }

    #[test]
    fn test_escape_order() {
        // Escaping: \ first, then ()
        assert_eq!(escape_rule_content("a\\b(c)"), "a\\\\b\\(c\\)");
        // Unescaping: () first, then \
        assert_eq!(unescape_rule_content("a\\\\b\\(c\\)"), "a\\b(c)");
    }
}
