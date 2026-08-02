//! Heredoc 解析与提取
//!
//! 等价于 TS heredoc.ts

use crate::{HeredocExtractionResult, HeredocInfo};
use std::collections::HashMap;

/// 检查命令是否包含 heredoc
#[must_use]
pub fn contains_heredoc(cmd: &str) -> bool {
    cmd.contains("<<")
}

/// 提取 heredoc 并替换为占位符
///
/// - `quoted_only`: 为 true 时只提取引号分隔符的 heredoc
pub fn extract_heredocs(cmd: &str, quoted_only: bool) -> HeredocExtractionResult {
    if !contains_heredoc(cmd) {
        return HeredocExtractionResult {
            command: cmd.to_string(),
            heredocs: HashMap::new(),
        };
    }

    let mut result_cmd = String::new();
    let mut heredocs = HashMap::new();
    let bytes = cmd.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // 跳过引号内容
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            result_cmd.push_str(&cmd[start..i]);
            continue;
        }

        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            result_cmd.push_str(&cmd[start..i]);
            continue;
        }

        // 检测 <<
        if i + 1 < bytes.len() && bytes[i] == b'<' && bytes[i + 1] == b'<' {
            // 排除 <<<（herestring）
            if i + 2 < bytes.len() && bytes[i + 2] == b'<' {
                result_cmd.push_str("<<<");
                i += 3;
                continue;
            }

            let op_start = i;
            i += 2;

            // 可选的 -
            let _strip_tabs = i < bytes.len() && bytes[i] == b'-';
            if _strip_tabs {
                i += 1;
            }

            // 跳过空白
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }

            let op_end = i;

            // 解析分隔符
            let (delim, delim_quoted, delim_end) = parse_heredoc_delimiter(cmd, i);

            if delim.is_empty() {
                result_cmd.push_str(&cmd[op_start..delim_end]);
                i = delim_end;
                continue;
            }

            if quoted_only && !delim_quoted {
                result_cmd.push_str(&cmd[op_start..delim_end]);
                i = delim_end;
                continue;
            }

            // 查找 heredoc 正文
            i = delim_end;
            // 找到下一个换行
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            } // skip \n

            let content_start = i;

            // 查找关闭分隔符（独占一行）
            let content_end;
            let end_end;

            loop {
                if i >= bytes.len() {
                    content_end = i;
                    end_end = i;
                    break;
                }

                let line_start = i;
                // 如果 strip_tabs，跳过前导 tab
                if _strip_tabs {
                    while i < bytes.len() && bytes[i] == b'\t' {
                        i += 1;
                    }
                }

                // 检查是否匹配分隔符
                let delim_bytes = delim.as_bytes();
                let mut matched = true;
                for (j, &db) in delim_bytes.iter().enumerate() {
                    if i + j >= bytes.len() || bytes[i + j] != db {
                        matched = false;
                        break;
                    }
                }

                if matched {
                    let after = i + delim_bytes.len();
                    if after >= bytes.len() || bytes[after] == b'\n' {
                        content_end = line_start;
                        end_end = after;
                        i = if after < bytes.len() {
                            after + 1
                        } else {
                            after
                        };
                        break;
                    }
                }

                // 未匹配，跳到行尾
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }

            // 生成占位符
            let placeholder = format!("__HEREDOC_{}__", heredocs.len());

            let full_text = cmd[op_start..end_end.min(cmd.len())].to_string();
            heredocs.insert(
                placeholder.clone(),
                HeredocInfo {
                    full_text,
                    delimiter: delim,
                    operator_start_index: op_start,
                    operator_end_index: op_end,
                    content_start_index: content_start,
                    content_end_index: content_end,
                },
            );

            result_cmd.push_str(&placeholder);
            continue;
        }

        // 普通字符
        result_cmd.push(cmd[i..].chars().next().unwrap_or_default());
        i += cmd[i..].chars().next().map_or(1, char::len_utf8);
    }

    HeredocExtractionResult {
        command: result_cmd,
        heredocs,
    }
}

/// 还原 heredoc 占位符
#[must_use]
pub fn restore_heredocs(parts: &[String], heredocs: &HashMap<String, HeredocInfo>) -> Vec<String> {
    parts
        .iter()
        .map(|part| {
            let mut result = part.clone();
            for (placeholder, info) in heredocs {
                result = result.replace(placeholder, &info.full_text);
            }
            result
        })
        .collect()
}

/// 解析 heredoc 分隔符
///
/// 返回 (分隔符文本, 是否被引号, 结束位置)
fn parse_heredoc_delimiter(cmd: &str, start: usize) -> (String, bool, usize) {
    let bytes = cmd.as_bytes();
    let mut i = start;
    let mut delim = String::new();
    let mut quoted = false;

    if i >= bytes.len() {
        return (delim, quoted, i);
    }

    match bytes[i] {
        b'\'' => {
            quoted = true;
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                delim.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        }
        b'"' => {
            quoted = true;
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                delim.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        }
        _ => {
            while i < bytes.len() {
                match bytes[i] {
                    b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b')' => break,
                    b'\\' => {
                        quoted = true;
                        i += 1;
                        if i < bytes.len() {
                            delim.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    c => {
                        delim.push(c as char);
                        i += 1;
                    }
                }
            }
        }
    }

    (delim, quoted, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_heredoc() {
        assert!(contains_heredoc("cat <<EOF\nhello\nEOF"));
        assert!(!contains_heredoc("echo hello"));
        assert!(contains_heredoc("cat << 'END'\ndata\nEND"));
    }

    #[test]
    fn test_extract_simple_heredoc() {
        let result = extract_heredocs("cat <<EOF\nhello world\nEOF", false);
        assert!(!result.heredocs.is_empty());
        assert!(!result.command.contains("hello world"));
    }

    #[test]
    fn test_no_heredoc() {
        let result = extract_heredocs("echo hello", false);
        assert!(result.heredocs.is_empty());
        assert_eq!(result.command, "echo hello");
    }

    #[test]
    fn test_herestring_not_heredoc() {
        let result = extract_heredocs("cat <<< 'hello'", false);
        assert!(result.heredocs.is_empty());
    }
}
