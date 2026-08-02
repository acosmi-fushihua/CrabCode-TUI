//! Shell 规则匹配
//!
//! 等价于 TS shellRuleMatching.ts
//! 支持精确匹配、前缀匹配（:*）和通配符匹配

use crate::types::ShellPermissionRule;

/// 解析 shell 权限规则字符串
#[must_use]
pub fn parse_shell_rule(rule: &str) -> ShellPermissionRule {
    // 1. 旧版 :* 前缀语法
    if let Some(prefix) = extract_prefix(rule) {
        return ShellPermissionRule::Prefix { prefix };
    }
    // 2. 通配符语法（包含未转义的 *）
    if has_wildcards(rule) {
        return ShellPermissionRule::Wildcard {
            pattern: rule.to_string(),
        };
    }
    // 3. 精确匹配
    ShellPermissionRule::Exact {
        command: rule.to_string(),
    }
}

/// 提取旧版 :* 前缀
#[must_use]
pub fn extract_prefix(rule: &str) -> Option<String> {
    if let Some(prefix) = rule.strip_suffix(":*")
        && !prefix.is_empty()
    {
        return Some(prefix.to_string());
    }
    None
}

/// 检测未转义的通配符
///
/// TS 逻辑：如果模式以 `:*` 结尾，整个模式是旧版前缀，返回 false。
/// 否则，任何未转义的 `*` 都表示通配符。
#[must_use]
pub fn has_wildcards(pattern: &str) -> bool {
    // 旧版 :* 前缀语法：如果整个模式以 :* 结尾，不是通配符
    if pattern.ends_with(":*") {
        return false;
    }
    let bytes = pattern.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'*' {
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                return true;
            }
        }
    }
    false
}

/// 匹配命令是否符合规则
#[must_use]
pub fn matches_rule(rule: &ShellPermissionRule, command: &str) -> bool {
    match rule {
        ShellPermissionRule::Exact { command: rule_cmd } => command == rule_cmd,
        ShellPermissionRule::Prefix { prefix } => {
            command == prefix || command.starts_with(&format!("{prefix} "))
        }
        ShellPermissionRule::Wildcard { pattern } => {
            match_wildcard_pattern(pattern, command, false)
        }
    }
}

/// 通配符模式匹配
///
/// 算法：
/// 1. 转义的 \* → 字面量星号占位符
/// 2. 转义的 \\\\ → 字面量反斜杠占位符
/// 3. 转义正则特殊字符
/// 4. 未转义 * → .*
/// 5. 还原占位符
/// 6. 特殊：末尾 ` *` 使尾部空格+参数可选
#[must_use]
pub fn match_wildcard_pattern(pattern: &str, command: &str, case_insensitive: bool) -> bool {
    const ESCAPED_STAR: &str = "\x00ES\x00";
    const ESCAPED_BACKSLASH: &str = "\x00EB\x00";

    // 1. 处理转义序列
    let mut processed = pattern.replace("\\*", ESCAPED_STAR);
    processed = processed.replace("\\\\", ESCAPED_BACKSLASH);

    // 2. 转义正则特殊字符（除 * 外）
    let mut regex_str = String::new();
    for ch in processed.chars() {
        if matches!(
            ch,
            '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' | '\''
        ) {
            regex_str.push('\\');
        }
        regex_str.push(ch);
    }

    // 3. 未转义 * → .*
    regex_str = regex_str.replace('*', ".*");

    // 4. 还原占位符
    regex_str = regex_str.replace(ESCAPED_STAR, "\\*");
    regex_str = regex_str.replace(ESCAPED_BACKSLASH, "\\\\");

    // 5. 末尾 ` .*` 使尾部可选（仅当只有一个未转义 * 时）
    // TS: unescapedStarCount === 1 时才启用此优化
    let unescaped_star_count = {
        let mut count = 0;
        let pb = processed.as_bytes();
        for i in 0..pb.len() {
            if pb[i] == b'*' {
                let mut bs = 0;
                let mut j = i;
                while j > 0 && pb[j - 1] == b'\\' {
                    bs += 1;
                    j -= 1;
                }
                if bs % 2 == 0 {
                    count += 1;
                }
            }
        }
        count
    };
    if regex_str.ends_with(" .*") && unescaped_star_count == 1 {
        let prefix = &regex_str[..regex_str.len() - 3];
        regex_str = format!("(?:{prefix} .*|{prefix})");
    }

    // 6. 构建完整正则
    let full_regex = format!("^(?s){regex_str}$");

    // 简单手动匹配（避免依赖 regex crate）
    simple_regex_match(&full_regex, command, case_insensitive)
}

/// 简化的正则匹配（处理 .* 和字面量）
///
/// 注意：这是一个简化实现，覆盖常见的通配符模式。
/// 完整的正则引擎需要 regex crate。
fn simple_regex_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    // 简化策略：将 .* 转换为贪婪匹配点
    let text = if case_insensitive {
        text.to_lowercase()
    } else {
        text.to_string()
    };
    let clean_pattern = pattern.trim_start_matches("^(?s)").trim_end_matches('$');

    // 处理 (?:A|B) 选择分支
    if clean_pattern.starts_with("(?:") && clean_pattern.ends_with(')') {
        let inner = &clean_pattern[3..clean_pattern.len() - 1];
        // 简单分割（不处理嵌套括号）
        for branch in inner.split('|') {
            if wildcard_match(branch, &text, case_insensitive) {
                return true;
            }
        }
        return false;
    }

    wildcard_match(clean_pattern, &text, case_insensitive)
}

/// 简单通配符匹配（* = 匹配任意字符）
fn wildcard_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let pattern = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    // 将 .* 替换回 * 用于简化匹配
    let pattern = pattern.replace(".*", "*");
    // 去除正则转义
    let mut clean = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                clean.push(next);
                chars.next();
            }
        } else {
            clean.push(ch);
        }
    }

    glob_match(&clean, text)
}

/// 标准 glob 匹配算法
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: Option<usize> = None;

    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ti = Some(ti);
            pi += 1;
        } else if pi < p.len() && (p[pi] == t[ti] || p[pi] == '?') {
            pi += 1;
            ti += 1;
        } else if let (Some(sp), Some(st)) = (star_pi, star_ti) {
            pi = sp + 1;
            star_ti = Some(st + 1);
            ti = st + 1;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_exact() {
        assert_eq!(
            parse_shell_rule("git commit"),
            ShellPermissionRule::Exact {
                command: "git commit".into()
            }
        );
    }

    #[test]
    fn test_parse_prefix() {
        assert_eq!(
            parse_shell_rule("npm:*"),
            ShellPermissionRule::Prefix {
                prefix: "npm".into()
            }
        );
    }

    #[test]
    fn test_parse_wildcard() {
        assert_eq!(
            parse_shell_rule("git *"),
            ShellPermissionRule::Wildcard {
                pattern: "git *".into()
            }
        );
    }

    #[test]
    fn test_exact_match() {
        let rule = parse_shell_rule("git commit");
        assert!(matches_rule(&rule, "git commit"));
        assert!(!matches_rule(&rule, "git push"));
    }

    #[test]
    fn test_prefix_match() {
        let rule = parse_shell_rule("npm:*");
        assert!(matches_rule(&rule, "npm install"));
        assert!(matches_rule(&rule, "npm"));
        assert!(!matches_rule(&rule, "npx test"));
    }

    #[test]
    fn test_wildcard_match() {
        let rule = parse_shell_rule("git *");
        assert!(matches_rule(&rule, "git commit"));
        assert!(matches_rule(&rule, "git push --force"));
        assert!(matches_rule(&rule, "git")); // trailing * makes space optional
        assert!(!matches_rule(&rule, "gh pr"));
    }

    #[test]
    fn test_has_wildcards() {
        assert!(has_wildcards("git *"));
        assert!(!has_wildcards("npm:*")); // :* is prefix syntax
        assert!(!has_wildcards("git commit"));
        assert!(!has_wildcards("git \\*")); // escaped
    }
}
