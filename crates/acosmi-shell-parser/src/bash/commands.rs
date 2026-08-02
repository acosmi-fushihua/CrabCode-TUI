//! 命令拆分与重定向提取
//!
//! 等价于 TS commands.ts

use super::quote::{ShellToken, parse_shell_tokens};

/// 提取命令中的输出重定向
#[must_use]
pub fn extract_output_redirections(cmd: &str) -> OutputRedirections {
    let tokens = parse_shell_tokens(cmd);
    let mut redirections = Vec::new();
    let mut has_dangerous = false;
    let mut cmd_without = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        match &tokens[i] {
            ShellToken::Redirect(op) if op == ">" || op == ">>" => {
                let operator = if op == ">>" { ">>" } else { ">" };
                if i + 1 < tokens.len()
                    && let ShellToken::Word(target) = &tokens[i + 1]
                {
                    // 检查危险重定向
                    if is_dangerous_redirect_target(target) {
                        has_dangerous = true;
                    }
                    redirections.push(OutputRedirection {
                        target: target.clone(),
                        operator: operator.to_string(),
                    });
                    i += 2;
                    continue;
                }
                cmd_without.push(tokens[i].clone());
            }
            _ => {
                cmd_without.push(tokens[i].clone());
            }
        }
        i += 1;
    }

    let command_without_redirections = cmd_without
        .iter()
        .map(|t| match t {
            ShellToken::Word(w) => w.clone(),
            ShellToken::Op(o) => o.clone(),
            ShellToken::Redirect(r) => r.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ");

    OutputRedirections {
        command_without_redirections,
        redirections,
        has_dangerous_redirection: has_dangerous,
    }
}

/// 按操作符分割命令
#[must_use]
pub fn split_command_with_operators(cmd: &str) -> Vec<String> {
    let tokens = parse_shell_tokens(cmd);
    let mut segments = Vec::new();
    let mut current = String::new();

    for token in &tokens {
        match token {
            ShellToken::Op(op) if op == "&&" || op == "||" || op == ";" || op == "|" => {
                if !current.is_empty() {
                    segments.push(current.trim().to_string());
                    current = String::new();
                }
            }
            ShellToken::Word(w) => {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(w);
            }
            ShellToken::Op(o) | ShellToken::Redirect(o) => {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(o);
            }
        }
    }

    if !current.is_empty() {
        segments.push(current.trim().to_string());
    }

    segments
}

/// 检查是否为帮助命令
#[must_use]
pub fn is_help_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    trimmed.ends_with("--help") || trimmed.ends_with("-h") || trimmed.starts_with("man ")
}

/// 输出重定向信息
#[derive(Debug, Clone)]
pub struct OutputRedirection {
    pub target: String,
    pub operator: String,
}

/// 输出重定向提取结果
#[derive(Debug, Clone)]
pub struct OutputRedirections {
    pub command_without_redirections: String,
    pub redirections: Vec<OutputRedirection>,
    pub has_dangerous_redirection: bool,
}

/// 检查重定向目标是否危险
fn is_dangerous_redirect_target(target: &str) -> bool {
    let dangerous_paths = [
        "/etc/",
        "/dev/",
        "/proc/",
        "/sys/",
        "/boot/",
        "/var/log/",
        "/tmp/",
    ];
    let target_lower = target.to_lowercase();
    dangerous_paths.iter().any(|p| target_lower.starts_with(p))
        || target == "/dev/null" // 允许
        || target.starts_with('|') // pipe redirect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_command() {
        let segments = split_command_with_operators("cmd1 && cmd2 || cmd3");
        assert_eq!(segments, vec!["cmd1", "cmd2", "cmd3"]);
    }

    #[test]
    fn test_split_pipeline() {
        let segments = split_command_with_operators("ls | grep foo | wc -l");
        assert_eq!(segments, vec!["ls", "grep foo", "wc -l"]);
    }

    #[test]
    fn test_extract_redirections() {
        let result = extract_output_redirections("echo hello > output.txt");
        assert_eq!(result.redirections.len(), 1);
        assert_eq!(result.redirections[0].target, "output.txt");
        assert_eq!(result.redirections[0].operator, ">");
    }

    #[test]
    fn test_is_help() {
        assert!(is_help_command("git --help"));
        assert!(is_help_command("man git"));
        assert!(!is_help_command("git status"));
    }
}
