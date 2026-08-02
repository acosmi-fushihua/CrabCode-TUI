//! 命令级路径提取器
//!
//! 等价于 TS BashTool/pathValidation.ts `PATH_EXTRACTORS` (1,303 LOC)
//! 按命令提取路径参数用于权限检查。
//!
//! 每个提取器知道命令的 flag 语义，正确跳过 flag 值参数，
//! 提取真正的文件路径操作数。

use crate::types::FileOperationType;

// ─── Path Command Info ───

/// 命令路径信息
#[derive(Debug, Clone)]
pub struct CommandPathInfo {
    /// 提取的路径列表
    pub paths: Vec<String>,
    /// 操作类型
    pub operation: FileOperationType,
    /// 命令动作描述
    pub action_verb: &'static str,
}

/// 支持的路径命令集合
const SUPPORTED_PATH_COMMANDS: &[&str] = &[
    "cd",
    "ls",
    "find",
    "mkdir",
    "touch",
    "rm",
    "rmdir",
    "mv",
    "cp",
    "cat",
    "head",
    "tail",
    "sort",
    "uniq",
    "wc",
    "cut",
    "paste",
    "column",
    "tr",
    "file",
    "stat",
    "diff",
    "awk",
    "strings",
    "hexdump",
    "od",
    "base64",
    "nl",
    "grep",
    "rg",
    "sed",
    "git",
    "jq",
    "sha256sum",
    "sha1sum",
    "md5sum",
];

/// 检查命令是否受支持
#[must_use]
pub fn is_supported_path_command(cmd: &str) -> bool {
    SUPPORTED_PATH_COMMANDS.contains(&cmd)
}

/// 提取命令的路径参数
#[must_use]
pub fn extract_paths(cmd_name: &str, args: &[&str]) -> Option<CommandPathInfo> {
    match cmd_name {
        "cd" => Some(extract_cd(args)),
        "ls" => Some(extract_ls(args)),
        "find" => Some(extract_find(args)),
        "mkdir" => Some(extract_mkdir(args)),
        "touch" => Some(extract_touch(args)),
        "rm" => Some(extract_rm(args)),
        "rmdir" => Some(extract_rmdir(args)),
        "mv" => Some(extract_mv(args)),
        "cp" => Some(extract_cp(args)),
        "cat" | "head" | "tail" | "sort" | "uniq" | "wc" | "cut" | "paste" | "column" | "file"
        | "stat" | "diff" | "awk" | "strings" | "hexdump" | "od" | "base64" | "nl" => {
            Some(extract_read_command(cmd_name, args))
        }
        "tr" => Some(extract_tr(args)),
        "grep" => Some(extract_grep(args)),
        "rg" => Some(extract_rg(args)),
        "sed" => Some(extract_sed(args)),
        "git" => extract_git(args),
        "jq" => Some(extract_jq(args)),
        "sha256sum" | "sha1sum" | "md5sum" => Some(extract_read_command(cmd_name, args)),
        _ => None,
    }
}

// ─── Individual Extractors ───

/// cd: 所有参数拼接为单个路径
fn extract_cd(args: &[&str]) -> CommandPathInfo {
    let filtered = filter_out_flags(args);
    let path = if filtered.is_empty() {
        "~".to_string()
    } else {
        filtered.join(" ")
    };
    CommandPathInfo {
        paths: vec![path],
        operation: FileOperationType::Read,
        action_verb: "change directory to",
    }
}

/// ls: 过滤 flags，默认 "."
fn extract_ls(args: &[&str]) -> CommandPathInfo {
    let filtered = filter_out_flags(args);
    let paths = if filtered.is_empty() {
        vec![".".to_string()]
    } else {
        filtered
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    };
    CommandPathInfo {
        paths,
        operation: FileOperationType::Read,
        action_verb: "list",
    }
}

/// find: 复杂 flag 处理
fn extract_find(args: &[&str]) -> CommandPathInfo {
    let mut paths = Vec::new();
    let mut i = 0;
    let mut found_expression = false;

    // find 路径参数在表达式开始前
    let path_ending_flags = [
        "-name",
        "-iname",
        "-path",
        "-ipath",
        "-wholename",
        "-iwholename",
        "-regex",
        "-iregex",
        "-type",
        "-perm",
        "-user",
        "-group",
        "-size",
        "-newer",
        "-newermt",
        "-newerat",
        "-newerct",
        "-maxdepth",
        "-mindepth",
        "-printf",
        "-fprintf",
        "-exec",
        "-execdir",
        "-ok",
        "-okdir",
        "-delete",
        "-print",
        "-print0",
        "-ls",
        "-fls",
        "-prune",
        "-not",
        "!",
        "-and",
        "-or",
        "-a",
        "-o",
        "(",
        ")",
    ];

    let flags_with_args = [
        "-name",
        "-iname",
        "-path",
        "-ipath",
        "-wholename",
        "-iwholename",
        "-regex",
        "-iregex",
        "-type",
        "-perm",
        "-user",
        "-group",
        "-size",
        "-newer",
        "-newermt",
        "-newerat",
        "-newerct",
        "-maxdepth",
        "-mindepth",
        "-printf",
        "-fprintf",
    ];

    while i < args.len() {
        let arg = args[i];

        if arg == "--" {
            i += 1;
            // -- 后的全是路径
            while i < args.len() {
                paths.push(args[i].to_string());
                i += 1;
            }
            break;
        }

        // 检查是否到达表达式开始
        if path_ending_flags.iter().any(|f| arg.starts_with(f))
            || arg.starts_with('-') && found_expression
        {
            break;
        }

        if arg.starts_with('-') {
            found_expression = true;
            // 检查是否有参数
            if flags_with_args.contains(&arg) {
                i += 2; // 跳过 flag 和参数
            } else {
                i += 1;
            }
            continue;
        }

        // 非 flag, 非表达式 → 路径
        paths.push(arg.to_string());
        i += 1;
    }

    if paths.is_empty() {
        paths.push(".".to_string());
    }

    CommandPathInfo {
        paths,
        operation: FileOperationType::Read,
        action_verb: "search in",
    }
}

/// mkdir/touch: 所有非 flag 参数都是路径
fn extract_mkdir(args: &[&str]) -> CommandPathInfo {
    CommandPathInfo {
        paths: filter_out_flags(args)
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        operation: FileOperationType::Create,
        action_verb: "create directory",
    }
}

fn extract_touch(args: &[&str]) -> CommandPathInfo {
    CommandPathInfo {
        paths: filter_out_flags(args)
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        operation: FileOperationType::Create,
        action_verb: "create/touch",
    }
}

/// rm/rmdir: 写操作
fn extract_rm(args: &[&str]) -> CommandPathInfo {
    CommandPathInfo {
        paths: filter_out_flags(args)
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        operation: FileOperationType::Write,
        action_verb: "remove",
    }
}

fn extract_rmdir(args: &[&str]) -> CommandPathInfo {
    CommandPathInfo {
        paths: filter_out_flags(args)
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        operation: FileOperationType::Write,
        action_verb: "remove directory",
    }
}

/// mv/cp: 写操作
fn extract_mv(args: &[&str]) -> CommandPathInfo {
    CommandPathInfo {
        paths: filter_out_flags(args)
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        operation: FileOperationType::Write,
        action_verb: "move",
    }
}

fn extract_cp(args: &[&str]) -> CommandPathInfo {
    CommandPathInfo {
        paths: filter_out_flags(args)
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        operation: FileOperationType::Write,
        action_verb: "copy",
    }
}

/// 通用读命令: cat, head, tail, sort, etc.
fn extract_read_command(cmd_name: &str, args: &[&str]) -> CommandPathInfo {
    let action = match cmd_name {
        "cat" => "read",
        "head" => "read head of",
        "tail" => "read tail of",
        "sort" => "sort",
        "wc" => "count in",
        "diff" => "compare",
        "stat" => "stat",
        "file" => "identify type of",
        _ => "read",
    };

    // 跳过带参数的 flags
    let flags_with_args: &[&str] = match cmd_name {
        "head" | "tail" => &["-n", "-c"],
        "sort" => &["-k", "-t", "-S", "-T"],
        "cut" => &["-d", "-f", "-c", "-b"],
        "awk" => &["-F", "-v", "-f"],
        _ => &[],
    };

    let mut paths = Vec::new();
    let mut i = 0;
    let mut past_double_dash = false;

    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            past_double_dash = true;
            i += 1;
            continue;
        }
        if past_double_dash || !arg.starts_with('-') || arg == "-" {
            paths.push(arg.to_string());
        } else if flags_with_args.contains(&arg) {
            i += 1; // 跳过 flag 参数
        }
        i += 1;
    }

    CommandPathInfo {
        paths,
        operation: FileOperationType::Read,
        action_verb: action,
    }
}

/// tr: 跳过字符集参数
const fn extract_tr(_args: &[&str]) -> CommandPathInfo {
    // tr 不操作文件路径（从 stdin 读取）
    CommandPathInfo {
        paths: vec![],
        operation: FileOperationType::Read,
        action_verb: "translate",
    }
}

/// grep: pattern + file list
fn extract_grep(args: &[&str]) -> CommandPathInfo {
    let flags_with_args = [
        "-e",
        "-f",
        "-m",
        "-A",
        "-B",
        "-C",
        "--regexp",
        "--file",
        "--max-count",
        "--label",
        "--after-context",
        "--before-context",
        "--context",
        "--color",
        "--colour",
        "--include",
        "--exclude",
        "--exclude-dir",
    ];

    let mut paths = Vec::new();
    let mut i = 0;
    let mut pattern_seen = false;
    let mut past_double_dash = false;

    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            past_double_dash = true;
            i += 1;
            continue;
        }
        if past_double_dash {
            paths.push(arg.to_string());
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            if flags_with_args
                .iter()
                .any(|f| arg == *f || arg.starts_with(&format!("{f}=")))
                && !arg.contains('=')
            {
                i += 1;
            } // 跳过参数
            // -e 意味着下一个非 flag 是文件
            if arg == "-e" || arg == "--regexp" {
                pattern_seen = true;
            }
            i += 1;
            continue;
        }
        if pattern_seen {
            paths.push(arg.to_string());
        } else {
            pattern_seen = true; // 第一个非 flag 是 pattern
        }
        i += 1;
    }

    CommandPathInfo {
        paths,
        operation: FileOperationType::Read,
        action_verb: "search in",
    }
}

/// rg: pattern + file list, 默认 "."
fn extract_rg(args: &[&str]) -> CommandPathInfo {
    let flags_with_args = [
        "-e",
        "-f",
        "-t",
        "-T",
        "-g",
        "-m",
        "-A",
        "-B",
        "-C",
        "-j",
        "-M",
        "-r",
        "--regexp",
        "--type",
        "--type-not",
        "--glob",
        "--iglob",
        "--max-count",
        "--max-depth",
        "--max-filesize",
        "--max-columns",
        "--threads",
        "--replace",
        "--color",
        "--encoding",
        "--sort",
        "--sortr",
    ];

    let mut paths = Vec::new();
    let mut i = 0;
    let mut pattern_seen = false;
    let mut past_double_dash = false;

    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            past_double_dash = true;
            i += 1;
            continue;
        }
        if past_double_dash {
            paths.push(arg.to_string());
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            if flags_with_args
                .iter()
                .any(|f| arg == *f || arg.starts_with(&format!("{f}=")))
                && !arg.contains('=')
            {
                i += 1;
            }
            if arg == "-e" || arg == "--regexp" {
                pattern_seen = true;
            }
            i += 1;
            continue;
        }
        if pattern_seen {
            paths.push(arg.to_string());
        } else {
            pattern_seen = true;
        }
        i += 1;
    }

    if paths.is_empty() {
        paths.push(".".to_string());
    }

    CommandPathInfo {
        paths,
        operation: FileOperationType::Read,
        action_verb: "search in",
    }
}

/// sed: script + file list
fn extract_sed(args: &[&str]) -> CommandPathInfo {
    let mut paths = Vec::new();
    let mut i = 0;
    let mut script_seen = false;
    let mut past_double_dash = false;

    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            past_double_dash = true;
            i += 1;
            continue;
        }
        if past_double_dash {
            paths.push(arg.to_string());
            i += 1;
            continue;
        }
        if arg == "-e" || arg == "--expression" || arg == "-f" || arg == "--file" {
            script_seen = true;
            i += 2; // 跳过 flag + script/file
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            i += 1;
            continue;
        }
        if script_seen {
            paths.push(arg.to_string());
        } else {
            script_seen = true;
        }
        i += 1;
    }

    // sed 可以修改文件（-i 选项）— 默认视为写操作
    CommandPathInfo {
        paths,
        operation: FileOperationType::Write,
        action_verb: "edit",
    }
}

/// git: 只验证 git diff --no-index
fn extract_git(args: &[&str]) -> Option<CommandPathInfo> {
    if args.first() == Some(&"diff") && args.contains(&"--no-index") {
        let filtered = filter_out_flags(&args[1..]);
        return Some(CommandPathInfo {
            paths: filtered
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            operation: FileOperationType::Read,
            action_verb: "compare",
        });
    }
    None
}

/// jq: filter + file list
fn extract_jq(args: &[&str]) -> CommandPathInfo {
    let flags_with_args = [
        "-f",
        "--from-file",
        "--arg",
        "--argjson",
        "--slurpfile",
        "--rawfile",
        "--jsonargs",
        "--args",
    ];

    let mut paths = Vec::new();
    let mut i = 0;
    let mut filter_seen = false;
    let mut past_double_dash = false;

    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            past_double_dash = true;
            i += 1;
            continue;
        }
        if past_double_dash {
            paths.push(arg.to_string());
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            if flags_with_args.contains(&arg) {
                if arg == "-f" || arg == "--from-file" {
                    filter_seen = true;
                }
                i += 2; // 跳过 flag + 参数
                continue;
            }
            i += 1;
            continue;
        }
        if filter_seen {
            paths.push(arg.to_string());
        } else {
            filter_seen = true;
        }
        i += 1;
    }

    CommandPathInfo {
        paths,
        operation: FileOperationType::Read,
        action_verb: "process",
    }
}

// ─── Helpers ───

/// 过滤掉 flags
fn filter_out_flags<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut result = Vec::new();
    let mut past_dd = false;
    for &arg in args {
        if arg == "--" {
            past_dd = true;
            continue;
        }
        if past_dd || !arg.starts_with('-') || arg == "-" {
            result.push(arg);
        }
    }
    result
}

// ─── Operation Type Map ───

/// 获取命令的默认操作类型
#[must_use]
pub fn get_command_operation(cmd_name: &str) -> FileOperationType {
    match cmd_name {
        "mkdir" | "touch" => FileOperationType::Create,
        "rm" | "rmdir" | "mv" | "cp" | "sed" => FileOperationType::Write,
        _ => FileOperationType::Read,
    }
}

/// 获取命令的动作描述
#[must_use]
pub fn get_command_action_verb(cmd_name: &str) -> &'static str {
    match cmd_name {
        "cd" => "change directory to",
        "ls" => "list",
        "find" => "search in",
        "mkdir" => "create directory",
        "touch" => "create/touch",
        "rm" => "remove",
        "rmdir" => "remove directory",
        "mv" => "move",
        "cp" => "copy",
        "cat" => "read",
        "grep" | "rg" => "search in",
        "sed" => "edit",
        "diff" => "compare",
        _ => "access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cd() {
        let info = extract_paths("cd", &["/home/user"]).unwrap();
        assert_eq!(info.paths, vec!["/home/user"]);
        assert!(matches!(info.operation, FileOperationType::Read));
    }

    #[test]
    fn test_extract_ls() {
        let info = extract_paths("ls", &["-la", "/tmp"]).unwrap();
        assert_eq!(info.paths, vec!["/tmp"]);

        let info = extract_paths("ls", &["-la"]).unwrap();
        assert_eq!(info.paths, vec!["."]);
    }

    #[test]
    fn test_extract_rm() {
        let info = extract_paths("rm", &["-rf", "file1", "file2"]).unwrap();
        assert_eq!(info.paths, vec!["file1", "file2"]);
        assert!(matches!(info.operation, FileOperationType::Write));
    }

    #[test]
    fn test_extract_cat() {
        let info = extract_paths("cat", &["file1.txt", "file2.txt"]).unwrap();
        assert_eq!(info.paths, vec!["file1.txt", "file2.txt"]);
        assert!(matches!(info.operation, FileOperationType::Read));
    }

    #[test]
    fn test_extract_grep() {
        let info = extract_paths("grep", &["-r", "pattern", "src/"]).unwrap();
        assert_eq!(info.paths, vec!["src/"]);
    }

    #[test]
    fn test_extract_grep_with_e_flag() {
        let info = extract_paths("grep", &["-e", "pat1", "-e", "pat2", "file.txt"]).unwrap();
        assert_eq!(info.paths, vec!["file.txt"]);
    }

    #[test]
    fn test_extract_rg_default_dir() {
        let info = extract_paths("rg", &["pattern"]).unwrap();
        assert_eq!(info.paths, vec!["."]);
    }

    #[test]
    fn test_extract_sed() {
        let info = extract_paths("sed", &["-e", "s/a/b/", "file.txt"]).unwrap();
        assert_eq!(info.paths, vec!["file.txt"]);
        assert!(matches!(info.operation, FileOperationType::Write));
    }

    #[test]
    fn test_extract_find() {
        let info = extract_paths("find", &["/home", "-name", "*.rs"]).unwrap();
        assert_eq!(info.paths, vec!["/home"]);
    }

    #[test]
    fn test_extract_find_default() {
        let info = extract_paths("find", &["-name", "*.rs"]).unwrap();
        assert_eq!(info.paths, vec!["."]);
    }

    #[test]
    fn test_extract_git_diff_no_index() {
        let info = extract_paths("git", &["diff", "--no-index", "a.txt", "b.txt"]).unwrap();
        assert_eq!(info.paths, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn test_extract_git_other() {
        assert!(extract_paths("git", &["commit", "-m", "msg"]).is_none());
    }

    #[test]
    fn test_extract_jq() {
        let info = extract_paths("jq", &[".", "data.json"]).unwrap();
        assert_eq!(info.paths, vec!["data.json"]);
    }

    #[test]
    fn test_extract_with_double_dash() {
        let info = extract_paths("rm", &["--", "-rf"]).unwrap();
        assert_eq!(info.paths, vec!["-rf"]);
    }

    #[test]
    fn test_supported_commands() {
        assert!(is_supported_path_command("cat"));
        assert!(is_supported_path_command("grep"));
        assert!(is_supported_path_command("rm"));
        assert!(!is_supported_path_command("npm"));
    }

    #[test]
    fn test_tr_no_paths() {
        let info = extract_paths("tr", &["a-z", "A-Z"]).unwrap();
        assert!(info.paths.is_empty());
    }

    #[test]
    fn test_head_with_flags() {
        let info = extract_paths("head", &["-n", "10", "file.txt"]).unwrap();
        assert_eq!(info.paths, vec!["file.txt"]);
    }
}
