//! Shell 引用工具
//!
//! 等价于 TS shellQuote.ts + shellQuoting.ts

/// 对单个参数进行 shell 安全转义
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }

    // 如果不含特殊字符，直接返回
    if !needs_quoting(arg) {
        return arg.to_string();
    }

    // 使用单引号包裹（最安全）
    let mut result = String::with_capacity(arg.len() + 4);
    result.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            // 单引号中不能转义单引号，需要: '\''
            result.push_str("'\\''");
        } else {
            result.push(c);
        }
    }
    result.push('\'');
    result
}

/// 检查参数是否需要引用
#[must_use]
pub fn needs_quoting(arg: &str) -> bool {
    if arg.is_empty() {
        return true;
    }
    arg.chars().any(|c| {
        matches!(
            c,
            ' ' | '\t'
                | '\n'
                | '"'
                | '\''
                | '\\'
                | '`'
                | '$'
                | '!'
                | '#'
                | '&'
                | '|'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '*'
                | '?'
                | '~'
        )
    })
}

/// 对多个参数进行 shell 安全转义并用空格连接
#[must_use]
pub fn shell_quote_args(args: &[&str]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 解析 shell 引用的字符串，返回 token 列表
///
/// 简化版 shell-quote parse：处理基本的引号和转义
#[must_use]
pub fn parse_shell_tokens(cmd: &str) -> Vec<ShellToken> {
    let mut tokens = Vec::new();
    let mut chars = cmd.chars().peekable();
    let mut current = String::new();

    while let Some(c) = chars.next() {
        match c {
            // 空白分隔 token
            ' ' | '\t' => {
                if !current.is_empty() {
                    tokens.push(ShellToken::Word(std::mem::take(&mut current)));
                }
            }

            // 单引号
            '\'' => loop {
                match chars.next() {
                    Some('\'') | None => break,
                    Some(c) => current.push(c),
                }
            },

            // 双引号
            '"' => loop {
                match chars.next() {
                    Some('"') | None => break,
                    Some('\\') => {
                        if let Some(&next) = chars.peek() {
                            if matches!(next, '"' | '\\' | '$' | '`') {
                                current.push(next);
                                chars.next();
                            } else {
                                current.push('\\');
                            }
                        }
                    }
                    Some(c) => current.push(c),
                }
            },

            // 反斜杠转义
            '\\' => {
                if let Some(next) = chars.next()
                    && next != '\n'
                {
                    current.push(next);
                }
            }

            // 操作符
            '|' => {
                if !current.is_empty() {
                    tokens.push(ShellToken::Word(std::mem::take(&mut current)));
                }
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(ShellToken::Op("||".to_string()));
                } else {
                    tokens.push(ShellToken::Op("|".to_string()));
                }
            }

            '&' => {
                if !current.is_empty() {
                    tokens.push(ShellToken::Word(std::mem::take(&mut current)));
                }
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(ShellToken::Op("&&".to_string()));
                } else {
                    tokens.push(ShellToken::Op("&".to_string()));
                }
            }

            ';' => {
                if !current.is_empty() {
                    tokens.push(ShellToken::Word(std::mem::take(&mut current)));
                }
                tokens.push(ShellToken::Op(";".to_string()));
            }

            '>' | '<' => {
                if !current.is_empty() {
                    tokens.push(ShellToken::Word(std::mem::take(&mut current)));
                }
                let mut op = c.to_string();
                if chars.peek() == Some(&c)
                    || chars.peek() == Some(&'&')
                    || chars.peek() == Some(&'|')
                {
                    op.push(chars.next().unwrap_or_default());
                }
                tokens.push(ShellToken::Redirect(op));
            }

            // 普通字符
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        tokens.push(ShellToken::Word(current));
    }

    tokens
}

/// Shell token
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellToken {
    Word(String),
    Op(String),
    Redirect(String),
}

/// 检查命令是否有 stdin 重定向
#[must_use]
pub fn has_stdin_redirect(cmd: &str) -> bool {
    let tokens = parse_shell_tokens(cmd);
    tokens
        .iter()
        .any(|t| matches!(t, ShellToken::Redirect(r) if r == "<" || r == "<<<" || r == "<<"))
}

/// 检查是否应该添加 stdin 重定向
#[must_use]
pub fn should_add_stdin_redirect(cmd: &str) -> bool {
    !has_stdin_redirect(cmd)
}

/// Windows null 重定向转换
///
/// 将 `2>nul` 转为 `2>/dev/null`（用于 Git Bash 环境）
#[must_use]
pub fn rewrite_windows_null_redirect(cmd: &str) -> String {
    cmd.replace("2>nul", "2>/dev/null")
        .replace("2>NUL", "2>/dev/null")
        .replace(">nul", ">/dev/null")
        .replace(">NUL", ">/dev/null")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_shell_quote_special_chars() {
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("a&b"), "'a&b'");
        assert_eq!(shell_quote("test;cmd"), "'test;cmd'");
    }

    #[test]
    fn test_shell_quote_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_parse_shell_tokens() {
        let tokens = parse_shell_tokens("echo hello world");
        assert_eq!(
            tokens,
            vec![
                ShellToken::Word("echo".to_string()),
                ShellToken::Word("hello".to_string()),
                ShellToken::Word("world".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_shell_tokens_quoted() {
        let tokens = parse_shell_tokens("echo 'hello world'");
        assert_eq!(
            tokens,
            vec![
                ShellToken::Word("echo".to_string()),
                ShellToken::Word("hello world".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_shell_tokens_pipe() {
        let tokens = parse_shell_tokens("ls | grep foo");
        assert_eq!(
            tokens,
            vec![
                ShellToken::Word("ls".to_string()),
                ShellToken::Op("|".to_string()),
                ShellToken::Word("grep".to_string()),
                ShellToken::Word("foo".to_string()),
            ]
        );
    }

    #[test]
    fn test_has_stdin_redirect() {
        assert!(has_stdin_redirect("cmd < file"));
        assert!(!has_stdin_redirect("cmd > file"));
        assert!(has_stdin_redirect("cmd <<< 'string'"));
    }

    #[test]
    fn test_rewrite_windows_null() {
        assert_eq!(
            rewrite_windows_null_redirect("git status 2>nul"),
            "git status 2>/dev/null"
        );
    }
}
