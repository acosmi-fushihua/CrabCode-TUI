//! 危险 cmdlet 清单
//!
//! 等价于 TS dangerousCmdlets.ts — 6 类危险 cmdlet 列表

use std::collections::HashSet;

/// 文件路径执行 cmdlet
#[must_use]
pub fn filepath_execution_cmdlets() -> HashSet<&'static str> {
    [
        "invoke-command",
        "start-job",
        "start-threadjob",
        "register-scheduledjob",
    ]
    .into_iter()
    .collect()
}

/// 脚本块执行 cmdlet
#[must_use]
pub fn dangerous_script_block_cmdlets() -> HashSet<&'static str> {
    [
        "invoke-command",
        "invoke-expression",
        "start-job",
        "start-threadjob",
        "register-scheduledjob",
        "register-engineevent",
        "register-objectevent",
        "register-wmievent",
        "new-pssession",
        "enter-pssession",
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
    ]
    .into_iter()
    .collect()
}

/// 网络操作 cmdlet
#[must_use]
pub fn network_cmdlets() -> HashSet<&'static str> {
    ["invoke-webrequest", "invoke-restmethod"]
        .into_iter()
        .collect()
}

/// 别名/变量劫持 cmdlet
#[must_use]
pub fn alias_hijack_cmdlets() -> HashSet<&'static str> {
    [
        "set-alias",
        "sal",
        "new-alias",
        "nal",
        "set-variable",
        "sv",
        "new-variable",
        "nv",
    ]
    .into_iter()
    .collect()
}

/// WMI/CIM 进程生成绕过 cmdlet
#[must_use]
pub fn wmi_cim_cmdlets() -> HashSet<&'static str> {
    ["invoke-wmimethod", "iwmi", "invoke-cimmethod"]
        .into_iter()
        .collect()
}

/// 参数限制 cmdlet（允许列表模式）
#[must_use]
pub fn arg_gated_cmdlets() -> HashSet<&'static str> {
    [
        "select-object",
        "sort-object",
        "group-object",
        "where-object",
        "measure-object",
        "write-output",
        "write-host",
        "start-sleep",
        "format-table",
        "format-list",
        "format-wide",
        "format-custom",
        "out-string",
        "out-host",
        "ipconfig",
        "hostname",
        "route",
    ]
    .into_iter()
    .collect()
}

/// Shells and process spawners
#[must_use]
pub fn shells_and_spawners() -> HashSet<&'static str> {
    [
        "pwsh",
        "powershell",
        "cmd",
        "bash",
        "wsl",
        "sh",
        "start-process",
        "start",
        "add-type",
        "new-object",
    ]
    .into_iter()
    .collect()
}

/// Cross-platform code execution entry points (single-word only)
#[must_use]
pub fn cross_platform_code_exec() -> Vec<&'static str> {
    vec![
        "python", "python3", "node", "deno", "ruby", "perl", "php", "lua", "npx", "bunx",
    ]
}

/// 综合不推荐集合
///
/// Commands to never suggest as a wildcard prefix in the permission dialog.
/// Derived from the validator lists above plus shells, foreach-object,
/// cross-platform executors, and their aliases.
#[must_use]
pub fn never_suggest_cmdlets() -> HashSet<&'static str> {
    let mut core = HashSet::new();
    core.extend(shells_and_spawners());
    core.extend(filepath_execution_cmdlets());
    core.extend(dangerous_script_block_cmdlets());
    core.extend(module_loading_cmdlets());
    core.extend(network_cmdlets());
    core.extend(alias_hijack_cmdlets());
    core.extend(wmi_cim_cmdlets());
    core.extend(arg_gated_cmdlets());
    // ForEach-Object's -MemberName (positional: `% Delete`) resolves against
    // the runtime pipeline object — can invoke arbitrary methods.
    core.insert("foreach-object");
    // Cross-platform code executors (single-word only)
    for exec in cross_platform_code_exec() {
        core.insert(exec);
    }
    // Expand aliases: any alias that resolves to a core member is also blocked
    let core_snapshot: Vec<&str> = core.iter().copied().collect();
    for (alias, target) in common_aliases() {
        if core_snapshot
            .iter()
            .any(|&c| c.eq_ignore_ascii_case(target))
        {
            core.insert(alias);
        }
    }
    core
}

/// 常用别名映射 — complete 82-entry list from TS
///
/// SECURITY: The following aliases are deliberately omitted because PS Core 6+
/// removed them (they collide with native executables):
///   'sc'   -> sc.exe (Service Controller)
///   'sort' -> sort.exe
///   'curl' -> curl.exe (shipped with Windows 10 1803+)
///   'wget' -> wget.exe (if installed)
#[must_use]
pub fn common_aliases() -> Vec<(&'static str, &'static str)> {
    vec![
        // Directory listing
        ("ls", "Get-ChildItem"),
        ("dir", "Get-ChildItem"),
        ("gci", "Get-ChildItem"),
        // Content
        ("cat", "Get-Content"),
        ("type", "Get-Content"),
        ("gc", "Get-Content"),
        // Navigation
        ("cd", "Set-Location"),
        ("sl", "Set-Location"),
        ("chdir", "Set-Location"),
        ("pushd", "Push-Location"),
        ("popd", "Pop-Location"),
        ("pwd", "Get-Location"),
        ("gl", "Get-Location"),
        // Items
        ("gi", "Get-Item"),
        ("gp", "Get-ItemProperty"),
        ("ni", "New-Item"),
        ("mkdir", "New-Item"),
        ("md", "New-Item"),
        ("ri", "Remove-Item"),
        ("del", "Remove-Item"),
        ("rd", "Remove-Item"),
        ("rmdir", "Remove-Item"),
        ("rm", "Remove-Item"),
        ("erase", "Remove-Item"),
        ("mi", "Move-Item"),
        ("mv", "Move-Item"),
        ("move", "Move-Item"),
        ("ci", "Copy-Item"),
        ("cp", "Copy-Item"),
        ("copy", "Copy-Item"),
        ("cpi", "Copy-Item"),
        ("si", "Set-Item"),
        ("rni", "Rename-Item"),
        ("ren", "Rename-Item"),
        // Process
        ("ps", "Get-Process"),
        ("gps", "Get-Process"),
        ("kill", "Stop-Process"),
        ("spps", "Stop-Process"),
        ("start", "Start-Process"),
        ("saps", "Start-Process"),
        ("sajb", "Start-Job"),
        ("ipmo", "Import-Module"),
        // Output
        ("echo", "Write-Output"),
        ("write", "Write-Output"),
        ("sleep", "Start-Sleep"),
        // Help
        ("help", "Get-Help"),
        ("man", "Get-Help"),
        ("gcm", "Get-Command"),
        // Service
        ("gsv", "Get-Service"),
        // Variables
        ("gv", "Get-Variable"),
        ("sv", "Set-Variable"),
        // History
        ("h", "Get-History"),
        ("history", "Get-History"),
        // Invoke
        ("iex", "Invoke-Expression"),
        ("iwr", "Invoke-WebRequest"),
        ("irm", "Invoke-RestMethod"),
        ("icm", "Invoke-Command"),
        ("ii", "Invoke-Item"),
        // PSSession — remote code execution surface
        ("nsn", "New-PSSession"),
        ("etsn", "Enter-PSSession"),
        ("exsn", "Exit-PSSession"),
        ("gsn", "Get-PSSession"),
        ("rsn", "Remove-PSSession"),
        // Misc
        ("cls", "Clear-Host"),
        ("clear", "Clear-Host"),
        ("select", "Select-Object"),
        ("where", "Where-Object"),
        ("foreach", "ForEach-Object"),
        ("%", "ForEach-Object"),
        ("?", "Where-Object"),
        ("measure", "Measure-Object"),
        ("ft", "Format-Table"),
        ("fl", "Format-List"),
        ("fw", "Format-Wide"),
        ("oh", "Out-Host"),
        ("ogv", "Out-GridView"),
        // Content manipulation
        ("ac", "Add-Content"),
        ("clc", "Clear-Content"),
        // Write/export
        ("tee", "Tee-Object"),
        ("epcsv", "Export-Csv"),
        ("sp", "Set-ItemProperty"),
        ("rp", "Remove-ItemProperty"),
        ("cli", "Clear-Item"),
        ("epal", "Export-Alias"),
        // Text search
        ("sls", "Select-String"),
        // Variables
        ("nv", "New-Variable"),
        ("rv", "Remove-Variable"),
        // Aliases
        ("sal", "Set-Alias"),
        ("nal", "New-Alias"),
        ("gal", "Get-Alias"),
        // Group/Diff
        ("group", "Group-Object"),
        ("diff", "Compare-Object"),
    ]
}

/// 解析别名到 cmdlet 名
#[must_use]
pub fn resolve_alias(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    common_aliases()
        .into_iter()
        .find(|(alias, _)| alias.to_lowercase() == lower)
        .map(|(_, cmdlet)| cmdlet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_never_suggest() {
        let set = never_suggest_cmdlets();
        assert!(set.contains("invoke-expression"));
        assert!(set.contains("invoke-webrequest"));
        assert!(set.contains("invoke-wmimethod"));
        assert!(set.contains("foreach-object"));
        assert!(set.contains("python"));
        assert!(set.contains("node"));
        // Aliases of core members should be included
        assert!(set.contains("iex")); // alias of invoke-expression
        assert!(set.contains("iwr")); // alias of invoke-webrequest
        assert!(!set.contains("get-childitem"));
    }

    #[test]
    fn test_shells_and_spawners() {
        let set = shells_and_spawners();
        assert!(set.contains("pwsh"));
        assert!(set.contains("cmd"));
        assert!(set.contains("start-process"));
        assert!(set.contains("add-type"));
        assert!(set.contains("new-object"));
    }

    #[test]
    fn test_resolve_alias() {
        assert_eq!(resolve_alias("ls"), Some("Get-ChildItem"));
        assert_eq!(resolve_alias("cd"), Some("Set-Location"));
        assert_eq!(resolve_alias("iex"), Some("Invoke-Expression"));
        assert_eq!(resolve_alias("nonexistent"), None);
    }

    #[test]
    fn test_common_aliases_no_security_collisions() {
        let aliases = common_aliases();
        let alias_names: Vec<&str> = aliases.iter().map(|(a, _)| *a).collect();
        // These must NOT be present (security: collide with native executables)
        assert!(!alias_names.contains(&"sc"));
        assert!(!alias_names.contains(&"sort"));
        assert!(!alias_names.contains(&"curl"));
        assert!(!alias_names.contains(&"wget"));
        // oss was also removed
        assert!(!alias_names.contains(&"oss"));
    }

    #[test]
    fn test_common_aliases_has_required_entries() {
        let aliases = common_aliases();
        let alias_names: Vec<&str> = aliases.iter().map(|(a, _)| *a).collect();
        // Verify key entries that were added
        assert!(alias_names.contains(&"mkdir"));
        assert!(alias_names.contains(&"md"));
        assert!(alias_names.contains(&"del"));
        assert!(alias_names.contains(&"rd"));
        assert!(alias_names.contains(&"rmdir"));
        assert!(alias_names.contains(&"erase"));
        assert!(alias_names.contains(&"mi"));
        assert!(alias_names.contains(&"move"));
        assert!(alias_names.contains(&"ci"));
        assert!(alias_names.contains(&"copy"));
        assert!(alias_names.contains(&"cpi"));
        assert!(alias_names.contains(&"rni"));
        assert!(alias_names.contains(&"ren"));
        assert!(alias_names.contains(&"ps"));
        assert!(alias_names.contains(&"spps"));
        assert!(alias_names.contains(&"start"));
        assert!(alias_names.contains(&"sajb"));
        assert!(alias_names.contains(&"gcm"));
        assert!(alias_names.contains(&"h"));
        assert!(alias_names.contains(&"history"));
        assert!(alias_names.contains(&"ii"));
        assert!(alias_names.contains(&"nsn"));
        assert!(alias_names.contains(&"etsn"));
        assert!(alias_names.contains(&"exsn"));
        assert!(alias_names.contains(&"gsn"));
        assert!(alias_names.contains(&"rsn"));
        assert!(alias_names.contains(&"clear"));
        assert!(alias_names.contains(&"type"));
        assert!(alias_names.contains(&"tee"));
        assert!(alias_names.contains(&"epcsv"));
        assert!(alias_names.contains(&"rp"));
        assert!(alias_names.contains(&"cli"));
        assert!(alias_names.contains(&"epal"));
        assert!(alias_names.contains(&"sls"));
        assert!(alias_names.contains(&"clc"));
        assert!(alias_names.contains(&"ac"));
    }
}
