//! 只读命令验证
//!
//! 等价于 TS readOnlyValidation.ts (1,990 LOC) + readOnlyCommandValidation.ts
//! 判断命令是否只读（无副作用），从而可以自动批准。
//!
//! 核心数据结构: `COMMAND_ALLOWLIST` — 50+ 命令及其安全 flag 配置

use std::collections::HashMap;

use crate::constants;

/// Flag 参数类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagArgType {
    /// 无参数（纯开关）
    None,
    /// 数字参数
    Number,
    /// 字符串参数
    StringArg,
    /// 单字符参数
    Char,
    /// 字面量 "{}" 参数
    Braces,
    /// 字面量 "EOF" 参数
    Eof,
}

/// 命令配置
pub struct CommandConfig {
    pub safe_flags: HashMap<&'static str, FlagArgType>,
    pub respects_double_dash: bool,
    pub additional_check: Option<fn(&str, &[&str]) -> bool>,
}

// ─── Public API ───

/// 判断命令是否为只读命令
#[must_use]
pub fn is_command_readonly(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }

    if contains_unquoted_expansion(trimmed) {
        return false;
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }

    let cmd_name = parts[0];
    let args = &parts[1..];

    // 简单只读命令集合
    let readonly_cmds = constants::readonly_commands();
    if readonly_cmds.contains(cmd_name) {
        return validate_readonly_args(cmd_name, args);
    }

    // COMMAND_ALLOWLIST
    if let Some(result) = check_command_allowlist(trimmed, cmd_name, args) {
        return result;
    }

    // 特殊 find
    if cmd_name == "find" {
        return !has_dangerous_find_flags(trimmed);
    }

    if is_simple_readonly_pattern(trimmed) {
        return true;
    }

    if is_version_check(trimmed) {
        return true;
    }

    false
}

fn check_command_allowlist(full_cmd: &str, cmd_name: &str, args: &[&str]) -> Option<bool> {
    let allowlist = command_allowlist();

    if let Some(config) = allowlist.get(cmd_name) {
        if let Some(check) = config.additional_check
            && check(full_cmd, args)
        {
            return Some(false);
        }
        return Some(validate_flags(
            args,
            &config.safe_flags,
            config.respects_double_dash,
        ));
    }

    let parts: Vec<&str> = full_cmd.split_whitespace().collect();
    if parts.len() >= 2 {
        let two_word = format!("{} {}", parts[0], parts[1]);
        if let Some(config) = allowlist.get(two_word.as_str()) {
            let remaining_args: Vec<&str> = parts[2..].to_vec();
            if let Some(check) = config.additional_check
                && check(full_cmd, &remaining_args)
            {
                return Some(false);
            }
            return Some(validate_flags(
                &remaining_args,
                &config.safe_flags,
                config.respects_double_dash,
            ));
        }
    }

    if parts.len() >= 3 {
        let three_word = format!("{} {} {}", parts[0], parts[1], parts[2]);
        if let Some(config) = allowlist.get(three_word.as_str()) {
            let remaining_args: Vec<&str> = parts[3..].to_vec();
            if let Some(check) = config.additional_check
                && check(full_cmd, &remaining_args)
            {
                return Some(false);
            }
            return Some(validate_flags(
                &remaining_args,
                &config.safe_flags,
                config.respects_double_dash,
            ));
        }
    }

    None
}

/// 验证 flags 是否在安全列表中
#[must_use]
pub fn validate_flags(
    args: &[&str],
    safe_flags: &HashMap<&str, FlagArgType>,
    respects_double_dash: bool,
) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];

        if arg == "--" {
            if respects_double_dash {
                break; // -- 后全是操作数
            }
            // 即使不尊重 --，也跳过 -- token 本身（避免被当作未知 flag 拒绝）
            i += 1;
            continue;
        }

        if arg.starts_with('-') && arg != "-" {
            if arg.starts_with("--") {
                let (flag_name, has_equals, inline_value) = if let Some(eq_idx) = arg.find('=') {
                    (&arg[..eq_idx], true, Some(&arg[eq_idx + 1..]))
                } else {
                    (arg, false, None)
                };

                match safe_flags.get(flag_name) {
                    Some(&FlagArgType::None) => {
                        if has_equals {
                            return false;
                        }
                    }
                    Some(arg_type) => {
                        if has_equals {
                            if let Some(val) = inline_value
                                && !validate_flag_arg(val, *arg_type)
                            {
                                return false;
                            }
                        } else {
                            i += 1;
                            if i >= args.len() {
                                return false;
                            }
                            if !validate_flag_arg(args[i], *arg_type) {
                                return false;
                            }
                        }
                    }
                    None => return false,
                }
            } else {
                let flag_chars: Vec<char> = arg[1..].chars().collect();
                let mut j = 0;
                while j < flag_chars.len() {
                    let flag_key = format!("-{}", flag_chars[j]);
                    match safe_flags.get(flag_key.as_str()) {
                        Some(&FlagArgType::None) => {
                            j += 1;
                        }
                        Some(arg_type) => {
                            if j + 1 < flag_chars.len() {
                                let flag_arg: String = flag_chars[j + 1..].iter().collect();
                                if !validate_flag_arg(&flag_arg, *arg_type) {
                                    return false;
                                }
                                break;
                            }
                            i += 1;
                            if i >= args.len() {
                                return false;
                            }
                            if !validate_flag_arg(args[i], *arg_type) {
                                return false;
                            }
                            break;
                        }
                        None => {
                            if flag_chars[j].is_ascii_digit()
                                && flag_chars[j..].iter().all(char::is_ascii_digit)
                            {
                                break;
                            }
                            return false;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    true
}

fn validate_flag_arg(arg: &str, arg_type: FlagArgType) -> bool {
    match arg_type {
        FlagArgType::None => true,
        FlagArgType::Number => arg.chars().all(|c| c.is_ascii_digit()),
        FlagArgType::StringArg => !arg.is_empty(),
        FlagArgType::Char => arg.len() == 1,
        FlagArgType::Braces => arg == "{}",
        FlagArgType::Eof => arg == "EOF",
    }
}

#[must_use]
pub fn contains_unquoted_expansion(command: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for c in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if !in_single_quote => {
                escaped = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            '$' if !in_single_quote => return true,
            '`' if !in_single_quote => return true,
            _ => {}
        }
    }
    false
}

#[must_use]
pub fn filter_out_flags<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut result = Vec::new();
    let mut past_double_dash = false;
    for &arg in args {
        if arg == "--" {
            past_double_dash = true;
            continue;
        }
        if past_double_dash || !arg.starts_with('-') || arg == "-" {
            result.push(arg);
        }
    }
    result
}

#[must_use]
pub fn validate_command_flags(
    _cmd: &str,
    args: &[&str],
    safe_flags: &HashMap<&str, FlagArgType>,
) -> bool {
    validate_flags(args, safe_flags, true)
}

// ─── COMMAND_ALLOWLIST ───

fn command_allowlist() -> HashMap<&'static str, CommandConfig> {
    let mut m: HashMap<&'static str, CommandConfig> = HashMap::new();

    m.insert(
        "xargs",
        CommandConfig {
            safe_flags: flags(&[
                ("-I", FlagArgType::Braces),
                ("-n", FlagArgType::Number),
                ("-P", FlagArgType::Number),
                ("-L", FlagArgType::Number),
                ("-s", FlagArgType::Number),
                ("-E", FlagArgType::Eof),
                ("-0", FlagArgType::None),
                ("-t", FlagArgType::None),
                ("-r", FlagArgType::None),
                ("-x", FlagArgType::None),
                ("-d", FlagArgType::Char),
                ("--null", FlagArgType::None),
                ("--no-run-if-empty", FlagArgType::None),
                ("--max-args", FlagArgType::Number),
                ("--max-procs", FlagArgType::Number),
                ("--verbose", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "grep",
        CommandConfig {
            safe_flags: flags(&[
                ("-e", FlagArgType::StringArg),
                ("-f", FlagArgType::StringArg),
                ("-i", FlagArgType::None),
                ("-v", FlagArgType::None),
                ("-w", FlagArgType::None),
                ("-x", FlagArgType::None),
                ("-c", FlagArgType::None),
                ("-l", FlagArgType::None),
                ("-L", FlagArgType::None),
                ("-n", FlagArgType::None),
                ("-o", FlagArgType::None),
                ("-q", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-r", FlagArgType::None),
                ("-R", FlagArgType::None),
                ("-h", FlagArgType::None),
                ("-H", FlagArgType::None),
                ("-m", FlagArgType::Number),
                ("-A", FlagArgType::Number),
                ("-B", FlagArgType::Number),
                ("-C", FlagArgType::Number),
                ("-P", FlagArgType::None),
                ("-E", FlagArgType::None),
                ("-F", FlagArgType::None),
                ("-G", FlagArgType::None),
                ("-Z", FlagArgType::None),
                ("-a", FlagArgType::None),
                ("-b", FlagArgType::None),
                ("-T", FlagArgType::None),
                ("-u", FlagArgType::None),
                ("--extended-regexp", FlagArgType::None),
                ("--fixed-strings", FlagArgType::None),
                ("--ignore-case", FlagArgType::None),
                ("--invert-match", FlagArgType::None),
                ("--word-regexp", FlagArgType::None),
                ("--line-regexp", FlagArgType::None),
                ("--count", FlagArgType::None),
                ("--files-with-matches", FlagArgType::None),
                ("--files-without-match", FlagArgType::None),
                ("--max-count", FlagArgType::Number),
                ("--only-matching", FlagArgType::None),
                ("--quiet", FlagArgType::None),
                ("--silent", FlagArgType::None),
                ("--with-filename", FlagArgType::None),
                ("--no-filename", FlagArgType::None),
                ("--label", FlagArgType::StringArg),
                ("--line-number", FlagArgType::None),
                ("--byte-offset", FlagArgType::None),
                ("--null", FlagArgType::None),
                ("--after-context", FlagArgType::Number),
                ("--before-context", FlagArgType::Number),
                ("--context", FlagArgType::Number),
                ("--color", FlagArgType::StringArg),
                ("--colour", FlagArgType::StringArg),
                ("--text", FlagArgType::None),
                ("--recursive", FlagArgType::None),
                ("--include", FlagArgType::StringArg),
                ("--exclude", FlagArgType::StringArg),
                ("--exclude-dir", FlagArgType::StringArg),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "rg",
        CommandConfig {
            safe_flags: flags(&[
                ("-e", FlagArgType::StringArg),
                ("-f", FlagArgType::StringArg),
                ("-i", FlagArgType::None),
                ("-v", FlagArgType::None),
                ("-w", FlagArgType::None),
                ("-c", FlagArgType::None),
                ("-l", FlagArgType::None),
                ("-n", FlagArgType::None),
                ("-N", FlagArgType::None),
                ("-o", FlagArgType::None),
                ("-q", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-S", FlagArgType::None),
                ("-m", FlagArgType::Number),
                ("-p", FlagArgType::None),
                ("-A", FlagArgType::Number),
                ("-B", FlagArgType::Number),
                ("-C", FlagArgType::Number),
                ("-F", FlagArgType::None),
                ("-P", FlagArgType::None),
                ("-L", FlagArgType::None),
                ("-M", FlagArgType::Number),
                ("-u", FlagArgType::None),
                ("-z", FlagArgType::None),
                ("-0", FlagArgType::None),
                ("-t", FlagArgType::StringArg),
                ("-T", FlagArgType::StringArg),
                ("-g", FlagArgType::StringArg),
                ("-j", FlagArgType::Number),
                ("-r", FlagArgType::StringArg),
                ("--regexp", FlagArgType::StringArg),
                ("--type", FlagArgType::StringArg),
                ("--type-not", FlagArgType::StringArg),
                ("--glob", FlagArgType::StringArg),
                ("--max-count", FlagArgType::Number),
                ("--max-depth", FlagArgType::Number),
                ("--threads", FlagArgType::Number),
                ("--replace", FlagArgType::StringArg),
                ("--color", FlagArgType::StringArg),
                ("--encoding", FlagArgType::StringArg),
                ("--ignore-case", FlagArgType::None),
                ("--smart-case", FlagArgType::None),
                ("--fixed-strings", FlagArgType::None),
                ("--count", FlagArgType::None),
                ("--files-with-matches", FlagArgType::None),
                ("--line-number", FlagArgType::None),
                ("--no-line-number", FlagArgType::None),
                ("--only-matching", FlagArgType::None),
                ("--hidden", FlagArgType::None),
                ("--no-ignore", FlagArgType::None),
                ("--follow", FlagArgType::None),
                ("--json", FlagArgType::None),
                ("--vimgrep", FlagArgType::None),
                ("--trim", FlagArgType::None),
                ("--heading", FlagArgType::None),
                ("--no-heading", FlagArgType::None),
                ("--sort", FlagArgType::StringArg),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "sort",
        CommandConfig {
            safe_flags: flags(&[
                ("-b", FlagArgType::None),
                ("-d", FlagArgType::None),
                ("-f", FlagArgType::None),
                ("-g", FlagArgType::None),
                ("-i", FlagArgType::None),
                ("-M", FlagArgType::None),
                ("-h", FlagArgType::None),
                ("-n", FlagArgType::None),
                ("-R", FlagArgType::None),
                ("-r", FlagArgType::None),
                ("-V", FlagArgType::None),
                ("-c", FlagArgType::None),
                ("-C", FlagArgType::None),
                ("-m", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-u", FlagArgType::None),
                ("-z", FlagArgType::None),
                ("-k", FlagArgType::StringArg),
                ("-t", FlagArgType::Char),
                ("-S", FlagArgType::StringArg),
                ("--parallel", FlagArgType::Number),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "file",
        CommandConfig {
            safe_flags: flags(&[
                ("-b", FlagArgType::None),
                ("-i", FlagArgType::None),
                ("-L", FlagArgType::None),
                ("-z", FlagArgType::None),
                ("-0", FlagArgType::None),
                ("-p", FlagArgType::None),
                ("--mime-type", FlagArgType::None),
                ("--brief", FlagArgType::None),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    let checksum_flags = flags(&[
        ("-b", FlagArgType::None),
        ("-t", FlagArgType::None),
        ("-c", FlagArgType::None),
        ("--check", FlagArgType::None),
        ("--strict", FlagArgType::None),
        ("--tag", FlagArgType::None),
        ("-z", FlagArgType::None),
        ("--help", FlagArgType::None),
        ("--version", FlagArgType::None),
    ]);
    for cmd in &["sha256sum", "sha1sum", "md5sum"] {
        m.insert(
            *cmd,
            CommandConfig {
                safe_flags: checksum_flags.clone(),
                respects_double_dash: true,
                additional_check: None,
            },
        );
    }

    m.insert(
        "ps",
        CommandConfig {
            safe_flags: flags(&[
                ("-e", FlagArgType::None),
                ("-a", FlagArgType::None),
                ("-f", FlagArgType::None),
                ("-l", FlagArgType::None),
                ("-x", FlagArgType::None),
                ("-u", FlagArgType::StringArg),
                ("-p", FlagArgType::StringArg),
                ("-o", FlagArgType::StringArg),
                ("-w", FlagArgType::None),
                ("-H", FlagArgType::None),
                ("--forest", FlagArgType::None),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            // BSD-style option letters have no leading dash. The `e`
            // modifier includes process environments, so it is not readonly.
            // Keep this aligned with TS readOnlyValidation.ts.
            additional_check: Some(|_cmd, args| {
                args.iter().any(|arg| {
                    !arg.starts_with('-')
                        && arg.contains('e')
                        && arg.chars().all(|c| c.is_ascii_alphabetic())
                })
            }),
        },
    );

    m.insert(
        "netstat",
        CommandConfig {
            safe_flags: flags(&[
                ("-a", FlagArgType::None),
                ("-l", FlagArgType::None),
                ("-n", FlagArgType::None),
                ("-t", FlagArgType::None),
                ("-u", FlagArgType::None),
                ("-p", FlagArgType::None),
                ("-r", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-v", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "base64",
        CommandConfig {
            safe_flags: flags(&[
                ("-d", FlagArgType::None),
                ("-D", FlagArgType::None),
                ("-w", FlagArgType::Number),
                ("--decode", FlagArgType::None),
                ("--wrap", FlagArgType::Number),
                ("--help", FlagArgType::None),
            ]),
            respects_double_dash: false,
            additional_check: None,
        },
    );

    m.insert(
        "date",
        CommandConfig {
            safe_flags: flags(&[
                ("-d", FlagArgType::StringArg),
                ("-r", FlagArgType::StringArg),
                ("-u", FlagArgType::None),
                ("-R", FlagArgType::None),
                ("-I", FlagArgType::None),
                ("--date", FlagArgType::StringArg),
                ("--utc", FlagArgType::None),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: Some(|_cmd, args| {
                args.iter().any(|a| *a == "-s" || a.starts_with("--set"))
            }),
        },
    );

    m.insert(
        "hostname",
        CommandConfig {
            safe_flags: flags(&[
                ("-f", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-i", FlagArgType::None),
                ("-I", FlagArgType::None),
                ("-a", FlagArgType::None),
                ("-d", FlagArgType::None),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: Some(|_cmd, args| {
                filter_out_flags(args).iter().any(|a| !a.is_empty())
            }),
        },
    );

    m.insert(
        "tree",
        CommandConfig {
            safe_flags: flags(&[
                ("-a", FlagArgType::None),
                ("-d", FlagArgType::None),
                ("-f", FlagArgType::None),
                ("-i", FlagArgType::None),
                ("-l", FlagArgType::None),
                ("-p", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-h", FlagArgType::None),
                ("-D", FlagArgType::None),
                ("-F", FlagArgType::None),
                ("-n", FlagArgType::None),
                ("-C", FlagArgType::None),
                ("-t", FlagArgType::None),
                ("-r", FlagArgType::None),
                ("-L", FlagArgType::Number),
                ("-P", FlagArgType::StringArg),
                ("-I", FlagArgType::StringArg),
                ("--noreport", FlagArgType::None),
                ("--dirsfirst", FlagArgType::None),
                ("-J", FlagArgType::None),
                ("-X", FlagArgType::None),
                ("--help", FlagArgType::None),
                ("--version", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "pyright",
        CommandConfig {
            safe_flags: flags(&[
                ("--outputjson", FlagArgType::None),
                ("--project", FlagArgType::StringArg),
                ("-p", FlagArgType::StringArg),
                ("--pythonversion", FlagArgType::StringArg),
                ("--stats", FlagArgType::None),
                ("--verbose", FlagArgType::None),
                ("--version", FlagArgType::None),
                ("--dependencies", FlagArgType::None),
            ]),
            respects_double_dash: false,
            additional_check: Some(|_cmd, args| args.iter().any(|a| *a == "--watch" || *a == "-w")),
        },
    );

    // ─── Git Commands ───

    let git_display_flags = flags(&[
        ("--stat", FlagArgType::None),
        ("--numstat", FlagArgType::None),
        ("--name-only", FlagArgType::None),
        ("--name-status", FlagArgType::None),
        ("--color", FlagArgType::StringArg),
        ("--no-color", FlagArgType::None),
        ("-p", FlagArgType::None),
        ("--patch", FlagArgType::None),
        ("-u", FlagArgType::None),
        ("-s", FlagArgType::None),
        ("--no-patch", FlagArgType::None),
        ("--raw", FlagArgType::None),
        ("--diff-filter", FlagArgType::StringArg),
        ("-U", FlagArgType::Number),
        ("--unified", FlagArgType::Number),
        ("-b", FlagArgType::None),
        ("-w", FlagArgType::None),
        ("-M", FlagArgType::None),
        ("-C", FlagArgType::None),
        ("-a", FlagArgType::None),
        ("--text", FlagArgType::None),
        ("--full-index", FlagArgType::None),
        ("--binary", FlagArgType::None),
        ("--relative", FlagArgType::None),
        ("--check", FlagArgType::None),
        ("--word-diff", FlagArgType::None),
    ]);

    let mut git_log_flags = git_display_flags.clone();
    for (k, v) in &[
        ("--oneline", FlagArgType::None),
        ("--graph", FlagArgType::None),
        ("--all", FlagArgType::None),
        ("--decorate", FlagArgType::None),
        ("--format", FlagArgType::StringArg),
        ("--pretty", FlagArgType::StringArg),
        ("-n", FlagArgType::Number),
        ("--max-count", FlagArgType::Number),
        ("--since", FlagArgType::StringArg),
        ("--until", FlagArgType::StringArg),
        ("--after", FlagArgType::StringArg),
        ("--before", FlagArgType::StringArg),
        ("--author", FlagArgType::StringArg),
        ("--grep", FlagArgType::StringArg),
        ("--first-parent", FlagArgType::None),
        ("--no-merges", FlagArgType::None),
        ("--follow", FlagArgType::None),
        ("--reverse", FlagArgType::None),
        ("--date", FlagArgType::StringArg),
        ("--abbrev-commit", FlagArgType::None),
        ("-g", FlagArgType::None),
    ] {
        git_log_flags.insert(k, *v);
    }

    m.insert(
        "git diff",
        CommandConfig {
            safe_flags: git_display_flags.clone(),
            respects_double_dash: true,
            additional_check: None,
        },
    );
    m.insert(
        "git log",
        CommandConfig {
            safe_flags: git_log_flags.clone(),
            respects_double_dash: true,
            additional_check: None,
        },
    );
    m.insert(
        "git show",
        CommandConfig {
            safe_flags: git_log_flags.clone(),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "git status",
        CommandConfig {
            safe_flags: flags(&[
                ("-s", FlagArgType::None),
                ("--short", FlagArgType::None),
                ("-b", FlagArgType::None),
                ("--branch", FlagArgType::None),
                ("--porcelain", FlagArgType::None),
                ("-u", FlagArgType::StringArg),
                ("--ignored", FlagArgType::None),
                ("-z", FlagArgType::None),
                ("--long", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "git blame",
        CommandConfig {
            safe_flags: flags(&[
                ("-L", FlagArgType::StringArg),
                ("-l", FlagArgType::None),
                ("-t", FlagArgType::None),
                ("-p", FlagArgType::None),
                ("--porcelain", FlagArgType::None),
                ("-w", FlagArgType::None),
                ("-M", FlagArgType::None),
                ("-C", FlagArgType::None),
                ("--date", FlagArgType::StringArg),
                ("-s", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "git ls-files",
        CommandConfig {
            safe_flags: flags(&[
                ("-c", FlagArgType::None),
                ("-d", FlagArgType::None),
                ("-m", FlagArgType::None),
                ("-o", FlagArgType::None),
                ("-i", FlagArgType::None),
                ("-s", FlagArgType::None),
                ("-z", FlagArgType::None),
                ("-x", FlagArgType::StringArg),
                ("--exclude", FlagArgType::StringArg),
                ("--exclude-standard", FlagArgType::None),
                ("--full-name", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "git branch",
        CommandConfig {
            safe_flags: flags(&[
                ("-a", FlagArgType::None),
                ("-r", FlagArgType::None),
                ("-l", FlagArgType::None),
                ("-v", FlagArgType::None),
                ("--all", FlagArgType::None),
                ("--list", FlagArgType::None),
                ("--sort", FlagArgType::StringArg),
                ("--contains", FlagArgType::StringArg),
                ("--merged", FlagArgType::StringArg),
                ("--format", FlagArgType::StringArg),
                ("--color", FlagArgType::StringArg),
                ("--no-color", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: Some(|_cmd, args| {
                let has_list = args
                    .iter()
                    .any(|a| matches!(*a, "-l" | "--list" | "-a" | "--all" | "-r" | "--remotes"));
                if has_list {
                    false
                } else {
                    !filter_out_flags(args).is_empty()
                }
            }),
        },
    );

    m.insert(
        "git tag",
        CommandConfig {
            safe_flags: flags(&[
                ("-l", FlagArgType::None),
                ("--list", FlagArgType::None),
                ("-n", FlagArgType::Number),
                ("--sort", FlagArgType::StringArg),
                ("--contains", FlagArgType::StringArg),
                ("--format", FlagArgType::StringArg),
            ]),
            respects_double_dash: true,
            additional_check: Some(|_cmd, args| {
                let has_list = args.iter().any(|a| *a == "-l" || *a == "--list");
                if has_list {
                    false
                } else {
                    !filter_out_flags(args).is_empty()
                }
            }),
        },
    );

    m.insert(
        "git rev-parse",
        CommandConfig {
            safe_flags: flags(&[
                ("--verify", FlagArgType::None),
                ("--short", FlagArgType::None),
                ("--abbrev-ref", FlagArgType::None),
                ("--show-toplevel", FlagArgType::None),
                ("--show-cdup", FlagArgType::None),
                ("--show-prefix", FlagArgType::None),
                ("--git-dir", FlagArgType::None),
                ("--is-inside-work-tree", FlagArgType::None),
                ("--is-bare-repository", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "git remote",
        CommandConfig {
            safe_flags: flags(&[("-v", FlagArgType::None), ("--verbose", FlagArgType::None)]),
            respects_double_dash: true,
            additional_check: Some(|_cmd, args| !filter_out_flags(args).is_empty()),
        },
    );

    m.insert(
        "git stash list",
        CommandConfig {
            safe_flags: git_log_flags.clone(),
            respects_double_dash: true,
            additional_check: None,
        },
    );
    m.insert(
        "git stash show",
        CommandConfig {
            safe_flags: git_display_flags.clone(),
            respects_double_dash: true,
            additional_check: None,
        },
    );
    m.insert(
        "git describe",
        CommandConfig {
            safe_flags: flags(&[
                ("--tags", FlagArgType::None),
                ("--all", FlagArgType::None),
                ("--long", FlagArgType::None),
                ("--always", FlagArgType::None),
                ("--abbrev", FlagArgType::Number),
                ("--match", FlagArgType::StringArg),
                ("--exact-match", FlagArgType::None),
                ("--contains", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m.insert(
        "git merge-base",
        CommandConfig {
            safe_flags: flags(&[
                ("--is-ancestor", FlagArgType::None),
                ("--fork-point", FlagArgType::None),
                ("--all", FlagArgType::None),
            ]),
            respects_double_dash: true,
            additional_check: None,
        },
    );

    m
}

fn flags(entries: &[(&'static str, FlagArgType)]) -> HashMap<&'static str, FlagArgType> {
    entries.iter().copied().collect()
}

// ─── Internal Helpers ───

fn validate_readonly_args(_cmd: &str, args: &[&str]) -> bool {
    for arg in args {
        if *arg == ">" || *arg == ">>" || *arg == "|" || arg.starts_with('>') {
            return false;
        }
    }
    true
}

fn is_simple_readonly_pattern(command: &str) -> bool {
    let trimmed = command.trim();
    let simple = [
        "echo", "pwd", "whoami", "hostname", "date", "ls", "tree", "find", "grep", "rg", "fd",
        "file",
    ];
    for &cmd in &simple {
        if trimmed == cmd || trimmed.starts_with(&format!("{cmd} ")) {
            if cmd == "find" {
                return !has_dangerous_find_flags(trimmed);
            }
            return true;
        }
    }
    false
}

fn is_version_check(command: &str) -> bool {
    let patterns = [
        "node -v",
        "node --version",
        "npm -v",
        "npm --version",
        "python --version",
        "python3 --version",
        "rustc --version",
        "cargo --version",
        "go version",
        "git --version",
        "git version",
        "docker --version",
        "gcc --version",
        "make --version",
        "ruby --version",
    ];
    patterns
        .iter()
        .any(|p| command == *p || command.starts_with(&format!("{p} ")))
}

fn has_dangerous_find_flags(command: &str) -> bool {
    let dangerous = [
        "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fls", "-fprintf",
    ];
    let parts: Vec<&str> = command.split_whitespace().collect();
    for part in &parts {
        if dangerous.iter().any(|d| part.to_lowercase() == *d) {
            return true;
        }
    }
    let bytes = command.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'<' | b'>' => return true,
            b'(' | b')' if i == 0 || bytes[i - 1] != b'\\' => return true,
            b'$' | b'`' | b'|' | b'&' | b';' | b'{' | b'}' => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_readonly() {
        assert!(is_command_readonly("cat file.txt"));
        assert!(is_command_readonly("head -n 10 file.txt"));
        assert!(is_command_readonly("wc -l file.txt"));
        assert!(is_command_readonly("diff file1 file2"));
    }

    #[test]
    fn test_echo_readonly() {
        assert!(is_command_readonly("echo hello"));
        assert!(is_command_readonly("echo"));
    }

    #[test]
    fn test_pwd_whoami() {
        assert!(is_command_readonly("pwd"));
        assert!(is_command_readonly("whoami"));
        assert!(is_command_readonly("hostname"));
    }

    #[test]
    fn test_version_checks() {
        assert!(is_command_readonly("node --version"));
        assert!(is_command_readonly("rustc --version"));
        assert!(is_command_readonly("git --version"));
        assert!(is_command_readonly("go version"));
    }

    #[test]
    fn test_ls_readonly() {
        assert!(is_command_readonly("ls"));
        assert!(is_command_readonly("ls -la"));
    }

    #[test]
    fn test_non_readonly() {
        assert!(!is_command_readonly("rm file.txt"));
        assert!(!is_command_readonly("mkdir dir"));
        assert!(!is_command_readonly("cp src dst"));
        assert!(!is_command_readonly("mv src dst"));
    }

    #[test]
    fn test_expansion_blocks() {
        assert!(!is_command_readonly("echo $HOME"));
        assert!(!is_command_readonly("cat $(ls)"));
    }

    #[test]
    fn test_find_safe() {
        assert!(is_command_readonly("find . -name '*.rs'"));
    }

    #[test]
    fn test_find_dangerous() {
        assert!(!is_command_readonly("find . -exec rm {} \\;"));
        assert!(!is_command_readonly("find . -delete"));
    }

    #[test]
    fn test_sleep() {
        assert!(is_command_readonly("sleep 5"));
    }

    #[test]
    fn test_grep_rg() {
        assert!(is_command_readonly("grep -r pattern ."));
        assert!(is_command_readonly("rg pattern"));
    }

    #[test]
    fn test_git_diff() {
        assert!(is_command_readonly("git diff"));
        assert!(is_command_readonly("git diff --stat"));
    }

    #[test]
    fn test_git_log() {
        assert!(is_command_readonly("git log"));
        assert!(is_command_readonly("git log --oneline -n 10"));
    }

    #[test]
    fn test_git_status() {
        assert!(is_command_readonly("git status"));
        assert!(is_command_readonly("git status -s"));
    }

    #[test]
    fn test_git_branch_list() {
        assert!(is_command_readonly("git branch -a"));
        assert!(is_command_readonly("git branch --list"));
    }

    #[test]
    fn test_ps() {
        assert!(is_command_readonly("ps -ef"));
    }

    #[test]
    fn environment_dump_commands_are_not_readonly() {
        for command in [
            "env",
            "printenv",
            "env rm -rf /",
            "env bash -lc 'echo x'",
            "env sh -c 'echo x'",
        ] {
            assert!(
                !is_command_readonly(command),
                "{command} must require an explicit permission decision"
            );
        }
    }

    #[test]
    fn ps_bsd_environment_modifier_is_not_readonly() {
        for command in ["ps e", "ps axe", "ps auxe"] {
            assert!(
                !is_command_readonly(command),
                "{command} exposes process environments and must not be readonly"
            );
        }
        assert!(is_command_readonly("ps aux"));
        assert!(is_command_readonly("ps -ef"));
    }

    #[test]
    fn test_validate_flags() {
        let safe = flags(&[("-n", FlagArgType::Number), ("-l", FlagArgType::None)]);
        assert!(validate_flags(&["-n", "10", "file"], &safe, true));
        assert!(validate_flags(&["-l", "file"], &safe, true));
        assert!(!validate_flags(&["--unknown"], &safe, true));
    }

    #[test]
    fn test_validate_flags_equals() {
        let safe = flags(&[("--color", FlagArgType::StringArg)]);
        assert!(validate_flags(&["--color=always"], &safe, true));
    }

    #[test]
    fn test_validate_flags_dd() {
        let safe = flags(&[("-l", FlagArgType::None)]);
        assert!(validate_flags(&["-l", "--", "-dangerous"], &safe, true));
    }

    #[test]
    fn test_date_blocks_set() {
        assert!(!is_command_readonly("date -s '2024-01-01'"));
    }

    #[test]
    fn test_empty() {
        assert!(!is_command_readonly(""));
    }

    #[test]
    fn test_filter_out_flags() {
        assert_eq!(filter_out_flags(&["-l", "file.txt"]), vec!["file.txt"]);
        assert_eq!(filter_out_flags(&["--", "-x"]), vec!["-x"]);
    }

    #[test]
    fn test_contains_unquoted_expansion() {
        assert!(contains_unquoted_expansion("echo $HOME"));
        assert!(!contains_unquoted_expansion("echo '$HOME'"));
        assert!(contains_unquoted_expansion("echo \"$HOME\""));
    }
}
