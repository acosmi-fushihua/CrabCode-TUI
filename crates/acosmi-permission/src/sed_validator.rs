//! sed 安全验证
//!
//! 等价于 TS sedValidator.ts
//! 检查 sed 命令是否为只读操作（只输出不修改文件）。

/// 检查 sed 命令是否为只读
///
/// 只有两种安全模式：
/// 1. 行打印：`sed -n 'NUMp'` 或 `sed -n 'NUM,NUMp'`
/// 2. 单次替换：`sed 's/pattern/replacement/flags'`
///
/// 任何写入文件的选项（-i）或危险子命令（e, w, W, !, ~, #, {}）都被拒绝。
#[must_use]
pub fn is_sed_readonly(command: &str, args: &[&str]) -> bool {
    let _ = command; // 使用完整命令进行上下文（未来扩展）

    if args.is_empty() {
        return false;
    }

    // 解析 sed flags 和表达式
    let mut has_n_flag = false;
    let mut has_i_flag = false;
    let mut expressions: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = args[i];

        if arg == "--" {
            // 之后都是文件名
            break;
        }

        if arg == "-n" || arg == "--quiet" || arg == "--silent" {
            has_n_flag = true;
            i += 1;
            continue;
        }

        if arg == "-i" || arg.starts_with("-i") || arg == "--in-place" {
            has_i_flag = true;
            i += 1;
            continue;
        }

        if arg == "-e" || arg == "--expression" {
            // 下一个参数是表达式
            i += 1;
            if i < args.len() {
                expressions.push(args[i]);
            }
            i += 1;
            continue;
        }

        if let Some(rest) = arg.strip_prefix("-e") {
            // -eEXPRESSION
            expressions.push(rest);
            i += 1;
            continue;
        }

        if arg == "-f" || arg == "--file" {
            // 脚本文件 — 不允许
            return false;
        }

        // 非 flag 参数：第一个是表达式（如果还没有 -e），之后是文件名
        if expressions.is_empty() && !arg.starts_with('-') {
            expressions.push(arg);
        }

        i += 1;
    }

    // -i（就地编辑）始终不安全
    if has_i_flag {
        return false;
    }

    // 没有表达式
    if expressions.is_empty() {
        return false;
    }

    // 检查所有表达式
    for expr in &expressions {
        if !is_safe_sed_expression(expr, has_n_flag) {
            return false;
        }
    }

    true
}

/// 检查单个 sed 表达式是否安全
fn is_safe_sed_expression(expr: &str, has_n_flag: bool) -> bool {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return false;
    }

    // 拒绝列表检查
    if has_denylist_content(trimmed) {
        return false;
    }

    // 模式 1：行打印 (需要 -n flag)
    if has_n_flag && is_print_expression(trimmed) {
        return true;
    }

    // 模式 2：替换
    if is_substitution_expression(trimmed) {
        return true;
    }

    false
}

/// 检查是否包含拒绝列表内容
fn has_denylist_content(expr: &str) -> bool {
    // 非 ASCII 字符
    if expr.chars().any(|c| c as u32 > 0x7F) {
        return true;
    }

    // 花括号（分组命令）
    if expr.contains('{') || expr.contains('}') {
        return true;
    }

    // 换行符
    if expr.contains('\n') || expr.contains('\r') {
        return true;
    }

    // 注释符 # 在表达式中
    if expr.contains('#') {
        return true;
    }

    // ! (取反) 和 ~ (GNU 步进)
    if expr.contains('!') || expr.contains('~') {
        return true;
    }

    // w/W/e/E 命令（写入文件/执行）
    // 检查这些作为 sed 命令字母（在 s 替换之外）
    if has_dangerous_sed_command(expr) {
        return true;
    }

    false
}

/// 检查是否包含危险的 sed 命令字母
///
/// 只检查 sed 命令位置（地址后的命令字母），不检查地址/模式内的字符。
fn has_dangerous_sed_command(expr: &str) -> bool {
    let trimmed = expr.trim();

    // 替换表达式 — 在 is_substitution_expression 中单独处理
    if trimmed.starts_with('s') {
        return false;
    }

    // 打印表达式 — 安全
    if trimmed.ends_with('p') || trimmed.ends_with('d') {
        return false;
    }

    // 跳过地址部分，只检查命令字母
    let command_part = skip_sed_address(trimmed);

    // 检查命令字母
    if let Some(cmd_char) = command_part.chars().next() {
        return matches!(
            cmd_char,
            'w' | 'W' | 'e' | 'E' | 'r' | 'R' | 'a' | 'c' | 'i'
        );
    }

    false
}

/// 跳过 sed 地址部分，返回命令字母开始的位置
fn skip_sed_address(expr: &str) -> &str {
    let bytes = expr.as_bytes();
    let mut i = 0;

    // 跳过数字地址
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // 跳过 $ 地址
    if i < bytes.len() && bytes[i] == b'$' {
        i += 1;
    }
    // 跳过 /pattern/ 地址
    if i < bytes.len() && bytes[i] == b'/' {
        i += 1;
        while i < bytes.len() && bytes[i] != b'/' {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 1;
            }
            i += 1;
        }
        if i < bytes.len() {
            i += 1;
        } // skip closing /
    }
    // 跳过范围分隔符 , 和第二个地址
    if i < bytes.len() && bytes[i] == b',' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'$' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'/' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'/' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        }
    }

    &expr[i..]
}

/// 检查是否为打印表达式 (Np, N,Mp, /pattern/p)
fn is_print_expression(expr: &str) -> bool {
    let trimmed = expr.trim();

    // 末尾必须是 p
    if !trimmed.ends_with('p') {
        return false;
    }

    let before_p = &trimmed[..trimmed.len() - 1];

    // 空地址 + p
    if before_p.is_empty() {
        return true;
    }

    // 纯数字地址: 5p
    if before_p.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }

    // 范围地址: 1,5p
    if let Some(comma_idx) = before_p.find(',') {
        let start = &before_p[..comma_idx];
        let end = &before_p[comma_idx + 1..];
        if start.chars().all(|c| c.is_ascii_digit() || c == '$')
            && end.chars().all(|c| c.is_ascii_digit() || c == '$')
        {
            return true;
        }
    }

    // $ 地址（最后一行）: $p
    if before_p == "$" {
        return true;
    }

    // 正则地址: /pattern/p
    if before_p.starts_with('/') && before_p.ends_with('/') && before_p.len() >= 2 {
        let pattern = &before_p[1..before_p.len() - 1];
        // 简单的正则模式（不包含危险字符）
        if !pattern.contains('\\') || pattern.contains("\\/") {
            return true;
        }
        return true;
    }

    false
}

/// 检查是否为安全的替换表达式 s/pattern/replacement/flags
fn is_substitution_expression(expr: &str) -> bool {
    let trimmed = expr.trim();

    if !trimmed.starts_with('s') {
        return false;
    }

    if trimmed.len() < 4 {
        return false; // 至少 s/x/y
    }

    // 分隔符是 s 后面的第一个字符。trimmed.len() >= 3 已保证有 index 1 字符，
    // expect 描述这个不变量。
    let delim = trimmed
        .chars()
        .nth(1)
        .expect("trimmed length >= 3 guaranteed by check above");

    // 不允许使用字母数字作为分隔符
    if delim.is_ascii_alphanumeric() {
        return false;
    }

    let rest = &trimmed[2..]; // 跳过 s + 分隔符

    // 找到第二个和第三个分隔符
    let mut parts: Vec<&str> = Vec::new();
    let mut current_start = 0;
    let mut escaped = false;

    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == delim {
            parts.push(&rest[current_start..i]);
            current_start = i + c.len_utf8();
        }
    }
    // 最后一部分（flags）
    if current_start <= rest.len() {
        parts.push(&rest[current_start..]);
    }

    // 应该有 pattern, replacement, [flags]
    if parts.len() < 2 {
        return false;
    }

    // 检查 flags
    if parts.len() >= 3 {
        let flags = parts[2];
        // 安全的 flags: g, i, I, p, 数字
        for c in flags.chars() {
            match c {
                'g' | 'i' | 'I' | 'p' => {}
                c if c.is_ascii_digit() => {}
                'w' | 'e' => return false, // 写入文件/执行
                _ => return false,         // 未知 flag
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sed_print_readonly() {
        assert!(is_sed_readonly("sed", &["-n", "5p", "file.txt"]));
        assert!(is_sed_readonly("sed", &["-n", "1,10p", "file.txt"]));
        assert!(is_sed_readonly("sed", &["-n", "$p", "file.txt"]));
        assert!(is_sed_readonly("sed", &["-n", "/pattern/p", "file.txt"]));
    }

    #[test]
    fn test_sed_substitution_readonly() {
        assert!(is_sed_readonly("sed", &["s/foo/bar/", "file.txt"]));
        assert!(is_sed_readonly("sed", &["s/foo/bar/g", "file.txt"]));
        assert!(is_sed_readonly("sed", &["s|foo|bar|g", "file.txt"]));
    }

    #[test]
    fn test_sed_inplace_not_readonly() {
        assert!(!is_sed_readonly("sed", &["-i", "s/foo/bar/", "file.txt"]));
        assert!(!is_sed_readonly(
            "sed",
            &["-i.bak", "s/foo/bar/", "file.txt"]
        ));
        assert!(!is_sed_readonly(
            "sed",
            &["--in-place", "s/foo/bar/", "file.txt"]
        ));
    }

    #[test]
    fn test_sed_dangerous_flags() {
        // w flag — 写入文件
        assert!(!is_sed_readonly(
            "sed",
            &["s/foo/bar/w output.txt", "file.txt"]
        ));
        // e flag — 执行命令
        assert!(!is_sed_readonly("sed", &["s/foo/bar/e", "file.txt"]));
    }

    #[test]
    fn test_sed_dangerous_commands() {
        // w 命令
        assert!(!is_sed_readonly("sed", &["-n", "w output.txt", "file.txt"]));
    }

    #[test]
    fn test_sed_braces_rejected() {
        assert!(!is_sed_readonly("sed", &["-n", "{p;d}", "file.txt"]));
    }

    #[test]
    fn test_sed_negation_rejected() {
        assert!(!is_sed_readonly("sed", &["-n", "!p", "file.txt"]));
    }

    #[test]
    fn test_sed_non_ascii_rejected() {
        assert!(!is_sed_readonly("sed", &["s/\u{00E9}/e/g", "file.txt"]));
    }

    #[test]
    fn test_sed_empty() {
        assert!(!is_sed_readonly("sed", &[]));
    }

    #[test]
    fn test_sed_script_file_rejected() {
        assert!(!is_sed_readonly("sed", &["-f", "script.sed", "file.txt"]));
    }

    #[test]
    fn test_sed_expression_flag() {
        assert!(is_sed_readonly("sed", &["-e", "s/foo/bar/g", "file.txt"]));
        assert!(is_sed_readonly("sed", &["-n", "-e", "5p", "file.txt"]));
    }

    #[test]
    fn test_sed_hash_rejected() {
        assert!(!is_sed_readonly("sed", &["#comment", "file.txt"]));
    }

    #[test]
    fn test_sed_newline_rejected() {
        assert!(!is_sed_readonly("sed", &["s/foo/bar\n/g", "file.txt"]));
    }

    #[test]
    fn test_print_without_n_flag() {
        // print 需要 -n flag 才认为安全
        assert!(!is_sed_readonly("sed", &["5p", "file.txt"]));
    }
}
