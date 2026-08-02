//! `PowerShell` 安全扫描器（24 项验证器）
//!
//! 等价于 TS powershellSecurity.ts (~1,090 LOC) + clmTypes.ts (~212 LOC)
//! 检测 `PowerShell` 命令中的危险模式。
//!
//! **生产可达性（P2-1 诚实声明 2026-06-05）**：本扫描器是 TS `powershellSecurity.ts`
//! 的 **Rust 镜像参考实现，当前不在生产 agent 执行路径上**。面向 agent 的 PowerShell
//! 工具（`src/tools/PowerShellTool`，`powershellPermissions.ts::powershellCommandIsSafe`）
//! 在 **TS 侧自行扫描**并经 `LocalShellTask` 执行，不经 Rust supervisor ExecBridge；
//! ExecBridge 仅跑固定内部 bash-shaped 命令（git/worktree/LSP），不跑 PowerShell。
//! 因此 `powershell_command_is_safe` / `check_powershell_permission` 在生产无调用方
//! 属**设计如此**（非缺陷）；若未来确有可达路径需接通，须经新审计立项，勿擅自接 ExecBridge
//! （会改变 Windows 既有 exec 判定 = 回归面）。
//!
//! 安全模式:
//! - 解析失败 → ask (fail-open: 无法静态验证时请求用户确认)
//! - 短路: 第一个 ask 结果立即返回
//! - 参数灵活性: 处理命名, 缩写, 冒号绑定, 位置参数
//! - 替代前缀: en-dash, em-dash, 全角连字符, / 前缀

use std::collections::HashSet;

use acosmi_shell_parser::powershell::parser;
use acosmi_shell_parser::powershell::types::{
    CommandElementType, ParsedCommandElement, ParsedPowerShellCommand, SecurityFlags,
};

use crate::types::SecurityViolation;

// ─── Constants ───

fn powershell_executables() -> HashSet<&'static str> {
    ["pwsh", "pwsh.exe", "powershell", "powershell.exe"]
        .into_iter()
        .collect()
}

/// 检查名称或路径是否指向 `PowerShell` 可执行文件
fn is_powershell_executable(name: &str) -> bool {
    let lower = name.to_lowercase();
    let ps_exes = powershell_executables();

    // 直接名称匹配
    if ps_exes.contains(lower.as_str()) {
        return true;
    }

    // 路径匹配: 提取 basename
    let normalized = lower.replace('\\', "/");
    if let Some(basename) = normalized.rsplit('/').next()
        && ps_exes.contains(basename)
    {
        return true;
    }

    false
}

fn downloader_names() -> HashSet<&'static str> {
    [
        "invoke-webrequest",
        "iwr",
        "invoke-restmethod",
        "irm",
        "new-object",
        "start-bitstransfer",
    ]
    .into_iter()
    .collect()
}

fn safe_script_block_cmdlets() -> HashSet<&'static str> {
    [
        "where-object",
        "sort-object",
        "select-object",
        "group-object",
        "format-table",
        "format-list",
        "format-wide",
        "format-custom",
    ]
    .into_iter()
    .collect()
}

fn dangerous_script_block_cmdlets() -> HashSet<&'static str> {
    [
        "invoke-command",
        "icm",
        "start-job",
        "start-threadjob",
        "register-scheduledjob",
        "register-objectevent",
        "register-engineevent",
        "register-wmievent",
        "invoke-expression",
        "iex",
    ]
    .into_iter()
    .collect()
}

fn scheduled_task_cmdlets() -> HashSet<&'static str> {
    [
        "register-scheduledtask",
        "new-scheduledtask",
        "new-scheduledtaskaction",
        "set-scheduledtask",
    ]
    .into_iter()
    .collect()
}

fn env_write_cmdlets() -> HashSet<&'static str> {
    [
        "set-item",
        "si",
        "new-item",
        "ni",
        "remove-item",
        "ri",
        "del",
        "rm",
        "rd",
        "rmdir",
        "erase",
        "clear-item",
        "cli",
        "set-content",
        "add-content",
        "ac",
    ]
    .into_iter()
    .collect()
}

fn runtime_state_cmdlets() -> HashSet<&'static str> {
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

fn wmi_spawn_cmdlets() -> HashSet<&'static str> {
    ["invoke-wmimethod", "iwmi", "invoke-cimmethod"]
        .into_iter()
        .collect()
}

fn module_loading_cmdlets() -> HashSet<&'static str> {
    [
        "import-module",
        "ipmo",
        "install-module",
        "save-module",
        "update-module",
    ]
    .into_iter()
    .collect()
}

fn filepath_exec_cmdlets() -> HashSet<&'static str> {
    [
        "invoke-command",
        "icm",
        "start-job",
        "start-threadjob",
        "register-scheduledjob",
    ]
    .into_iter()
    .collect()
}

// ─── CLM Type Allowlist ───

/// Constrained Language Mode 允许的 .NET 类型（139 条）
///
/// 等价于 TS clmTypes.ts `CLM_ALLOWED_TYPES`
///
/// SECURITY: 以下类型已被移除:
/// - adsi, adsisearcher: 执行 LDAP 网络绑定
/// - wmi, wmiclass, wmisearcher, cimsession: WMI/CIM 远程查询
/// - directoryentry, directorysearcher: LDAP 网络绑定
/// - managementobject, managementclass, managementobjectsearcher: WMI 网络绑定
fn clm_allowed_types() -> HashSet<&'static str> {
    [
        // 类型加速器（短名）
        "alias",
        "allowemptycollection",
        "allowemptystring",
        "allownull",
        "argumentcompleter",
        "argumentcompletions",
        "array",
        "bigint",
        "bool",
        "byte",
        "char",
        "cimclass",
        "cimconverter",
        "ciminstance",
        "cimtype",
        "cmdletbinding",
        "cultureinfo",
        "datetime",
        "decimal",
        "double",
        "dsclocalconfigurationmanager",
        "dscproperty",
        "dscresource",
        "experimentaction",
        "experimental",
        "experimentalfeature",
        "float",
        "guid",
        "hashtable",
        "int",
        "int16",
        "int32",
        "int64",
        "ipaddress",
        "ipendpoint",
        "long",
        "mailaddress",
        "norunspaceaffinity",
        "nullstring",
        "objectsecurity",
        "ordered",
        "outputtype",
        "parameter",
        "physicaladdress",
        "pscredential",
        "pscustomobject",
        "psdefaultvalue",
        "pslistmodifier",
        "psobject",
        "psprimitivedictionary",
        "pstypenameattribute",
        "ref",
        "regex",
        "sbyte",
        "securestring",
        "semver",
        "short",
        "single",
        "string",
        "supportswildcards",
        "switch",
        "timespan",
        "uint",
        "uint16",
        "uint32",
        "uint64",
        "ulong",
        "uri",
        "ushort",
        "validatecount",
        "validatedrive",
        "validatelength",
        "validatenotnull",
        "validatenotnullorempty",
        "validatenotnullorwhitespace",
        "validatepattern",
        "validaterange",
        "validatescript",
        "validateset",
        "validatetrusteddata",
        "validateuserdrive",
        "version",
        "void",
        "wildcardpattern",
        "x500distinguishedname",
        "x509certificate",
        "xml",
        // 完整 System.* 名
        "system.array",
        "system.boolean",
        "system.byte",
        "system.char",
        "system.datetime",
        "system.decimal",
        "system.double",
        "system.guid",
        "system.int16",
        "system.int32",
        "system.int64",
        "system.numerics.biginteger",
        "system.sbyte",
        "system.single",
        "system.string",
        "system.timespan",
        "system.uint16",
        "system.uint32",
        "system.uint64",
        "system.uri",
        "system.version",
        "system.void",
        "system.collections.hashtable",
        "system.text.regularexpressions.regex",
        "system.globalization.cultureinfo",
        "system.net.ipaddress",
        "system.net.ipendpoint",
        "system.net.mail.mailaddress",
        "system.net.networkinformation.physicaladdress",
        "system.security.securestring",
        "system.security.cryptography.x509certificates.x509certificate",
        "system.security.cryptography.x509certificates.x500distinguishedname",
        "system.xml.xmldocument",
        "system.management.automation.pscredential",
        "system.management.automation.pscustomobject",
        "system.management.automation.pslistmodifier",
        "system.management.automation.psobject",
        "system.management.automation.psprimitivedictionary",
        "system.management.automation.psreference",
        "system.management.automation.semanticversion",
        "system.management.automation.switchparameter",
        "system.management.automation.wildcardpattern",
        "system.management.automation.language.nullstring",
        // Microsoft.Management.Infrastructure (CIM)
        "microsoft.management.infrastructure.cimclass",
        "microsoft.management.infrastructure.cimconverter",
        "microsoft.management.infrastructure.ciminstance",
        "microsoft.management.infrastructure.cimtype",
        "system.collections.specialized.ordereddictionary",
        "system.security.accesscontrol.objectsecurity",
        // 其他安全类型
        "object",
        "system.object",
        "microsoft.powershell.commands.modulespecification",
    ]
    .into_iter()
    .collect()
}

/// 标准化类型名（等价于 TS normalizeTypeName）
///
/// 去除数组后缀 [] 和泛型括号 [T]，转小写
fn normalize_type_name(name: &str) -> String {
    let mut result = name.to_lowercase();
    // 去除数组后缀 []
    while result.ends_with("[]") {
        result = result[..result.len() - 2].to_string();
    }
    // 去除泛型括号 [T]
    if let Some(idx) = result.find('[') {
        result = result[..idx].to_string();
    }
    result.trim().to_string()
}

/// 检查类型是否在 CLM 允许列表中
fn is_clm_allowed_type(type_name: &str) -> bool {
    let normalized = normalize_type_name(type_name);
    clm_allowed_types().contains(normalized.as_str())
}

// ─── Parameter Abbreviation Helper ───

/// 检查命令是否有参数缩写匹配（等价于 TS psExeHasParamAbbreviation）
///
/// `PowerShell` 允许参数缩写: -`EncodedCommand` 可以写为 -e, -en, -enc 等
/// 同时检查替代前缀: / (Windows), en-dash, em-dash, horizontal-bar
fn ps_exe_has_param_abbreviation(
    cmd: &ParsedCommandElement,
    full_param: &str,
    min_prefix: &str,
) -> bool {
    let full_lower = full_param.to_lowercase();
    let min_lower = min_prefix.to_lowercase();

    for arg in &cmd.args {
        let arg_lower = arg.to_lowercase();

        // 标准 - 前缀检查
        if check_param_abbreviation(&arg_lower, &full_lower, &min_lower) {
            return true;
        }

        // 替代前缀检查: /, en-dash, em-dash, horizontal-bar
        let normalized = normalize_param_prefix(arg);
        if let Some(ref norm) = normalized {
            let norm_lower = norm.to_lowercase();
            if check_param_abbreviation(&norm_lower, &full_lower, &min_lower) {
                return true;
            }
        }
    }
    false
}

/// 检查单个参数是否匹配缩写
fn check_param_abbreviation(arg: &str, full_param: &str, min_prefix: &str) -> bool {
    if !arg.starts_with('-') {
        return false;
    }

    // 处理冒号绑定: -param:value
    let param_part = if let Some(colon_idx) = arg.find(':') {
        &arg[..colon_idx]
    } else {
        arg
    };

    // 完全匹配
    if param_part == full_param {
        return true;
    }

    // 缩写匹配: arg 是 full_param 的前缀，且至少有 min_prefix 长度
    if full_param.starts_with(param_part) && param_part.len() >= min_prefix.len() {
        return true;
    }

    false
}

/// 将替代前缀标准化为 -
fn normalize_param_prefix(arg: &str) -> Option<String> {
    let mut chars = arg.chars();
    if let Some(first) = chars.next() {
        let cp = first as u32;
        // / (Windows 前缀), en-dash, em-dash, horizontal-bar
        if first == '/' || cp == 0x2013 || cp == 0x2014 || cp == 0x2015 {
            return Some(format!("-{}", chars.as_str()));
        }
    }
    None
}

/// 剥离模块限定符 (Microsoft.PowerShell.Utility\Set-Alias → Set-Alias)
///
/// 模块限定符格式: ModuleName\CmdletName（只有一个 \）
/// 文件路径格式: C:\path\to\script.ps1（多个 \）或含 /
fn strip_module_qualifier(name: &str) -> &str {
    // 含 / → Unix 路径，不剥离
    if name.contains('/') {
        return name;
    }
    // 含多个 \ → 可能是 Windows 文件路径，不剥离
    if name.matches('\\').count() > 1 {
        return name;
    }
    // 只有一个 \ → 模块限定符格式
    if let Some(idx) = name.rfind('\\') {
        return &name[idx + 1..];
    }
    name
}

// ─── Public API ───

/// 检查 `PowerShell` 命令是否安全
///
/// 返回所有发现的安全违规列表。空列表表示通过。
#[must_use]
pub fn powershell_command_is_safe(parsed: &ParsedPowerShellCommand) -> Vec<SecurityViolation> {
    let mut violations = Vec::new();

    // 解析失败 → 不安全
    if !parsed.valid {
        violations.push(SecurityViolation {
            check_id: 0,
            check_name: "parse_failure".to_string(),
            message: "Could not parse command for security analysis".to_string(),
            is_misparsing: false,
        });
        return violations;
    }

    // Unicode 替代参数前缀检查（在所有其他检查前）
    if let Some(msg) = check_unicode_dash_params(&parsed.original_command) {
        violations.push(SecurityViolation {
            check_id: 25,
            check_name: "unicode_param_prefix".to_string(),
            message: msg,
            is_misparsing: false,
        });
        return violations;
    }

    let commands = parser::get_all_commands(parsed);
    let flags = parser::derive_security_flags(parsed);

    type CheckFn<'a> = Box<dyn Fn() -> Option<String> + 'a>;
    // 按 TS 顺序运行 24 个验证器（短路模式: 第一个 ask 立即返回）
    let checks: Vec<(&str, u32, CheckFn)> = vec![
        (
            "invoke_expression",
            1,
            Box::new(|| check_invoke_expression(&commands)),
        ),
        (
            "dynamic_command_name",
            2,
            Box::new(|| check_dynamic_command_name(&commands)),
        ),
        (
            "encoded_command",
            3,
            Box::new(|| check_encoded_command(&commands)),
        ),
        (
            "pwsh_command_or_file",
            4,
            Box::new(|| check_pwsh_command_or_file(&commands)),
        ),
        (
            "download_cradles",
            5,
            Box::new(|| check_download_cradles(parsed)),
        ),
        (
            "download_utilities",
            6,
            Box::new(|| check_download_utilities(&commands)),
        ),
        ("add_type", 7, Box::new(|| check_add_type(&commands))),
        ("com_object", 8, Box::new(|| check_com_object(&commands))),
        (
            "dangerous_filepath",
            9,
            Box::new(|| check_dangerous_filepath_execution(&commands)),
        ),
        ("invoke_item", 19, Box::new(|| check_invoke_item(&commands))),
        (
            "scheduled_task",
            20,
            Box::new(|| check_scheduled_task(&commands)),
        ),
        (
            "foreach_member_name",
            10,
            Box::new(|| check_foreach_member_name(&commands)),
        ),
        (
            "start_process",
            11,
            Box::new(|| check_start_process(&commands)),
        ),
        (
            "script_block_injection",
            12,
            Box::new(|| check_script_block_injection(&commands, &flags)),
        ),
        (
            "sub_expressions",
            13,
            Box::new(|| check_sub_expressions(&flags)),
        ),
        (
            "expandable_strings",
            14,
            Box::new(|| check_expandable_strings(&flags)),
        ),
        ("splatting", 15, Box::new(|| check_splatting(&flags))),
        ("stop_parsing", 16, Box::new(|| check_stop_parsing(&flags))),
        (
            "member_invocations",
            17,
            Box::new(|| check_member_invocations(&flags)),
        ),
        (
            "type_literals",
            18,
            Box::new(|| check_type_literals(parsed)),
        ),
        (
            "env_var_manipulation",
            21,
            Box::new(|| check_env_var_manipulation(parsed, &commands)),
        ),
        (
            "module_loading",
            22,
            Box::new(|| check_module_loading(&commands)),
        ),
        (
            "runtime_state",
            23,
            Box::new(|| check_runtime_state_manipulation(&commands)),
        ),
        (
            "wmi_process_spawn",
            24,
            Box::new(|| check_wmi_process_spawn(&commands)),
        ),
    ];

    // 短路执行: 第一个违规立即返回
    for (name, id, check_fn) in &checks {
        if let Some(message) = check_fn() {
            violations.push(SecurityViolation {
                check_id: *id,
                check_name: name.to_string(),
                message,
                is_misparsing: false,
            });
            return violations; // 短路
        }
    }

    violations
}

// ─── Unicode Param Prefix Check ───

fn check_unicode_dash_params(raw_command: &str) -> Option<String> {
    for c in raw_command.chars() {
        let cp = c as u32;
        if (0x2010..=0x2015).contains(&cp) || cp == 0x2212 || cp == 0xFE63 || cp == 0xFF0D {
            return Some(format!(
                "Command contains Unicode look-alike parameter prefix U+{cp:04X}"
            ));
        }
    }
    None
}

// ─── Individual Validators ───

/// 1. Invoke-Expression / iex
fn check_invoke_expression(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if name == "invoke-expression" || name == "iex" {
            return Some("Invoke-Expression detected (eval equivalent)".to_string());
        }
    }
    None
}

/// 2. Dynamic command name
fn check_dynamic_command_name(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        if let Some(ref ets) = cmd.element_types
            && let Some(first) = ets.first()
            && !matches!(
                first,
                CommandElementType::StringConstant | CommandElementType::Parameter
            )
        {
            return Some(format!(
                "Command name is a dynamic expression: {}",
                cmd.name
            ));
        }
    }
    None
}

/// 3. Encoded command parameter
fn check_encoded_command(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        if is_powershell_executable(&cmd.name)
            && ps_exe_has_param_abbreviation(cmd, "-encodedcommand", "-e")
        {
            return Some("Encoded command parameter detected".to_string());
        }
    }
    None
}

/// 4. Nested `PowerShell` invocation (bare pwsh 也危险)
fn check_pwsh_command_or_file(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        if is_powershell_executable(&cmd.name) {
            return Some(format!("Nested PowerShell process: {}", cmd.name));
        }
    }
    None
}

/// 5. Download cradle patterns
fn check_download_cradles(parsed: &ParsedPowerShellCommand) -> Option<String> {
    let downloaders = downloader_names();
    // Per-statement 检查
    for stmt in &parsed.statements {
        if stmt.commands.len() >= 2 {
            let has_downloader = stmt
                .commands
                .iter()
                .any(|c| downloaders.contains(c.name.to_lowercase().as_str()));
            let has_iex = stmt.commands.iter().any(|c| {
                let n = c.name.to_lowercase();
                n == "invoke-expression" || n == "iex"
            });
            if has_downloader && has_iex {
                return Some("Download cradle detected (download piped to IEX)".to_string());
            }
        }
    }
    // Cross-statement 检查
    let all_commands: Vec<&ParsedCommandElement> = parsed
        .statements
        .iter()
        .flat_map(|s| s.commands.iter())
        .collect();
    let has_any_downloader = all_commands
        .iter()
        .any(|c| downloaders.contains(c.name.to_lowercase().as_str()));
    let has_any_iex = all_commands.iter().any(|c| {
        let n = c.name.to_lowercase();
        n == "invoke-expression" || n == "iex"
    });
    if has_any_downloader && has_any_iex {
        return Some("Download cradle detected (cross-statement)".to_string());
    }
    None
}

/// 6. LOLBAS download utilities
fn check_download_utilities(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        let name = cmd.name.to_lowercase();

        // Start-BitsTransfer: 始终标记
        if name == "start-bitstransfer" {
            return Some("Start-BitsTransfer detected (file download)".to_string());
        }

        // certutil -urlcache
        if name == "certutil" || name == "certutil.exe" {
            for arg in &cmd.args {
                let lower = arg.to_lowercase();
                if lower.contains("urlcache") || lower == "-urlcache" || lower == "/urlcache" {
                    return Some("certutil -urlcache download detected".to_string());
                }
            }
        }

        // bitsadmin /transfer
        if name == "bitsadmin" || name == "bitsadmin.exe" {
            for arg in &cmd.args {
                let lower = arg.to_lowercase();
                if lower.contains("transfer") || lower == "/transfer" || lower == "-transfer" {
                    return Some("bitsadmin /transfer download detected".to_string());
                }
            }
        }
    }
    None
}

/// 7. Add-Type
fn check_add_type(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        if strip_module_qualifier(&cmd.name).to_lowercase() == "add-type" {
            return Some("Add-Type detected (runtime .NET compilation)".to_string());
        }
    }
    None
}

/// 8. COM object + CLM type check
fn check_com_object(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        if strip_module_qualifier(&cmd.name).to_lowercase() == "new-object" {
            // 检查 -ComObject 参数
            if ps_exe_has_param_abbreviation(cmd, "-comobject", "-com") {
                return Some("New-Object -ComObject detected (COM execution)".to_string());
            }

            // 提取 TypeName 参数并检查 CLM 允许列表
            if let Some(type_name) = extract_new_object_type_name(cmd)
                && !is_clm_allowed_type(&type_name)
            {
                return Some(format!(
                    "New-Object type '{type_name}' outside CLM allowlist"
                ));
            }
        }
    }
    None
}

/// 从 New-Object 命令提取 `TypeName`
fn extract_new_object_type_name(cmd: &ParsedCommandElement) -> Option<String> {
    let args = &cmd.args;

    // 1. 命名参数: -TypeName Foo.Bar 或 -t Foo.Bar
    for i in 0..args.len() {
        let lower = args[i].to_lowercase();
        // 冒号绑定: -TypeName:Foo.Bar
        if let Some(colon_idx) = lower.find(':') {
            let param = &lower[..colon_idx];
            if "-typename".starts_with(param) && param.len() >= 2 && param.starts_with("-t") {
                return Some(args[i][colon_idx + 1..].to_string());
            }
        }
        // 空格分隔: -TypeName Foo.Bar
        if "-typename".starts_with(&lower)
            && lower.len() >= 2
            && lower.starts_with("-t")
            && lower != "-comobject"
            && i + 1 < args.len()
        {
            return Some(args[i + 1].clone());
        }
    }

    // 2. 位置参数: 第一个非参数非命名值参数
    //
    // PowerShell `New-Object` 参数语义分流（与 TS sibling powershellSecurity.ts:393-414 对齐）:
    //   value-param (-ArgumentList / -ComObject / -Property): 形如 `-X <val>`，消费下一个 token
    //   switch-param (-Strict): 形如 `-X`，无值 — 不消费下一个 token
    //   colon-bound (`-X:val`): 单 token，下一个 token 不属本 param
    //
    // 早期实施合并到一个 skip_params 统一 i+=2 → 把 `-Strict <TypeName>` 中的 TypeName 当成
    // -Strict 的值吃掉，CLM 类型白名单整段跳过（HIGH-ps-1 / 2026-05-05 audit）。
    let value_params: HashSet<&str> = ["-argumentlist", "-comobject", "-property"]
        .into_iter()
        .collect();
    let mut i = 0;
    while i < args.len() {
        let lower = args[i].to_lowercase();
        if lower.starts_with('-') {
            // -TypeName 变体（已被命名参数 loop 处理过；这里继续保守消费 value）
            if lower.starts_with("-t") && "-typename".starts_with(lower.as_str()) {
                i += 2;
                continue;
            }
            // colon-bound: `-Param:Value` 单 token，不消费下一个
            if lower.contains(':') {
                i += 1;
                continue;
            }
            // switch param: -Strict 是 New-Object 唯一的 switch（无值），不消费下一个
            if lower == "-strict" {
                i += 1;
                continue;
            }
            if value_params.contains(lower.as_str()) {
                i += 2;
                continue;
            }
            // 未知 -param: 保守只跳 token（与 TS 一致）
            i += 1;
            continue;
        }
        // 第一个非 dash 参数 → TypeName 位置
        return Some(args[i].clone());
    }

    None
}

/// 9. Dangerous file path execution
fn check_dangerous_filepath_execution(commands: &[&ParsedCommandElement]) -> Option<String> {
    let exec_cmdlets = filepath_exec_cmdlets();
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if !exec_cmdlets.contains(name.as_str()) {
            continue;
        }

        // 命名参数: -FilePath / -LiteralPath
        if ps_exe_has_param_abbreviation(cmd, "-filepath", "-f")
            || ps_exe_has_param_abbreviation(cmd, "-literalpath", "-l")
        {
            return Some(format!("{} -FilePath executes arbitrary script", cmd.name));
        }

        // 位置参数: 非 dash StringConstant 参数绑定到 -FilePath
        if let Some(ref ets) = cmd.element_types {
            for (i, et) in ets.iter().enumerate().skip(1) {
                if matches!(et, CommandElementType::StringConstant)
                    && i < cmd.args.len() + 1
                    && !cmd
                        .args
                        .get(i.saturating_sub(1))
                        .is_none_or(|a| a.starts_with('-'))
                {
                    return Some(format!(
                        "{} positional argument binds to -FilePath",
                        cmd.name
                    ));
                }
            }
        }
    }
    None
}

/// 10. ForEach-Object -`MemberName`
fn check_foreach_member_name(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if name == "foreach-object" || name == "%" || name == "foreach" {
            // 命名参数
            if ps_exe_has_param_abbreviation(cmd, "-membername", "-m") {
                return Some("ForEach-Object -MemberName invokes methods by string".to_string());
            }
            // 位置参数: 非 dash StringConstant
            if let Some(ref ets) = cmd.element_types {
                for (i, et) in ets.iter().enumerate().skip(1) {
                    if matches!(et, CommandElementType::StringConstant)
                        && let Some(arg) = cmd.args.get(i.saturating_sub(1))
                        && !arg.starts_with('-')
                    {
                        return Some("ForEach-Object positional binds to -MemberName".to_string());
                    }
                }
            }
        }
    }
    None
}

/// 11. Start-Process with elevation or nested PS
fn check_start_process(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if name != "start-process" && name != "saps" && name != "start" {
            continue;
        }

        // Vector 1: -Verb RunAs (elevation)
        if ps_exe_has_param_abbreviation(cmd, "-verb", "-v") {
            for arg in &cmd.args {
                let stripped = arg.trim_matches('\'').trim_matches('"').replace('`', "");
                if stripped.to_lowercase() == "runas" {
                    return Some("Start-Process -Verb RunAs (elevation)".to_string());
                }
            }
        }

        // 冒号绑定: -V:RunAs, -Verb:RunAs
        for arg in &cmd.args {
            let lower = arg.to_lowercase().replace('`', "");
            if lower.starts_with("-v")
                && lower.contains(':')
                && let Some(colon_idx) = lower.find(':')
            {
                let value = lower[colon_idx + 1..]
                    .trim_matches('\'')
                    .trim_matches('"')
                    .trim();
                if value == "runas" {
                    return Some("Start-Process -Verb RunAs (elevation)".to_string());
                }
            }
        }

        // Vector 2: Nested PowerShell in args
        for arg in &cmd.args {
            let stripped = arg.trim_matches('\'').trim_matches('"');
            if is_powershell_executable(stripped) {
                return Some(format!("Start-Process with nested PowerShell: {stripped}"));
            }
        }
    }
    None
}

/// 12. Script block with dangerous cmdlets
fn check_script_block_injection(
    commands: &[&ParsedCommandElement],
    flags: &SecurityFlags,
) -> Option<String> {
    if !flags.has_script_blocks {
        return None;
    }

    let safe = safe_script_block_cmdlets();
    let dangerous = dangerous_script_block_cmdlets();

    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();

        // 检查是否有 ScriptBlock 类型的元素
        let has_sb = cmd.element_types.as_ref().is_some_and(|ets| {
            ets.iter()
                .any(|et| matches!(et, CommandElementType::ScriptBlock))
        });

        if !has_sb {
            continue;
        }

        // 危险 cmdlet
        if dangerous.contains(name.as_str()) {
            return Some(format!("Script block with dangerous cmdlet: {}", cmd.name));
        }

        // 非安全 cmdlet
        if !safe.contains(name.as_str()) {
            return Some(format!("Script block with non-safe cmdlet: {}", cmd.name));
        }
    }
    None
}

/// 13. Sub-expressions
fn check_sub_expressions(flags: &SecurityFlags) -> Option<String> {
    if flags.has_sub_expressions {
        Some("Sub-expression $() detected".to_string())
    } else {
        None
    }
}

/// 14. Expandable strings
fn check_expandable_strings(flags: &SecurityFlags) -> Option<String> {
    if flags.has_expandable_strings {
        Some("Expandable string with embedded expression detected".to_string())
    } else {
        None
    }
}

/// 15. Splatting
fn check_splatting(flags: &SecurityFlags) -> Option<String> {
    if flags.has_splatting {
        Some("Variable splatting (@var) detected".to_string())
    } else {
        None
    }
}

/// 16. Stop-parsing token
fn check_stop_parsing(flags: &SecurityFlags) -> Option<String> {
    if flags.has_stop_parsing {
        Some("Stop-parsing token (--%) detected".to_string())
    } else {
        None
    }
}

/// 17. .NET member invocations
fn check_member_invocations(flags: &SecurityFlags) -> Option<String> {
    if flags.has_member_invocations {
        Some(".NET member invocation detected".to_string())
    } else {
        None
    }
}

/// 18. Type literals outside CLM allowlist
fn check_type_literals(parsed: &ParsedPowerShellCommand) -> Option<String> {
    if let Some(ref literals) = parsed.type_literals {
        for t in literals {
            if !is_clm_allowed_type(t) {
                return Some(format!("Type literal [{t}] outside CLM allowlist"));
            }
        }
    }
    None
}

/// 19. Invoke-Item (`ShellExecute`)
fn check_invoke_item(commands: &[&ParsedCommandElement]) -> Option<String> {
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if name == "invoke-item" || name == "ii" {
            return Some("Invoke-Item detected (ShellExecute)".to_string());
        }
    }
    None
}

/// 20. Scheduled task creation (含 schtasks.exe)
fn check_scheduled_task(commands: &[&ParsedCommandElement]) -> Option<String> {
    let sched = scheduled_task_cmdlets();
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if sched.contains(name.as_str()) {
            return Some(format!("Scheduled task cmdlet: {}", cmd.name));
        }
        // schtasks.exe with /create or /change
        if name == "schtasks" || name == "schtasks.exe" {
            for arg in &cmd.args {
                let lower = arg.to_lowercase();
                if lower == "/create"
                    || lower == "/change"
                    || lower == "-create"
                    || lower == "-change"
                {
                    return Some(format!("schtasks with create/change: {}", cmd.name));
                }
            }
            // 即使没有 /create /change 参数，schtasks 本身也是可疑的
            return Some(format!("Scheduled task utility: {}", cmd.name));
        }
    }
    None
}

/// 21. Environment variable manipulation
fn check_env_var_manipulation(
    parsed: &ParsedPowerShellCommand,
    commands: &[&ParsedCommandElement],
) -> Option<String> {
    let env_cmdlets = env_write_cmdlets();
    let flags = parser::derive_security_flags(parsed);

    // 检查 env: 作用域变量
    let has_env_vars = parsed
        .statements
        .iter()
        .flat_map(|s| s.commands.iter())
        .any(|c| {
            c.args.iter().any(|a| {
                let lower = a.to_lowercase();
                lower.starts_with("env:") || lower.starts_with("$env:")
            })
        });

    if !has_env_vars {
        return None;
    }

    // 检查是否有写入 cmdlet
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if env_cmdlets.contains(name.as_str()) {
            return Some(format!("Environment variable manipulation: {}", cmd.name));
        }
    }

    // 检查赋值操作
    if flags.has_assignments {
        return Some("Environment variable assignment detected".to_string());
    }

    None
}

/// 22. Module loading
fn check_module_loading(commands: &[&ParsedCommandElement]) -> Option<String> {
    let mod_cmdlets = module_loading_cmdlets();
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if mod_cmdlets.contains(name.as_str()) {
            return Some(format!("Module loading/installation: {}", cmd.name));
        }
    }
    None
}

/// 23. Runtime state manipulation (alias/variable)
fn check_runtime_state_manipulation(commands: &[&ParsedCommandElement]) -> Option<String> {
    let state_cmdlets = runtime_state_cmdlets();
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if state_cmdlets.contains(name.as_str()) {
            return Some(format!("Runtime state manipulation: {}", cmd.name));
        }
    }
    None
}

/// 24. WMI/CIM process spawning
fn check_wmi_process_spawn(commands: &[&ParsedCommandElement]) -> Option<String> {
    let wmi = wmi_spawn_cmdlets();
    for cmd in commands {
        let name = strip_module_qualifier(&cmd.name).to_lowercase();
        if wmi.contains(name.as_str()) {
            return Some(format!("WMI/CIM process spawn: {}", cmd.name));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use acosmi_shell_parser::powershell::types::{
        CommandNameType, ParsedStatement, PipelineElementType, StatementType,
    };

    fn make_parsed_cmd_with_stmt(
        cmd_text: &str,
        commands: Vec<ParsedCommandElement>,
    ) -> ParsedPowerShellCommand {
        ParsedPowerShellCommand {
            valid: true,
            errors: vec![],
            statements: vec![ParsedStatement {
                statement_type: StatementType::PipelineAst,
                commands,
                redirections: vec![],
                text: cmd_text.to_string(),
                nested_commands: None,
                security_patterns: None,
            }],
            variables: vec![],
            has_stop_parsing: false,
            original_command: cmd_text.to_string(),
            type_literals: None,
            has_using_statements: None,
            has_script_requirements: None,
        }
    }

    fn make_cmd(name: &str, args: &[&str]) -> ParsedCommandElement {
        ParsedCommandElement {
            name: name.to_string(),
            name_type: CommandNameType::Unknown,
            element_type: PipelineElementType::CommandAst,
            args: args.iter().map(|s| s.to_string()).collect(),
            text: format!("{} {}", name, args.join(" ")),
            element_types: None,
            children: None,
            redirections: None,
        }
    }

    // ─── CLM Type Allowlist ───

    #[test]
    fn test_clm_allowed_types() {
        assert!(is_clm_allowed_type("string"));
        assert!(is_clm_allowed_type("String"));
        assert!(is_clm_allowed_type("System.String"));
        assert!(is_clm_allowed_type("int"));
        assert!(is_clm_allowed_type("hashtable"));
        assert!(is_clm_allowed_type("System.Collections.Hashtable"));
        assert!(is_clm_allowed_type("pscredential"));
        assert!(is_clm_allowed_type(
            "Microsoft.Management.Infrastructure.CimInstance"
        ));
        // 数组后缀
        assert!(is_clm_allowed_type("String[]"));
        assert!(is_clm_allowed_type("int[]"));
    }

    #[test]
    fn test_clm_blocked_types() {
        // SECURITY: 这些类型必须不在允许列表中
        assert!(!is_clm_allowed_type("adsi"));
        assert!(!is_clm_allowed_type("adsisearcher"));
        assert!(!is_clm_allowed_type("wmi"));
        assert!(!is_clm_allowed_type("wmiclass"));
        assert!(!is_clm_allowed_type("wmisearcher"));
        assert!(!is_clm_allowed_type("cimsession"));
        assert!(!is_clm_allowed_type(
            "System.DirectoryServices.DirectoryEntry"
        ));
    }

    #[test]
    fn test_normalize_type_name() {
        assert_eq!(normalize_type_name("String[]"), "string");
        assert_eq!(normalize_type_name("List[int]"), "list");
        assert_eq!(normalize_type_name("  Int32  "), "int32");
    }

    // ─── Parameter Abbreviation ───

    #[test]
    fn test_param_abbreviation() {
        let cmd = make_cmd("pwsh", &["-encodedcommand", "base64"]);
        assert!(ps_exe_has_param_abbreviation(&cmd, "-encodedcommand", "-e"));

        let cmd2 = make_cmd("pwsh", &["-e", "base64"]);
        assert!(ps_exe_has_param_abbreviation(
            &cmd2,
            "-encodedcommand",
            "-e"
        ));

        let cmd3 = make_cmd("pwsh", &["-enc", "base64"]);
        assert!(ps_exe_has_param_abbreviation(
            &cmd3,
            "-encodedcommand",
            "-e"
        ));
    }

    #[test]
    fn test_param_abbreviation_alternative_prefix() {
        // / 前缀
        let cmd = make_cmd("pwsh", &["/encodedcommand", "base64"]);
        assert!(ps_exe_has_param_abbreviation(&cmd, "-encodedcommand", "-e"));
    }

    // ─── PowerShell Executable Detection ───

    #[test]
    fn test_is_powershell_executable() {
        assert!(is_powershell_executable("pwsh"));
        assert!(is_powershell_executable("pwsh.exe"));
        assert!(is_powershell_executable("PowerShell"));
        assert!(is_powershell_executable(
            "C:\\Windows\\System32\\powershell.exe"
        ));
        assert!(is_powershell_executable("/usr/bin/pwsh"));
        assert!(!is_powershell_executable("Get-Process"));
    }

    // ─── Module Qualifier Stripping ───

    #[test]
    fn test_strip_module_qualifier() {
        assert_eq!(
            strip_module_qualifier("Microsoft.PowerShell.Utility\\Set-Alias"),
            "Set-Alias"
        );
        assert_eq!(strip_module_qualifier("Get-Process"), "Get-Process");
    }

    // ─── Validator Tests ───

    #[test]
    fn test_invoke_expression() {
        let cmd = make_cmd("Invoke-Expression", &["'malicious code'"]);
        assert!(check_invoke_expression(&[&cmd]).is_some());
    }

    #[test]
    fn test_iex_alias() {
        let cmd = make_cmd("iex", &["$code"]);
        assert!(check_invoke_expression(&[&cmd]).is_some());
    }

    #[test]
    fn test_safe_command() {
        let cmd = make_cmd("Get-ChildItem", &["-Path", "."]);
        assert!(check_invoke_expression(&[&cmd]).is_none());
        assert!(check_add_type(&[&cmd]).is_none());
        assert!(check_com_object(&[&cmd]).is_none());
    }

    #[test]
    fn test_encoded_command() {
        let cmd = make_cmd("pwsh", &["-encodedcommand", "base64data"]);
        assert!(check_encoded_command(&[&cmd]).is_some());
    }

    #[test]
    fn test_encoded_command_abbreviation() {
        let cmd = make_cmd("pwsh", &["-e", "base64data"]);
        assert!(check_encoded_command(&[&cmd]).is_some());
    }

    #[test]
    fn test_add_type() {
        let cmd = make_cmd("Add-Type", &["-TypeDefinition", "code"]);
        assert!(check_add_type(&[&cmd]).is_some());
    }

    #[test]
    fn test_com_object() {
        let cmd = make_cmd("New-Object", &["-ComObject", "Shell.Application"]);
        assert!(check_com_object(&[&cmd]).is_some());
    }

    #[test]
    fn test_invoke_item() {
        let cmd = make_cmd("ii", &["."]);
        assert!(check_invoke_item(&[&cmd]).is_some());
    }

    #[test]
    fn test_wmi_spawn() {
        let cmd = make_cmd(
            "Invoke-WmiMethod",
            &["-Class", "Win32_Process", "-Name", "Create"],
        );
        assert!(check_wmi_process_spawn(&[&cmd]).is_some());
    }

    #[test]
    fn test_module_loading() {
        let cmd = make_cmd("Import-Module", &["SomeModule"]);
        assert!(check_module_loading(&[&cmd]).is_some());
    }

    #[test]
    fn test_security_flags() {
        let flags = SecurityFlags {
            has_sub_expressions: true,
            ..SecurityFlags::default()
        };
        assert!(check_sub_expressions(&flags).is_some());
        assert!(check_expandable_strings(&flags).is_none());
    }

    #[test]
    fn test_start_process_elevation() {
        let cmd = make_cmd("Start-Process", &["-Verb", "RunAs", "cmd"]);
        assert!(check_start_process(&[&cmd]).is_some());
    }

    #[test]
    fn test_start_process_colon_bind() {
        let cmd = make_cmd("Start-Process", &["-Verb:RunAs", "cmd"]);
        assert!(check_start_process(&[&cmd]).is_some());
    }

    #[test]
    fn test_env_var_manipulation() {
        let cmd = make_cmd("Set-Item", &["env:PATH", "malicious"]);
        assert!(
            check_env_var_manipulation(
                &make_parsed_cmd_with_stmt("Set-Item env:PATH malicious", vec![cmd.clone()]),
                &[&cmd],
            )
            .is_some()
        );
    }

    #[test]
    fn test_env_var_safe() {
        let cmd = make_cmd("Set-Item", &["variable:x", "safe"]);
        assert!(
            check_env_var_manipulation(
                &make_parsed_cmd_with_stmt("Set-Item variable:x safe", vec![cmd.clone()]),
                &[&cmd],
            )
            .is_none()
        );
    }

    #[test]
    fn test_bare_pwsh_detection() {
        // 裸 pwsh 调用也应该被检测
        let cmd = make_cmd("pwsh", &[]);
        assert!(check_pwsh_command_or_file(&[&cmd]).is_some());
    }

    #[test]
    fn test_schtasks_create() {
        let cmd = make_cmd("schtasks", &["/create", "/tn", "MyTask"]);
        assert!(check_scheduled_task(&[&cmd]).is_some());
    }

    #[test]
    fn test_type_literals_clm() {
        let mut parsed =
            make_parsed_cmd_with_stmt("[System.IO.File]::ReadAllText('secret')", vec![]);
        parsed.type_literals = Some(vec!["System.IO.File".to_string()]);
        assert!(check_type_literals(&parsed).is_some());

        // 允许的类型
        let mut parsed2 = make_parsed_cmd_with_stmt("[int]$x", vec![]);
        parsed2.type_literals = Some(vec!["int".to_string()]);
        assert!(check_type_literals(&parsed2).is_none());
    }

    // ─── HIGH-ps-1 回归测试（2026-05-05 audit）─────────────────────────────
    // extract_new_object_type_name 早期把 -Strict (switch) 与 -ArgumentList (value)
    // 混入同一 skip_params 集合统一 i+=2，吃掉真 TypeName，CLM 白名单整段跳过。
    // 修复后 switch / value / colon-bound 三类分流。

    #[test]
    fn test_new_object_strict_switch_does_not_swallow_typename() {
        // -Strict 是 switch (无值), 不能多跳一格吃掉 TypeName
        let cmd = make_cmd("New-Object", &["-Strict", "System.IO.FileStream"]);
        assert!(
            check_com_object(&[&cmd]).is_some(),
            "New-Object -Strict System.IO.FileStream 必须触发 CLM 违规"
        );
    }

    #[test]
    fn test_new_object_argumentlist_value_param_consumes_value() {
        // 控制: -ArgumentList 是 value-param, 应正确跳 2 格找到 TypeName
        let cmd = make_cmd(
            "New-Object",
            &["-ArgumentList", "@()", "System.IO.FileStream"],
        );
        assert!(check_com_object(&[&cmd]).is_some());
    }

    #[test]
    fn test_new_object_property_colon_bound_does_not_swallow_typename() {
        // colon-bound `-Property:@{...}` 是单 token, 不应消费下个 token
        let cmd = make_cmd(
            "New-Object",
            &["-Property:@{Name='x'}", "System.IO.FileStream"],
        );
        assert!(check_com_object(&[&cmd]).is_some());
    }

    #[test]
    fn test_new_object_strict_with_clm_allowed_type_passes() {
        // 控制: -Strict + 白名单类型 (string) 不应触发违规
        let cmd = make_cmd("New-Object", &["-Strict", "string"]);
        assert!(check_com_object(&[&cmd]).is_none());
    }
}
