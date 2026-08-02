//! 共享常量集
//!
//! 等价于 TS bashPermissions.ts / powershellPermissions.ts 中的常量

use std::collections::HashSet;

// ─── Safe Environment Variables ───

/// 安全环境变量 — 精确匹配 TS bashPermissions.ts `SAFE_ENV_VARS`
///
/// SECURITY: 以下变量经过精心审计，故意排除：
/// - PATH — 控制二进制解析，执行劫持
/// - `NODE_OPTIONS` — 可包含代码执行标志（如 --require）
/// - `NODE_PATH` — 模块加载，任意代码执行
/// - PYTHONPATH — 模块加载，任意代码执行
/// - GOFLAGS/RUSTFLAGS — 可注入编译器标志
/// - HOME/SHELL/TMPDIR — 影响系统行为
/// - EDITOR/VISUAL/PAGER — 可执行任意命令
#[must_use]
pub fn safe_env_vars() -> HashSet<&'static str> {
    [
        // Go
        "GOEXPERIMENT",
        "GOOS",
        "GOARCH",
        "CGO_ENABLED",
        "GO111MODULE",
        // Rust
        "RUST_BACKTRACE",
        "RUST_LOG",
        // Node (仅 NODE_ENV — 不含 NODE_OPTIONS/NODE_PATH！)
        "NODE_ENV",
        // Python (仅安全变量 — 不含 PYTHONPATH！)
        "PYTHONUNBUFFERED",
        "PYTHONDONTWRITEBYTECODE",
        // Pytest
        "PYTEST_DISABLE_PLUGIN_AUTOLOAD",
        "PYTEST_DEBUG",
        // Locale/encoding
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "LC_CTYPE",
        "LC_TIME",
        "CHARSET",
        // Terminal/display (不含 PAGER/EDITOR — 可执行命令！)
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "FORCE_COLOR",
        "TZ",
        // Color config
        "LS_COLORS",
        "LSCOLORS",
        "GREP_COLOR",
        "GREP_COLORS",
        "GCC_COLORS",
        // Display formatting
        "TIME_STYLE",
        "BLOCK_SIZE",
        "BLOCKSIZE",
    ]
    .into_iter()
    .collect()
}

// ─── Bare Shell Prefixes ───

/// 裸 shell 和包装器前缀（传递执行子命令）
#[must_use]
pub fn bare_shell_prefixes() -> HashSet<&'static str> {
    [
        "sh",
        "bash",
        "zsh",
        "fish",
        "csh",
        "tcsh",
        "ksh",
        "dash",
        "cmd",
        "powershell",
        "pwsh",
        "env",
        "xargs",
        "sudo",
        "doas",
        "pkexec",
        "nice",
        "stdbuf",
        "nohup",
        "timeout",
        "time",
    ]
    .into_iter()
    .collect()
}

// ─── Dangerous Files ───

/// 危险配置文件（修改可能影响工具行为）
#[must_use]
pub fn dangerous_files() -> HashSet<&'static str> {
    [
        ".gitconfig",
        ".gitmodules",
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".zprofile",
        ".profile",
        ".ripgreprc",
        ".mcp.json",
        ".crabcode.json",
    ]
    .into_iter()
    .collect()
}

// ─── Dangerous Directories ───

/// 危险目录（包含敏感配置或元数据）
#[must_use]
pub fn dangerous_directories() -> HashSet<&'static str> {
    [".git", ".vscode", ".idea", ".crabcode"]
        .into_iter()
        .collect()
}

// ─── Readonly Commands ───

/// 只读命令集（50+ 条）
#[must_use]
pub fn readonly_commands() -> HashSet<&'static str> {
    [
        "cal", "uptime", "cat", "head", "tail", "wc", "stat", "strings", "hexdump", "od", "nl",
        "id", "uname", "free", "df", "du", "locale", "groups", "nproc", "basename", "dirname",
        "realpath", "readlink", "cut", "paste", "tr", "column", "tac", "rev", "fold", "expand",
        "unexpand", "fmt", "comm", "cmp", "numfmt", "diff", "true", "false", "sleep", "which",
        "type", "expr", "test", "getconf", "seq", "tsort", "pr",
    ]
    .into_iter()
    .collect()
}

// ─── ZSH Dangerous Commands ───

/// zsh 特有的危险命令 — 完整匹配 TS ast.ts `ZSH_DANGEROUS_BUILTINS`
#[must_use]
pub fn zsh_dangerous_commands() -> HashSet<&'static str> {
    [
        // 模块加载（gateway to dangerous modules）
        "zmodload", // 代码执行
        "emulate",  // zsh/system 文件 I/O
        "sysopen", "sysread", "syswrite", "sysseek", // 伪终端执行
        "zpty",    // 网络（数据泄露）
        "ztcp", "zsocket", // 不可见文件 I/O via 数组
        "mapfile", // 模块内置文件操作
        "zf_rm", "zf_mv", "zf_ln", "zf_chmod", "zf_chown", "zf_mkdir", "zf_rmdir", "zf_chgrp",
    ]
    .into_iter()
    .collect()
}

// ─── Command Substitution Patterns ───

/// 命令替换/危险展开模式 — 完整匹配 TS bashSecurity.ts `COMMAND_SUBSTITUTION_PATTERNS`
#[must_use]
pub fn command_substitution_patterns() -> Vec<&'static str> {
    vec![
        "`",        // backtick substitution
        "$(",       // modern command substitution
        "$((",      // arithmetic substitution
        "${",       // parameter expansion
        "$[",       // legacy arithmetic (bash)
        "<(",       // process substitution (input)
        ">(",       // process substitution (output)
        "=(",       // zsh process substitution
        "~[",       // zsh parameter expansion
        "(e:",      // zsh glob qualifiers
        "(+",       // zsh glob qualifier with execution
        "<#",       // PowerShell comment (defense-in-depth)
        "eval ",    // eval execution
        "eval\t",   // eval with tab
        "source ",  // source execution
        "source\t", // source with tab
        ". ",       // dot sourcing
    ]
}

/// zsh try/always 块模式（需要正则匹配）
#[must_use]
pub fn zsh_always_block_pattern(s: &str) -> bool {
    // } always { 模式
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(8) {
        if bytes[i] == b'}' {
            let rest = &s[i + 1..];
            let trimmed = rest.trim_start();
            if let Some(rest) = trimmed.strip_prefix("always") {
                let after = rest.trim_start();
                if after.starts_with('{') {
                    return true;
                }
            }
        }
    }
    false
}

/// zsh equals 展开检测: (?:^|[\s;&|])=[a-zA-Z_]
#[must_use]
pub fn has_zsh_equals_expansion(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'=' && bytes[i + 1].is_ascii_alphabetic() {
            if i == 0 {
                return true;
            }
            if matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'&' | b'|') {
                return true;
            }
        }
    }
    false
}

// ─── PowerShell Security Constants ───

/// `PowerShell` 可执行文件名
#[must_use]
pub fn powershell_executables() -> HashSet<&'static str> {
    ["powershell", "powershell.exe", "pwsh", "pwsh.exe"]
        .into_iter()
        .collect()
}

/// 下载器名称（用于检测 download cradle）
#[must_use]
pub fn downloader_names() -> HashSet<&'static str> {
    [
        "invoke-webrequest",
        "iwr",
        "invoke-restmethod",
        "irm",
        "wget",
        "curl",
        "start-bitstransfer",
        "system.net.webclient",
        "system.net.httpwebrequest",
        "system.net.http.httpclient",
        "net.webclient",
        "net.httpwebrequest",
        "net.http.httpclient",
    ]
    .into_iter()
    .collect()
}

/// 计划任务 cmdlet
#[must_use]
pub fn scheduled_task_cmdlets() -> HashSet<&'static str> {
    [
        "register-scheduledjob",
        "register-scheduledtask",
        "new-scheduledtask",
        "new-scheduledtasktrigger",
        "new-scheduledtaskaction",
        "set-scheduledtask",
        "register-objectevent",
        "register-engineevent",
        "register-wmievent",
    ]
    .into_iter()
    .collect()
}

/// 环境写入 cmdlet
#[must_use]
pub fn env_write_cmdlets() -> HashSet<&'static str> {
    [
        "set-item",
        "new-item",
        "remove-item",
        "clear-item",
        "set-content",
        "add-content",
    ]
    .into_iter()
    .collect()
}

/// 运行时状态操作 cmdlet
#[must_use]
pub fn runtime_state_cmdlets() -> HashSet<&'static str> {
    [
        "set-alias",
        "sal",
        "new-alias",
        "nal",
        "set-variable",
        "sv",
        "new-variable",
        "nv",
        "set-executionpolicy",
        "set-psbreakpoint",
        "set-strictmode",
        "add-type",
    ]
    .into_iter()
    .collect()
}

/// 安全的脚本块 cmdlet（不执行任意脚本块）
#[must_use]
pub fn safe_script_block_cmdlets() -> HashSet<&'static str> {
    [
        "where-object",
        "?",
        "foreach-object",
        "%",
        "select-object",
        "sort-object",
        "group-object",
        "measure-object",
        "format-table",
        "ft",
        "format-list",
        "fl",
        "format-wide",
        "fw",
        "format-custom",
    ]
    .into_iter()
    .collect()
}

/// 下载工具可执行文件名
#[must_use]
pub fn download_utility_names() -> HashSet<&'static str> {
    [
        "curl",
        "curl.exe",
        "wget",
        "wget.exe",
        "aria2c",
        "aria2c.exe",
        "bitsadmin",
        "bitsadmin.exe",
        "certutil",
        "certutil.exe",
    ]
    .into_iter()
    .collect()
}

/// WMI 进程生成 cmdlet
#[must_use]
pub fn wmi_process_spawn_cmdlets() -> HashSet<&'static str> {
    [
        "invoke-wmimethod",
        "iwmi",
        "invoke-cimmethod",
        "get-wmiobject",
        "gwmi",
        "get-ciminstance",
    ]
    .into_iter()
    .collect()
}

/// 模块加载 cmdlet
#[must_use]
pub fn module_loading_cmdlets() -> HashSet<&'static str> {
    [
        "import-module",
        "ipmo",
        "install-module",
        "save-module",
        "update-module",
        "install-script",
        "save-script",
        "install-package",
        "register-psrepository",
        "set-psrepository",
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_env_vars() {
        let vars = safe_env_vars();
        // SECURITY: 危险变量必须不在安全列表中
        assert!(!vars.contains("PATH"), "PATH must NOT be in safe_env_vars");
        assert!(
            !vars.contains("NODE_OPTIONS"),
            "NODE_OPTIONS must NOT be in safe_env_vars"
        );
        assert!(
            !vars.contains("PYTHONPATH"),
            "PYTHONPATH must NOT be in safe_env_vars"
        );
        assert!(
            !vars.contains("RUSTFLAGS"),
            "RUSTFLAGS must NOT be in safe_env_vars"
        );
        assert!(!vars.contains("HOME"), "HOME must NOT be in safe_env_vars");
        assert!(
            !vars.contains("ACOSMI_API_KEY"),
            "authentication secrets must NOT be in safe_env_vars"
        );
        assert!(
            !vars.contains("EDITOR"),
            "EDITOR must NOT be in safe_env_vars"
        );
        // 安全变量应在列表中
        assert!(vars.contains("NODE_ENV"));
        assert!(vars.contains("RUST_BACKTRACE"));
        assert!(vars.contains("GOEXPERIMENT"));
        assert!(vars.contains("TERM"));
        assert!(vars.contains("LANG"));
        assert!(vars.len() >= 28);
    }

    #[test]
    fn test_bare_shell_prefixes() {
        let prefixes = bare_shell_prefixes();
        assert!(prefixes.contains("bash"));
        assert!(prefixes.contains("sudo"));
        assert!(prefixes.contains("env"));
        assert!(prefixes.contains("timeout"));
    }

    #[test]
    fn test_dangerous_files() {
        let files = dangerous_files();
        assert!(files.contains(".gitconfig"));
        assert!(files.contains(".bashrc"));
        assert!(files.contains(".crabcode.json"));
    }

    #[test]
    fn test_readonly_commands() {
        let cmds = readonly_commands();
        assert!(cmds.contains("cat"));
        assert!(cmds.contains("wc"));
        assert!(cmds.contains("diff"));
        assert!(cmds.contains("sleep"));
        assert!(cmds.len() >= 45);
    }

    #[test]
    fn test_zsh_dangerous_commands() {
        let cmds = zsh_dangerous_commands();
        assert!(cmds.contains("zmodload"));
        assert!(cmds.contains("emulate"));
        assert!(cmds.contains("sysopen"));
        assert!(cmds.contains("zf_rm"));
        assert!(cmds.contains("mapfile"));
        assert_eq!(cmds.len(), 18);
    }

    #[test]
    fn test_command_substitution_patterns() {
        let patterns = command_substitution_patterns();
        assert!(patterns.len() >= 17);
        assert!(patterns.contains(&"$("));
        assert!(patterns.contains(&"`"));
        assert!(patterns.contains(&"$["));
        assert!(patterns.contains(&"~["));
        assert!(patterns.contains(&"=("));
        assert!(patterns.contains(&"<#"));
    }

    #[test]
    fn test_powershell_constants() {
        assert!(downloader_names().contains("invoke-webrequest"));
        assert!(scheduled_task_cmdlets().contains("register-scheduledjob"));
        assert!(env_write_cmdlets().contains("set-item"));
        assert!(powershell_executables().contains("pwsh"));
    }
}
