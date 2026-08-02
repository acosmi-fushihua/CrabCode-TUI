//! 静态前缀提取
//!
//! 等价于 TS powershell/staticPrefix.ts

use super::parser;

/// 静态提取命令前缀
pub async fn get_command_prefix_static(command: &str) -> Option<String> {
    let parsed = parser::parse_powershell_command(command).await.ok()?;

    if !parsed.valid {
        return None;
    }

    let commands = parser::get_all_commands(&parsed);
    if commands.is_empty() {
        return None;
    }

    let first = &commands[0];
    Some(first.name.clone())
}

/// 提取复合命令的所有前缀
pub async fn get_compound_command_prefixes_static(command: &str) -> Vec<String> {
    let parsed = match parser::parse_powershell_command(command).await {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    parser::get_all_command_names(&parsed)
}

/// 词对齐的最长公共前缀（case-insensitive comparison）
#[must_use]
pub fn word_aligned_lcp(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    if strings.len() == 1 {
        return strings[0].clone();
    }

    let first = &strings[0];
    let mut prefix_len = first.len();

    for s in &strings[1..] {
        prefix_len = prefix_len.min(s.len());
        for (i, (a, b)) in first.chars().zip(s.chars()).enumerate() {
            if !a.eq_ignore_ascii_case(&b) {
                prefix_len = prefix_len.min(i);
                break;
            }
        }
    }

    // 对齐到词边界
    let prefix = &first[..prefix_len];
    if let Some(last_space) = prefix.rfind(' ') {
        first[..=last_space].trim_end().to_string()
    } else {
        prefix.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_aligned_lcp() {
        assert_eq!(
            word_aligned_lcp(&["git status".to_string(), "git log".to_string()]),
            "git"
        );
        assert_eq!(word_aligned_lcp(&["single".to_string()]), "single");
        assert_eq!(word_aligned_lcp(&[]), "");
    }
}
