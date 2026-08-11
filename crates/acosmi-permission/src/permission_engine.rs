//! 权限决策引擎
//!
//! 等价于 TS bashPermissions.ts / powershellPermissions.ts 中的主决策逻辑
//! 负责：规则匹配 → 安全扫描 → 只读检查 → 最终决策

use crate::constants;
use crate::path_extraction;
use crate::path_policy;
use crate::readonly_validator;
use crate::rule_matching::{matches_rule, parse_shell_rule};
use crate::security_scanner::{self, BashSecurityResult};
use crate::types::{
    FileOperationType, PermissionBehavior, PermissionContext, PermissionDecisionReason,
    PermissionMode, PermissionResult, PermissionRule, PermissionRuleSource, PermissionRuleValue,
    PermissionUpdate,
};
use acosmi_shell_parser::bash::ast::parse_for_security;
use acosmi_shell_parser::{ParseForSecurityResult, RedirectOp, SimpleCommand};
use std::sync::OnceLock;

/// 安全检查子命令上限
pub const MAX_SUBCOMMANDS_FOR_SECURITY_CHECK: usize = 50;

/// Additional commands that delegate execution in a way this path floor
/// cannot inspect as one argv vector. Shells and standard wrappers come from
/// `constants::bare_shell_prefixes`; this list only contains non-shell
/// dispatch builtins and multi-call binaries.
const ADDITIONAL_UNINSPECTABLE_EXECUTION_COMMANDS: &[&str] =
    &["command", "builtin", "exec", "busybox", "toybox"];

/// General-purpose interpreters can perform arbitrary filesystem mutations
/// without exposing target paths in argv. Bypass mode is an approval policy,
/// not a process sandbox, so direct interpreter execution remains a sensitive
/// confirmation boundary. Pure version/help queries are carved out below.
const SENSITIVE_INTERPRETER_COMMANDS: &[&str] = &[
    "node",
    "nodejs",
    "bun",
    "deno",
    "pythonw",
    "pypy",
    "pypy3",
    "perl",
    "ruby",
    "php",
    "lua",
    "luajit",
    "osascript",
];

/// Bash 命令权限检查
///
/// 流程: deny/ask/allow 规则 → 安全扫描 → 只读检查 → Ask
#[must_use]
pub fn check_bash_permission(command: &str, ctx: &PermissionContext) -> PermissionResult {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return PermissionResult::deny(
            "Empty command",
            PermissionDecisionReason::Other {
                reason: "empty command".to_string(),
            },
        );
    }

    // 1. DontAsk 模式直接拒绝。BypassPermissions 不能在这里提前返回：
    // 显式 deny/ask 规则和安全扫描是所有“免常规审批”模式共享的安全底线。
    if ctx.mode == PermissionMode::DontAsk {
        return PermissionResult::deny(
            "Permission denied (DontAsk mode)",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::DontAsk,
            },
        );
    }

    // 2. 规则匹配（deny → ask → allow）。Allow 先记下，必须经过下面
    // security + path floor 后才能生效。
    let mut matching_allow_rule = None;
    if let Some((behavior, rule)) = match_command_against_rules(trimmed, &ctx.rules, "Bash") {
        match behavior {
            PermissionBehavior::Deny => {
                return PermissionResult::deny(
                    format!("Denied by rule: {:?}", rule.value),
                    PermissionDecisionReason::Rule { rule: rule.clone() },
                );
            }
            PermissionBehavior::Allow => {
                matching_allow_rule = Some(rule);
            }
            PermissionBehavior::Ask => {
                return PermissionResult::Ask {
                    message: format!("Approval required by rule: {:?}", rule.value),
                    suggestions: vec![],
                    blocked_path: None,
                };
            }
        }
    }

    // 3. 安全扫描
    let scanner_result = security_scanner::bash_security_check(trimmed);
    match &scanner_result {
        BashSecurityResult::Allow | BashSecurityResult::Passthrough => {}
        BashSecurityResult::Ask(violation) => {
            return PermissionResult::Ask {
                message: format!("Security check: {}", violation.message),
                suggestions: build_bash_suggestions(trimmed),
                blocked_path: None,
            };
        }
    }

    // 4. AST 路径安全下限。危险删除、受保护配置写入、不可安全解析的
    // 复合执行必须在显式 allow、scanner early-allow 和 bypass 之前处理。
    if let Some(result) = check_bash_path_safety_floor(trimmed, ctx) {
        return result;
    }

    if let Some(rule) = matching_allow_rule {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::Rule {
            rule: rule.clone(),
        });
    }

    if matches!(scanner_result, BashSecurityResult::Allow) {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::SafetyCheck {
            reason: "Safe command (early allow)".to_string(),
            classifier_approvable: true,
        });
    }

    // 5. BypassPermissions 只跳过下面的常规逐次审批，不能跳过上面的
    // 显式规则、语法安全扫描和路径安全下限。
    if ctx.mode == PermissionMode::BypassPermissions {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::Mode {
            mode: PermissionMode::BypassPermissions,
        });
    }

    // 6. 只读命令检查
    if readonly_validator::is_command_readonly(trimmed) {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::SafetyCheck {
            reason: "Command is readonly".to_string(),
            classifier_approvable: true,
        });
    }

    // 7. Plan 模式 — 不执行
    if ctx.mode == PermissionMode::Plan {
        return PermissionResult::deny(
            "Plan mode: command would be executed",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::Plan,
            },
        );
    }

    // 8. 默认 Ask
    PermissionResult::Ask {
        message: format!("Allow bash command: {trimmed}"),
        suggestions: build_bash_suggestions(trimmed),
        blocked_path: None,
    }
}

/// Parse a Bash command into atomic argv records and enforce the permission
/// floor that must survive explicit allow rules and bypass mode.
fn check_bash_path_safety_floor(
    command: &str,
    ctx: &PermissionContext,
) -> Option<PermissionResult> {
    let commands = match parse_for_security(command) {
        ParseForSecurityResult::Simple { commands } => commands,
        ParseForSecurityResult::TooComplex { reason, .. } => {
            return Some(PermissionResult::ask(format!(
                "Command requires explicit approval because path safety analysis was inconclusive: {reason}"
            )));
        }
        ParseForSecurityResult::ParseUnavailable => {
            return Some(PermissionResult::ask(
                "Command requires explicit approval because the shell parser could not analyze it",
            ));
        }
    };

    if commands.len() > MAX_SUBCOMMANDS_FOR_SECURITY_CHECK {
        return Some(PermissionResult::ask(format!(
            "Command contains too many subcommands for safe path analysis ({} > {MAX_SUBCOMMANDS_FOR_SECURITY_CHECK})",
            commands.len()
        )));
    }

    // Environment assignments are represented separately from argv by the
    // shell AST. Inspect them before wrapper normalization so `NAME=value cmd`
    // and `env NAME=value cmd` cannot disappear when the effective command is
    // selected below. These variables can replace the executable, inject a
    // loader/config/helper, or cause Git to launch an attacker-selected tool.
    for simple in &commands {
        if let Some(name) = sensitive_execution_environment_override(simple) {
            return Some(PermissionResult::ask(format!(
                "Execution environment override '{name}' requires explicit approval"
            )));
        }
    }

    let normalized: Result<Vec<&[String]>, &str> = commands
        .iter()
        .map(|simple| strip_safe_wrappers_from_argv(&simple.argv))
        .collect();
    let normalized = match normalized {
        Ok(value) => value,
        Err(reason) => {
            return Some(PermissionResult::ask(format!(
                "Command wrapper requires explicit approval: {reason}"
            )));
        }
    };
    let compound_has_cd = normalized.iter().any(|argv| {
        argv.first()
            .is_some_and(|name| command_basename(name) == "cd")
    });

    if compound_has_cd
        && normalized.iter().any(|argv| {
            argv.first()
                .is_some_and(|name| command_basename(name) != "cd")
                && !argv_is_readonly(argv)
        })
    {
        return Some(PermissionResult::ask(
            "A directory-changing compound command with a non-readonly step requires explicit approval because write targets cannot be resolved against one cwd",
        ));
    }

    for (simple, argv) in commands.iter().zip(normalized) {
        let Some(raw_name) = argv.first() else {
            continue;
        };
        let command_name = command_basename(raw_name);
        let args = &argv[1..];

        if is_uninspectable_execution_command(&command_name) {
            return Some(PermissionResult::ask(format!(
                "Nested or privileged command execution via '{command_name}' requires explicit approval"
            )));
        }

        if is_sensitive_interpreter_invocation(&command_name, args) {
            return Some(PermissionResult::ask(format!(
                "Interpreter execution via '{command_name}' requires explicit approval because filesystem targets are not inspectable from argv"
            )));
        }

        if has_uninspectable_delegated_execution(&command_name, args) {
            return Some(PermissionResult::ask(format!(
                "Command '{command_name}' requires explicit approval because an option delegates execution to another local program"
            )));
        }

        if command_name == "find"
            && args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
                ) || arg.starts_with("-exec")
                    || arg.starts_with("-ok")
            })
        {
            return Some(PermissionResult::ask(
                "find with deletion or command execution requires explicit approval",
            ));
        }

        if let Some(result) = validate_output_redirects(simple, &ctx.cwd, compound_has_cd) {
            return Some(result);
        }

        if let Some(result) = check_special_mutator_safety(&command_name, args, simple, &ctx.cwd) {
            return Some(result);
        }

        if !argv_is_readonly(argv)
            && !uses_special_path_analysis(&command_name, args)
            && let Some(path) = find_sensitive_argv_path(&command_name, args, &ctx.cwd)
        {
            return Some(PermissionResult::Ask {
                message: format!(
                    "Command '{command_name}' requires explicit approval because argv names protected config-home path '{path}'"
                ),
                suggestions: vec![],
                blocked_path: Some(path.to_string()),
            });
        }

        if !path_extraction::is_supported_path_command(&command_name) {
            continue;
        }

        // mv/cp flags such as --target-directory carry paths that the common
        // positional extractor intentionally cannot infer. Mirror the TS
        // command validator and fail closed instead of silently skipping one.
        if matches!(command_name.as_str(), "mv" | "cp") && has_pre_double_dash_flag(args) {
            return Some(PermissionResult::ask(format!(
                "{command_name} with flags requires explicit approval because its target path cannot be inferred safely"
            )));
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let Some(info) = path_extraction::extract_paths(&command_name, &arg_refs) else {
            continue;
        };
        if matches!(info.operation, FileOperationType::Read) {
            continue;
        }
        if compound_has_cd {
            return Some(PermissionResult::ask(
                "Commands that change directory and then write require explicit approval",
            ));
        }

        for path in info.paths {
            let result = path_policy::validate_path(&path, &ctx.cwd, info.operation);
            if !result.allowed {
                return Some(path_ask_result(
                    &command_name,
                    &path,
                    result.reason.as_ref(),
                ));
            }
        }
    }

    None
}

fn validate_output_redirects(
    command: &SimpleCommand,
    cwd: &str,
    compound_has_cd: bool,
) -> Option<PermissionResult> {
    for redirect in &command.redirects {
        if !matches!(
            redirect.op,
            RedirectOp::Write
                | RedirectOp::Append
                | RedirectOp::Clobber
                | RedirectOp::WriteAll
                | RedirectOp::AppendAll
        ) {
            continue;
        }
        if redirect.target == "/dev/null" {
            continue;
        }
        if compound_has_cd {
            return Some(PermissionResult::ask(
                "Commands that change directory and write via redirection require explicit approval",
            ));
        }
        let result = path_policy::validate_path(&redirect.target, cwd, FileOperationType::Create);
        if !result.allowed {
            return Some(path_ask_result(
                "output redirection",
                &redirect.target,
                result.reason.as_ref(),
            ));
        }
    }
    None
}

fn path_ask_result(
    operation: &str,
    path: &str,
    reason: Option<&PermissionDecisionReason>,
) -> PermissionResult {
    let reason = match reason {
        Some(
            PermissionDecisionReason::SafetyCheck { reason, .. }
            | PermissionDecisionReason::Other { reason },
        ) => reason.clone(),
        Some(PermissionDecisionReason::Mode { mode }) => format!("blocked by mode {mode:?}"),
        Some(PermissionDecisionReason::Rule { rule }) => {
            format!("blocked by path rule {:?}", rule.value)
        }
        None => "path is outside the allowed working directory".to_string(),
    };
    PermissionResult::Ask {
        message: format!("{operation} requires explicit approval for path '{path}': {reason}"),
        suggestions: vec![],
        blocked_path: Some(path.to_string()),
    }
}

fn command_basename(command: &str) -> String {
    let lowercase = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    lowercase
        .strip_suffix(".exe")
        .unwrap_or(&lowercase)
        .to_string()
}

fn is_uninspectable_execution_command(command_name: &str) -> bool {
    constants::bare_shell_prefixes().contains(command_name)
        || ADDITIONAL_UNINSPECTABLE_EXECUTION_COMMANDS.contains(&command_name)
}

fn is_sensitive_interpreter_invocation(command_name: &str, args: &[String]) -> bool {
    let is_python = command_name == "python"
        || command_name.strip_prefix("python").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                && suffix.chars().any(|ch| ch.is_ascii_digit())
        });
    if !is_python && !SENSITIVE_INTERPRETER_COMMANDS.contains(&command_name) {
        return false;
    }

    !matches!(
        args,
        [flag] if matches!(flag.as_str(), "--version" | "-V" | "-VV" | "--help" | "-h")
    )
}

fn has_pre_double_dash_flag(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg.starts_with('-'))
}

fn argv_is_readonly(argv: &[String]) -> bool {
    readonly_validator::is_command_readonly(&argv.join(" "))
}

/// Environment names that can change executable identity or delegate code
/// loading. This is deliberately a closed, case-insensitive safety set: an
/// explicit Allow rule and BypassPermissions both remain below this floor.
fn is_sensitive_execution_environment_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "PATHEXT"
            | "COMSPEC"
            | "HOME"
            | "XDG_CONFIG_HOME"
            | "BASH_ENV"
            | "ENV"
            | "PROMPT_COMMAND"
            | "SHELL"
            | "EDITOR"
            | "VISUAL"
            | "PAGER"
            | "MANPAGER"
            | "LESSOPEN"
            | "LESSCLOSE"
            | "GIT_EXEC_PATH"
            | "GIT_PAGER"
            | "GIT_EXTERNAL_DIFF"
            | "GIT_SSH"
            | "GIT_TEMPLATE_DIR"
            | "GIT_DIR"
            | "GIT_WORK_TREE"
            | "GIT_COMMON_DIR"
            | "GIT_OBJECT_DIRECTORY"
            | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
            | "GIT_INDEX_FILE"
            | "GIT_CONFIG"
            | "GIT_CEILING_DIRECTORIES"
            | "GIT_DISCOVERY_ACROSS_FILESYSTEM"
            | "GIT_NAMESPACE"
            | "GIT_SHALLOW_FILE"
            | "GIT_REPLACE_REF_BASE"
            | "GIT_NO_REPLACE_OBJECTS"
            | "GIT_LITERAL_PATHSPECS"
            | "GIT_GLOB_PATHSPECS"
            | "GIT_NOGLOB_PATHSPECS"
            | "GIT_ICASE_PATHSPECS"
    ) || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.ends_with("ASKPASS")
        || upper.starts_with("GIT_CONFIG_")
        || upper.starts_with("GIT_TRACE")
        || upper.starts_with("GIT_") && upper.ends_with("_COMMAND")
        || upper.starts_with("GIT_") && upper.ends_with("_EDITOR")
}

fn sensitive_execution_environment_override(command: &SimpleCommand) -> Option<&str> {
    if let Some(assignment) = command
        .env_vars
        .iter()
        .find(|assignment| is_sensitive_execution_environment_name(&assignment.name))
    {
        return Some(&assignment.name);
    }

    let mut argv = command.argv.as_slice();
    loop {
        let raw_name = argv.first()?;
        let wrapper_name = command_basename(raw_name);
        if wrapper_name == "env"
            && let Some(name) = sensitive_env_wrapper_override(argv)
        {
            return Some(name);
        }
        let command_index = safe_wrapper_command_index(argv).ok().flatten()?;
        argv = &argv[command_index..];
    }
}

/// Inspect the assignment/unset portion of a syntactically accepted `env`
/// wrapper. Unknown `env` grammar is handled separately by wrapper
/// normalization and already fails closed.
fn sensitive_env_wrapper_override(argv: &[String]) -> Option<&str> {
    let command_index = env_wrapped_command_index(argv)?;
    let mut index = 1;
    while index < command_index {
        let arg = argv.get(index)?.as_str();
        if arg == "-i" {
            // Clearing the complete environment also clears PATH/HOME and all
            // loader/config guards, so executable/config identity changes even
            // without an explicit assignment token.
            return Some(arg);
        }
        if matches!(arg, "-0" | "-v") || arg == "--" {
            index += 1;
            continue;
        }
        if arg == "-u" {
            let name = argv.get(index + 1)?.as_str();
            if is_sensitive_execution_environment_name(name) {
                return Some(name);
            }
            index += 2;
            continue;
        }
        if !arg.starts_with('-')
            && let Some((name, _)) = arg.split_once('=')
            && is_sensitive_execution_environment_name(name)
        {
            return Some(name);
        }
        index += 1;
    }
    None
}

fn has_uninspectable_delegated_execution(command_name: &str, args: &[String]) -> bool {
    matches!(command_name, "rg" | "ripgrep")
        && args.iter().any(|arg| {
            matches!(arg.as_str(), "--pre" | "--hostname-bin")
                || arg.starts_with("--pre=")
                || arg.starts_with("--hostname-bin=")
        })
}

fn uses_special_path_analysis(command_name: &str, args: &[String]) -> bool {
    matches!(
        command_name,
        "tee"
            | "install"
            | "rsync"
            | "dd"
            | "truncate"
            | "ln"
            | "tar"
            | "unzip"
            | "curl"
            | "wget"
            | "scp"
            | "awk"
            | "gawk"
            | "mawk"
            | "nawk"
            | "sed"
    ) || command_name == "git" && git_requires_special_path_analysis(args)
}

fn find_sensitive_argv_path<'a>(
    command_name: &str,
    args: &'a [String],
    cwd: &str,
) -> Option<&'a str> {
    for (index, arg) in args.iter().enumerate() {
        if is_literal_argv_value(command_name, args, index) {
            continue;
        }
        let candidate = if let Some((_, value)) = arg.split_once('=') {
            if arg.starts_with('-') {
                value
            } else {
                arg.as_str()
            }
        } else if arg.starts_with('-') && arg != "-" {
            continue;
        } else {
            arg.as_str()
        };
        if candidate == "-"
            || looks_like_url(candidate) && !url_like_candidate_has_local_prefix(candidate, cwd)
        {
            continue;
        }
        if path_policy::is_sensitive_config_write_target(candidate, cwd) {
            return Some(candidate);
        }
    }
    None
}

fn is_literal_argv_value(command_name: &str, args: &[String], index: usize) -> bool {
    let current = args.get(index).map(String::as_str).unwrap_or_default();
    const GIT_LITERAL_EQUALS_PREFIXES: &[&str] = &[
        "--message=",
        "--author=",
        "--date=",
        "--format=",
        "--pretty=",
        "--grep=",
    ];
    if command_name == "git"
        && GIT_LITERAL_EQUALS_PREFIXES
            .iter()
            .any(|prefix| current.starts_with(prefix))
    {
        return true;
    }
    let Some(previous) = index.checked_sub(1).and_then(|i| args.get(i)) else {
        return false;
    };
    match command_name {
        "git" => {
            matches!(
                previous.as_str(),
                "-m" | "--message" | "--author" | "--date" | "--format" | "--pretty" | "--grep"
            ) || GIT_LITERAL_EQUALS_PREFIXES.contains(&previous.as_str())
        }
        "grep" | "rg" => matches!(previous.as_str(), "-e" | "--regexp"),
        "sed" => matches!(previous.as_str(), "-e" | "--expression"),
        _ => false,
    }
}

fn looks_like_url(value: &str) -> bool {
    value.contains("://")
}

fn url_like_candidate_has_local_prefix(value: &str, cwd: &str) -> bool {
    let Some((scheme, _)) = value.split_once("://") else {
        return false;
    };
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
    {
        return false;
    }
    std::path::Path::new(cwd)
        .join(format!("{scheme}:"))
        .symlink_metadata()
        .is_ok()
}

fn looks_like_remote_spec(value: &str) -> bool {
    if value.starts_with("git@") {
        return true;
    }
    let Some((prefix, _)) = value.split_once(':') else {
        return false;
    };
    let windows_drive = prefix.len() == 1
        && prefix
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    !windows_drive && !prefix.contains(['/', '\\'])
}

fn check_special_mutator_safety(
    command_name: &str,
    args: &[String],
    simple: &SimpleCommand,
    cwd: &str,
) -> Option<PermissionResult> {
    if args.len() == 1
        && matches!(
            args[0].as_str(),
            "-h" | "--help" | "-V" | "-v" | "--version"
        )
    {
        return None;
    }
    match command_name {
        "tee" => check_tee_targets(args, cwd),
        "install" => Some(inconclusive_mutator_ask(
            "install",
            "install has multiple target modes and ownership/permission side effects",
        )),
        "rsync" => check_rsync_invocation(args, cwd),
        "dd" => args
            .iter()
            .enumerate()
            .find_map(|(index, arg)| {
                arg.strip_prefix("of=").and_then(|value| {
                    if value.is_empty() {
                        args.get(index + 1).cloned()
                    } else {
                        Some(value.to_string())
                    }
                })
            })
            .or_else(|| {
                simple
                    .env_vars
                    .iter()
                    .find(|assignment| assignment.name == "of")
                    .map(|assignment| assignment.value.clone())
            })
            .and_then(|path| validate_mutation_target("dd output", &path, cwd)),
        "truncate" => check_truncate_targets(args, cwd),
        "ln" => check_ln_target(args, cwd),
        "tar" => check_tar_invocation(args),
        "unzip" => check_unzip_invocation(args),
        "curl" => check_curl_outputs(args, cwd),
        "wget" => check_wget_output(args, cwd),
        "scp" => check_scp_invocation(args, cwd),
        "awk" | "gawk" | "mawk" | "nawk" => check_awk_program(command_name, args),
        "sed" => check_sed_program(args),
        "git" => check_git_mutator(args, cwd),
        _ => None,
    }
}

fn validate_mutation_target(operation: &str, path: &str, cwd: &str) -> Option<PermissionResult> {
    if path == "-" {
        return None;
    }
    let result = path_policy::validate_path(path, cwd, FileOperationType::Create);
    (!result.allowed).then(|| path_ask_result(operation, path, result.reason.as_ref()))
}

fn inconclusive_mutator_ask(command: &str, reason: &str) -> PermissionResult {
    PermissionResult::ask(format!(
        "{command} requires explicit approval because {reason}"
    ))
}

fn check_tee_targets(args: &[String], cwd: &str) -> Option<PermissionResult> {
    let mut past_double_dash = false;
    for arg in args {
        if arg == "--" {
            past_double_dash = true;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') && arg != "-" {
            if matches!(
                arg.as_str(),
                "-a" | "--append" | "-i" | "--ignore-interrupts" | "-p"
            ) || arg.starts_with("--output-error")
            {
                continue;
            }
            return Some(inconclusive_mutator_ask(
                "tee",
                "its option grammar was not recognized",
            ));
        }
        if let Some(result) = validate_mutation_target("tee output", arg, cwd) {
            return Some(result);
        }
    }
    None
}

fn validate_transfer_destination(
    command: &str,
    positionals: &[&str],
    cwd: &str,
) -> Option<PermissionResult> {
    if positionals.len() < 2 {
        return Some(inconclusive_mutator_ask(
            command,
            "a source and destination could not both be identified",
        ));
    }
    let destination = positionals.last().copied().unwrap_or_default();
    if looks_like_remote_spec(destination) {
        return None;
    }
    validate_mutation_target(&format!("{command} destination"), destination, cwd)
}

fn check_rsync_invocation(args: &[String], cwd: &str) -> Option<PermissionResult> {
    const SAFE_BOOLEAN_LONG: &[&str] = &[
        "--archive",
        "--recursive",
        "--verbose",
        "--quiet",
        "--compress",
        "--dry-run",
        "--checksum",
        "--update",
        "--existing",
        "--ignore-existing",
        "--partial",
        "--progress",
        "--human-readable",
        "--itemize-changes",
        "--stats",
        "--whole-file",
        "--sparse",
    ];
    const SAFE_VALUE_LONG: &[&str] = &[
        "--timeout",
        "--contimeout",
        "--bwlimit",
        "--max-size",
        "--min-size",
        "--chmod",
        "--exclude",
        "--include",
        "--port",
    ];

    let mut positionals = Vec::new();
    let mut index = 0;
    let mut past_double_dash = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            past_double_dash = true;
            index += 1;
            continue;
        }
        if !past_double_dash && SAFE_BOOLEAN_LONG.contains(&arg) {
            index += 1;
            continue;
        }
        if !past_double_dash && SAFE_VALUE_LONG.contains(&arg) {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    "rsync",
                    "a recognized option value is missing",
                ));
            }
            index += 2;
            continue;
        }
        if !past_double_dash
            && SAFE_VALUE_LONG.iter().any(|option| {
                arg.strip_prefix(option)
                    .is_some_and(|rest| rest.starts_with('='))
            })
        {
            index += 1;
            continue;
        }
        if !past_double_dash
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() > 1
            && arg.chars().skip(1).all(|ch| {
                matches!(
                    ch,
                    'a' | 'r' | 'v' | 'q' | 'z' | 'n' | 'c' | 'u' | 'i' | 'h' | 'P'
                )
            })
        {
            index += 1;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "rsync",
                "its option grammar is not in the closed safe allowlist (options may hide local paths, hard links, logs, filters, or delegated execution)",
            ));
        }
        positionals.push(arg);
        index += 1;
    }
    validate_transfer_destination("rsync", &positionals, cwd)
}

fn check_scp_invocation(args: &[String], cwd: &str) -> Option<PermissionResult> {
    let mut positionals = Vec::new();
    let mut index = 0;
    let mut past_double_dash = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            past_double_dash = true;
            index += 1;
            continue;
        }
        if !past_double_dash && matches!(arg, "-P" | "-l") {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    "scp",
                    "a recognized option value is missing",
                ));
            }
            index += 2;
            continue;
        }
        if !past_double_dash
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() > 1
            && arg
                .chars()
                .skip(1)
                .all(|ch| matches!(ch, '3' | '4' | '6' | 'B' | 'C' | 'p' | 'q' | 'r' | 'v'))
        {
            index += 1;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "scp",
                "its option grammar is not in the closed safe allowlist (options may select a local executable, proxy command, jump host, or config file)",
            ));
        }
        positionals.push(arg);
        index += 1;
    }
    validate_transfer_destination("scp", &positionals, cwd)
}

fn check_truncate_targets(args: &[String], cwd: &str) -> Option<PermissionResult> {
    let mut index = 0;
    let mut past_double_dash = false;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            past_double_dash = true;
            index += 1;
            continue;
        }
        if !past_double_dash && matches!(arg.as_str(), "-s" | "--size" | "-r" | "--reference") {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    "truncate",
                    "an option value is missing",
                ));
            }
            index += 2;
            continue;
        }
        if !past_double_dash
            && (arg.starts_with("--size=")
                || arg.starts_with("--reference=")
                || matches!(arg.as_str(), "-c" | "--no-create" | "-o" | "--io-blocks"))
        {
            index += 1;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "truncate",
                "its option grammar was not recognized",
            ));
        }
        if let Some(result) = validate_mutation_target("truncate target", arg, cwd) {
            return Some(result);
        }
        index += 1;
    }
    None
}

fn check_ln_target(args: &[String], cwd: &str) -> Option<PermissionResult> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut explicit_target_dir = None;
    let mut symbolic = false;
    let mut index = 0;
    let mut past_double_dash = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            past_double_dash = true;
            index += 1;
            continue;
        }
        if !past_double_dash && matches!(arg, "-t" | "--target-directory") {
            let Some(value) = args.get(index + 1) else {
                return Some(inconclusive_mutator_ask(
                    "ln",
                    "its target directory is missing",
                ));
            };
            explicit_target_dir = Some(value.as_str());
            index += 2;
            continue;
        }
        if !past_double_dash && let Some(value) = arg.strip_prefix("--target-directory=") {
            explicit_target_dir = Some(value);
            index += 1;
            continue;
        }
        if !past_double_dash
            && (matches!(
                arg,
                "-s" | "--symbolic"
                    | "-f"
                    | "--force"
                    | "-n"
                    | "--no-dereference"
                    | "-v"
                    | "--verbose"
                    | "-T"
                    | "--no-target-directory"
                    | "-r"
                    | "--relative"
            ) || arg.starts_with('-')
                && !arg.starts_with("--")
                && arg
                    .chars()
                    .skip(1)
                    .all(|ch| matches!(ch, 's' | 'f' | 'n' | 'v' | 'T' | 'r')))
        {
            if arg == "--symbolic"
                || arg == "-s"
                || arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.chars().skip(1).any(|ch| ch == 's')
            {
                symbolic = true;
            }
            index += 1;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "ln",
                "its option grammar was not recognized",
            ));
        }
        positionals.push(arg);
        index += 1;
    }
    if !symbolic {
        return Some(inconclusive_mutator_ask(
            "ln",
            "hard links can expose a protected config file through an otherwise safe inode alias",
        ));
    }
    if let Some(target) = explicit_target_dir {
        return validate_mutation_target("ln target directory", target, cwd);
    }
    let target = match positionals.as_slice() {
        [] => return None,
        [_source] => ".",
        _ => positionals.last().copied().unwrap_or("."),
    };
    validate_mutation_target("ln target", target, cwd)
}

fn check_tar_invocation(args: &[String]) -> Option<PermissionResult> {
    const SAFE_BOOLEAN_LONG: &[&str] = &[
        "--list",
        "--verbose",
        "--gzip",
        "--gunzip",
        "--ungzip",
        "--bzip2",
        "--xz",
        "--lzip",
        "--lzma",
        "--lzop",
        "--zstd",
        "--auto-compress",
        "--wildcards",
        "--no-wildcards",
        "--anchored",
        "--no-anchored",
        "--ignore-case",
        "--no-ignore-case",
        "--wildcards-match-slash",
        "--no-wildcards-match-slash",
        "--verbatim-files-from",
        "--null",
    ];
    const SAFE_VALUE_LONG: &[&str] = &["--file", "--exclude"];

    let mut saw_list = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            index += 1;
            continue;
        }
        if SAFE_BOOLEAN_LONG.contains(&arg) {
            saw_list |= arg == "--list";
            index += 1;
            continue;
        }
        if SAFE_VALUE_LONG.contains(&arg) {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    "tar",
                    "a recognized list option value is missing",
                ));
            }
            index += 2;
            continue;
        }
        if SAFE_VALUE_LONG.iter().any(|option| {
            arg.strip_prefix(option)
                .is_some_and(|rest| rest.starts_with('='))
        }) {
            index += 1;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let mut safe_cluster = true;
            for ch in arg.chars().skip(1) {
                if ch == 't' {
                    saw_list = true;
                } else if !matches!(ch, 'f' | 'v' | 'z' | 'j' | 'J' | 'a') {
                    safe_cluster = false;
                    break;
                }
            }
            if safe_cluster {
                index += 1;
                continue;
            }
        }
        if arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "tar",
                "only a closed list-only option set is safe; other flags may write files or execute checkpoint, decompressor, or member commands",
            ));
        }
        index += 1;
    }

    (!saw_list).then(|| {
        inconclusive_mutator_ask(
            "tar",
            "archive creation/extraction can write inferred members and symlinks",
        )
    })
}

fn check_unzip_invocation(args: &[String]) -> Option<PermissionResult> {
    let mut saw_readonly_action = false;
    for arg in args {
        if arg == "--" {
            continue;
        }
        if arg == "--help" {
            saw_readonly_action = true;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let mut safe_cluster = true;
            for ch in arg.chars().skip(1) {
                if matches!(ch, 'l' | 't' | 'p' | 'c' | 'Z' | 'v' | 'h') {
                    saw_readonly_action = true;
                } else if ch != 'q' {
                    safe_cluster = false;
                    break;
                }
            }
            if safe_cluster {
                continue;
            }
        }
        if arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "unzip",
                "only a closed listing, testing, or stdout option set is safe; other flags may extract or mutate the archive",
            ));
        }
    }
    (!saw_readonly_action).then(|| {
        inconclusive_mutator_ask(
            "unzip",
            "archive extraction writes member names that are not fully represented in argv",
        )
    })
}

fn check_curl_outputs(args: &[String], cwd: &str) -> Option<PermissionResult> {
    const WRITE_VALUE_OPTIONS: &[&str] = &[
        "-o",
        "--output",
        "--output-dir",
        "-D",
        "--dump-header",
        "-c",
        "--cookie-jar",
        "--trace",
        "--trace-ascii",
        "--stderr",
        "--etag-save",
        "--alt-svc",
        "--hsts",
        "--libcurl",
    ];
    const LONG_WRITE_OPTIONS: &[&str] = &[
        "--output",
        "--output-dir",
        "--dump-header",
        "--cookie-jar",
        "--trace",
        "--trace-ascii",
        "--stderr",
        "--etag-save",
        "--alt-svc",
        "--hsts",
        "--libcurl",
    ];

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        // curl's variable expansion layer is evaluated after argv parsing.
        // In particular, --expand-output and --expand-write-out can turn an
        // apparently harmless `{{name}}` operand into an arbitrary local
        // filename (including protected config-home state). Keep the whole
        // expansion family behind explicit approval: future expand-* options
        // must not silently inherit an automatic-write decision either.
        if arg == "--expand" || arg.starts_with("--expand-") {
            return Some(inconclusive_mutator_ask(
                "curl",
                "variable-expanded options can hide a local output filename or write-out redirection",
            ));
        }
        if matches!(arg, "-w" | "--write-out") {
            let Some(format) = args.get(index + 1) else {
                return Some(inconclusive_mutator_ask(
                    "curl",
                    "a write-out format is missing",
                ));
            };
            if format.starts_with('@') || format.contains("%output{") {
                return Some(inconclusive_mutator_ask(
                    "curl",
                    "the write-out DSL is external or can redirect output to a hidden local filename",
                ));
            }
            index += 2;
            continue;
        }
        if let Some(mut format) = arg.strip_prefix("--write-out=") {
            let consumed_next;
            if format.is_empty() {
                let Some(next) = args.get(index + 1) else {
                    return Some(inconclusive_mutator_ask(
                        "curl",
                        "a write-out format is missing",
                    ));
                };
                format = next;
                consumed_next = true;
            } else {
                consumed_next = false;
            }
            if format.starts_with('@') || format.contains("%output{") {
                return Some(inconclusive_mutator_ask(
                    "curl",
                    "the write-out DSL is external or can redirect output to a hidden local filename",
                ));
            }
            index += 1 + usize::from(consumed_next);
            continue;
        }
        if arg.starts_with("-O")
            || arg == "--remote-header-name"
            || arg.starts_with("--remote-name")
        {
            return Some(inconclusive_mutator_ask(
                "curl",
                "the local output filename is inferred from remote metadata",
            ));
        }
        if matches!(arg, "-K" | "--config")
            || arg.starts_with("--config=")
            || arg.starts_with("-K") && arg.len() > 2
        {
            return Some(inconclusive_mutator_ask(
                "curl",
                "a config file can hide local output directives",
            ));
        }
        if WRITE_VALUE_OPTIONS.contains(&arg) {
            let Some(path) = args.get(index + 1) else {
                return Some(inconclusive_mutator_ask(
                    "curl",
                    "an output path is missing",
                ));
            };
            if let Some(result) = validate_mutation_target("curl output", path, cwd) {
                return Some(result);
            }
            index += 2;
            continue;
        }
        let mut handled_long_output = false;
        for option in LONG_WRITE_OPTIONS {
            let Some(rest) = arg.strip_prefix(option) else {
                continue;
            };
            let Some(mut path) = rest.strip_prefix('=') else {
                continue;
            };
            let consumed_next;
            if path.is_empty() {
                let Some(next) = args.get(index + 1) else {
                    return Some(inconclusive_mutator_ask(
                        "curl",
                        "an output path is missing",
                    ));
                };
                path = next;
                consumed_next = true;
            } else {
                consumed_next = false;
            }
            if let Some(result) = validate_mutation_target("curl output", path, cwd) {
                return Some(result);
            }
            index += usize::from(consumed_next);
            handled_long_output = true;
            break;
        }
        if handled_long_output {
            index += 1;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            let mut handled_short_output = false;
            for (offset, ch) in arg.char_indices().skip(1) {
                if ch == 'w' {
                    let attached = &arg[offset + ch.len_utf8()..];
                    let (format, consumed_next) = if attached.is_empty() {
                        let Some(next) = args.get(index + 1) else {
                            return Some(inconclusive_mutator_ask(
                                "curl",
                                "a write-out format is missing",
                            ));
                        };
                        (next.as_str(), true)
                    } else {
                        (attached, false)
                    };
                    if format.starts_with('@') || format.contains("%output{") {
                        return Some(inconclusive_mutator_ask(
                            "curl",
                            "the write-out DSL is external or can redirect output to a hidden local filename",
                        ));
                    }
                    index += usize::from(consumed_next);
                    handled_short_output = true;
                    break;
                }
                if ch == 'K' {
                    return Some(inconclusive_mutator_ask(
                        "curl",
                        "a config file can hide local output directives",
                    ));
                }
                if ch == 'O' {
                    return Some(inconclusive_mutator_ask(
                        "curl",
                        "the local output filename is inferred from remote metadata",
                    ));
                }
                if matches!(ch, 'o' | 'D' | 'c') {
                    let attached = &arg[offset + ch.len_utf8()..];
                    let (path, consumed_next) = if attached.is_empty() {
                        let Some(next) = args.get(index + 1) else {
                            return Some(inconclusive_mutator_ask(
                                "curl",
                                "an output path is missing",
                            ));
                        };
                        (next.as_str(), true)
                    } else {
                        (attached, false)
                    };
                    if let Some(result) = validate_mutation_target("curl output", path, cwd) {
                        return Some(result);
                    }
                    index += usize::from(consumed_next);
                    handled_short_output = true;
                    break;
                }
            }
            if handled_short_output {
                index += 1;
                continue;
            }
        }
        index += 1;
    }
    None
}

fn check_wget_output(args: &[String], cwd: &str) -> Option<PermissionResult> {
    const WRITE_VALUE_OPTIONS: &[&str] = &[
        "-O",
        "--output-document",
        "-o",
        "--output-file",
        "-a",
        "--append-output",
        "--save-cookies",
        "--warc-file",
        "--warc-cdx",
        "--hsts-file",
        "--rejected-log",
        "-P",
        "--directory-prefix",
    ];
    const LONG_WRITE_OPTIONS: &[&str] = &[
        "--output-document",
        "--output-file",
        "--append-output",
        "--save-cookies",
        "--warc-file",
        "--warc-cdx",
        "--hsts-file",
        "--rejected-log",
        "--directory-prefix",
    ];

    let spider = args.iter().any(|arg| arg == "--spider");
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(arg, "-e" | "--execute" | "--config" | "--use-askpass")
            || arg.starts_with("--execute=")
            || arg.starts_with("--config=")
            || arg.starts_with("--use-askpass=")
            || arg.starts_with("-e") && arg.len() > 2
        {
            return Some(inconclusive_mutator_ask(
                "wget",
                "configuration directives can hide local output or delegated behavior",
            ));
        }
        if matches!(
            arg,
            "--content-disposition" | "--trust-server-names" | "--backup-converted"
        ) {
            return Some(inconclusive_mutator_ask(
                "wget",
                "the local output filename or an additional backup filename is inferred",
            ));
        }
        if WRITE_VALUE_OPTIONS.contains(&arg) {
            let Some(path) = args.get(index + 1) else {
                return Some(inconclusive_mutator_ask(
                    "wget",
                    "an output path is missing",
                ));
            };
            if spider {
                return Some(inconclusive_mutator_ask(
                    "wget",
                    "spider mode is only automatically allowed when no local output option is present",
                ));
            }
            if let Some(result) = validate_mutation_target("wget output", path, cwd) {
                return Some(result);
            }
            index += 2;
            continue;
        }
        let mut handled_long_output = false;
        for option in LONG_WRITE_OPTIONS {
            let Some(rest) = arg.strip_prefix(option) else {
                continue;
            };
            let Some(mut path) = rest.strip_prefix('=') else {
                continue;
            };
            let consumed_next;
            if path.is_empty() {
                let Some(next) = args.get(index + 1) else {
                    return Some(inconclusive_mutator_ask(
                        "wget",
                        "an output path is missing",
                    ));
                };
                path = next;
                consumed_next = true;
            } else {
                consumed_next = false;
            }
            if spider {
                return Some(inconclusive_mutator_ask(
                    "wget",
                    "spider mode is only automatically allowed when no local output option is present",
                ));
            }
            if let Some(result) = validate_mutation_target("wget output", path, cwd) {
                return Some(result);
            }
            index += usize::from(consumed_next);
            handled_long_output = true;
            break;
        }
        if handled_long_output {
            index += 1;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            let mut handled_short_output = false;
            for (offset, ch) in arg.char_indices().skip(1) {
                if ch == 'e' {
                    return Some(inconclusive_mutator_ask(
                        "wget",
                        "configuration directives can hide local output or delegated behavior",
                    ));
                }
                if matches!(ch, 'O' | 'o' | 'a' | 'P') {
                    if spider {
                        return Some(inconclusive_mutator_ask(
                            "wget",
                            "spider mode is only automatically allowed when no local output option is present",
                        ));
                    }
                    let attached = &arg[offset + ch.len_utf8()..];
                    let (path, consumed_next) = if attached.is_empty() {
                        let Some(next) = args.get(index + 1) else {
                            return Some(inconclusive_mutator_ask(
                                "wget",
                                "an output path is missing",
                            ));
                        };
                        (next.as_str(), true)
                    } else {
                        (attached, false)
                    };
                    if let Some(result) = validate_mutation_target("wget output", path, cwd) {
                        return Some(result);
                    }
                    index += usize::from(consumed_next);
                    handled_short_output = true;
                    break;
                }
            }
            if handled_short_output {
                index += 1;
                continue;
            }
        }
        index += 1;
    }
    if spider {
        return None;
    }
    Some(inconclusive_mutator_ask(
        "wget",
        "the local output filename is inferred from the URL",
    ))
}

fn check_awk_program(command: &str, args: &[String]) -> Option<PermissionResult> {
    if args.iter().any(|arg| {
        matches!(arg.as_str(), "-f" | "--file" | "-l" | "--load")
            || arg.starts_with("--file=")
            || arg.starts_with("--load=")
            || arg.starts_with("-f") && arg.len() > 2
            || arg.starts_with("-l") && arg.len() > 2
    }) {
        return Some(inconclusive_mutator_ask(
            command,
            "an external DSL program or extension may perform output redirection or execute commands",
        ));
    }
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(arg, "-e" | "--source" | "-E" | "--exec")
            || arg.starts_with("--source=")
            || arg.starts_with("--exec=")
        {
            return Some(inconclusive_mutator_ask(
                command,
                "a program supplied through an option is outside the proven pure DSL form",
            ));
        }
        if matches!(arg, "-F" | "-v" | "--field-separator" | "--assign") {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    command,
                    "a recognized option value is missing",
                ));
            }
            index += 2;
            continue;
        }
        if arg.starts_with("-F")
            || arg.starts_with("-v")
            || arg.starts_with("--field-separator=")
            || arg.starts_with("--assign=")
        {
            index += 1;
            continue;
        }
        if matches!(
            arg,
            "--posix"
                | "--traditional"
                | "--lint"
                | "--characters-as-bytes"
                | "--non-decimal-data"
                | "--sandbox"
                | "--csv"
        ) {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                command,
                "its option grammar is outside the closed safe allowlist",
            ));
        }
        break;
    }
    let program = args.get(index)?;
    let lower = program.to_ascii_lowercase();
    let print_redirect = lower
        .find("print")
        .is_some_and(|position| lower[position..].contains('>'));
    let printf_redirect = lower
        .find("printf")
        .is_some_and(|position| lower[position..].contains('>'));
    if awk_program_calls_system(program)
        || lower.contains("getline")
        || lower.contains('|')
        || lower.contains('@')
        || print_redirect
        || printf_redirect
    {
        return Some(inconclusive_mutator_ask(
            command,
            "its DSL can redirect output, load an extension, or execute/read from a nested command",
        ));
    }
    None
}

/// Match the AWK builtin as an identifier followed by arbitrary whitespace
/// and `(`. This covers `system (..)`, tabs, and embedded newlines without
/// treating an ordinary identifier such as `system_status` as execution.
fn awk_program_calls_system(program: &str) -> bool {
    static AWK_SYSTEM_CALL: OnceLock<regex::Regex> = OnceLock::new();
    // AWK accepts backslash-newline source continuation, including inside an
    // identifier or between the function name and `(`. Normalize it before
    // applying the identifier-boundary matcher.
    let normalized = program.replace("\\\r\n", "").replace("\\\n", "");
    AWK_SYSTEM_CALL
        .get_or_init(|| {
            regex::Regex::new(r"(?i)(?:^|[^[:alnum:]_])system[[:space:]]*\(")
                .expect("AWK system call regex must compile")
        })
        .is_match(&normalized)
}

fn check_sed_program(args: &[String]) -> Option<PermissionResult> {
    let mut scripts: Vec<&str> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut index = 0;
    let mut past_double_dash = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            past_double_dash = true;
            index += 1;
            continue;
        }
        if !past_double_dash && matches!(arg, "-f" | "--file") {
            return Some(inconclusive_mutator_ask(
                "sed",
                "an external script can hide write or command-execution directives",
            ));
        }
        if !past_double_dash
            && (arg.starts_with("--file=") || arg.starts_with("-f") && arg.len() > 2)
        {
            return Some(inconclusive_mutator_ask(
                "sed",
                "an external script can hide write or command-execution directives",
            ));
        }
        if !past_double_dash && matches!(arg, "-e" | "--expression") {
            let Some(script) = args.get(index + 1) else {
                return Some(inconclusive_mutator_ask(
                    "sed",
                    "an expression value is missing",
                ));
            };
            scripts.push(script);
            index += 2;
            continue;
        }
        if !past_double_dash && let Some(script) = arg.strip_prefix("--expression=") {
            if script.is_empty() {
                let Some(script) = args.get(index + 1) else {
                    return Some(inconclusive_mutator_ask(
                        "sed",
                        "an expression value is missing",
                    ));
                };
                scripts.push(script);
                index += 2;
            } else {
                scripts.push(script);
                index += 1;
            }
            continue;
        }
        if !past_double_dash && arg.starts_with("-e") && arg.len() > 2 {
            scripts.push(&arg[2..]);
            index += 1;
            continue;
        }
        if !past_double_dash
            && (matches!(
                arg,
                "-n" | "--quiet" | "--silent" | "-E" | "-r" | "--regexp-extended"
            ) || arg.starts_with("-i")
                || arg.starts_with("--in-place"))
        {
            index += 1;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "sed",
                "its option grammar was not recognized",
            ));
        }
        positionals.push(arg);
        index += 1;
    }

    if scripts.is_empty() {
        let script = positionals.first()?;
        scripts.push(script);
    }
    if scripts
        .iter()
        .any(|script| !sed_program_is_proven_safe(script))
    {
        return Some(inconclusive_mutator_ask(
            "sed",
            "the DSL is not in the proven non-writing, non-executing subset",
        ));
    }

    // File operands are validated later by the shared path extractor when
    // in-place editing is requested.
    None
}

fn sed_program_is_proven_safe(program: &str) -> bool {
    let script = program.trim();
    if matches!(script, "p" | "P" | "d" | "D" | "q" | "Q" | "n" | "N" | "=") {
        return true;
    }
    let mut chars = script.char_indices();
    if chars.next().map(|(_, ch)| ch) != Some('s') {
        return false;
    }
    let Some((_, delimiter)) = chars.next() else {
        return false;
    };
    if delimiter.is_ascii_alphanumeric() || delimiter.is_ascii_whitespace() || delimiter == '\\' {
        return false;
    }

    let mut escaped = false;
    let mut delimiters = 0;
    let mut flags_start = script.len();
    for (offset, ch) in script.char_indices().skip(2) {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == delimiter {
            delimiters += 1;
            if delimiters == 2 {
                flags_start = offset + ch.len_utf8();
                break;
            }
        }
    }
    if delimiters != 2 {
        return false;
    }
    script[flags_start..]
        .trim()
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, 'g' | 'p' | 'i' | 'I' | 'm' | 'M'))
}

fn git_subcommand(args: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(
            arg,
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            index += 2;
            continue;
        }
        if arg.starts_with("--git-dir=")
            || arg.starts_with("--work-tree=")
            || arg.starts_with("--namespace=")
        {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((arg, &args[index + 1..]));
    }
    None
}

fn git_forbidden_global_override(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(
            arg,
            "-C" | "-c"
                | "--config-env"
                | "--exec-path"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
        ) || arg.starts_with("-c") && arg.len() > 2
            || arg.starts_with("-C") && arg.len() > 2
            || arg.starts_with("--config-env=")
            || arg.starts_with("--exec-path=")
            || arg.starts_with("--git-dir=")
            || arg.starts_with("--work-tree=")
            || arg.starts_with("--namespace=")
        {
            return Some(arg);
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    None
}

/// Git dispatches unknown global options before the subcommand is known. Keep
/// a finite set of non-delegating global switches; a future/unknown switch may
/// alter executable or configuration discovery and therefore must not inherit
/// BypassPermissions or an explicit Allow rule.
fn git_unknown_global_option(args: &[String]) -> Option<&str> {
    for arg in args {
        if !arg.starts_with('-') {
            break;
        }
        if git_forbidden_global_override(std::slice::from_ref(arg)).is_some() {
            continue;
        }
        if matches!(
            arg.as_str(),
            "-P" | "--no-pager"
                | "--no-replace-objects"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
                | "--no-optional-locks"
                | "--no-lazy-fetch"
                | "--version"
                | "-v"
                | "--html-path"
                | "--man-path"
                | "--info-path"
        ) || arg.starts_with("--list-cmds=")
        {
            continue;
        }
        return Some(arg);
    }
    None
}

fn is_known_git_subcommand(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "add"
            | "am"
            | "annotate"
            | "apply"
            | "archive"
            | "bisect"
            | "blame"
            | "branch"
            | "bundle"
            | "cat-file"
            | "checkout"
            | "checkout-index"
            | "cherry"
            | "cherry-pick"
            | "clean"
            | "clone"
            | "column"
            | "commit"
            | "commit-tree"
            | "config"
            | "count-objects"
            | "credential"
            | "credential-cache"
            | "credential-store"
            | "describe"
            | "diff"
            | "diff-files"
            | "diff-index"
            | "diff-tree"
            | "difftool"
            | "fetch"
            | "fetch-pack"
            | "filter-branch"
            | "for-each-ref"
            | "format-patch"
            | "fsck"
            | "gc"
            | "get-tar-commit-id"
            | "grep"
            | "hash-object"
            | "help"
            | "index-pack"
            | "init"
            | "interpret-trailers"
            | "log"
            | "ls-files"
            | "ls-remote"
            | "ls-tree"
            | "maintenance"
            | "merge"
            | "merge-base"
            | "merge-file"
            | "merge-index"
            | "merge-ours"
            | "merge-recursive"
            | "merge-tree"
            | "mergetool"
            | "mktag"
            | "mktree"
            | "multi-pack-index"
            | "mv"
            | "name-rev"
            | "notes"
            | "pack-objects"
            | "pack-redundant"
            | "pack-refs"
            | "patch-id"
            | "prune"
            | "prune-packed"
            | "pull"
            | "push"
            | "range-diff"
            | "read-tree"
            | "rebase"
            | "reflog"
            | "remote"
            | "remote-ext"
            | "remote-fd"
            | "repack"
            | "replace"
            | "request-pull"
            | "rerere"
            | "reset"
            | "restore"
            | "rev-list"
            | "rev-parse"
            | "revert"
            | "rm"
            | "send-email"
            | "shortlog"
            | "show"
            | "show-branch"
            | "show-index"
            | "show-ref"
            | "sparse-checkout"
            | "status"
            | "stripspace"
            | "submodule"
            | "switch"
            | "symbolic-ref"
            | "tag"
            | "unpack-file"
            | "unpack-objects"
            | "update-index"
            | "update-ref"
            | "update-server-info"
            | "upload-archive"
            | "upload-pack"
            | "var"
            | "verify-commit"
            | "verify-pack"
            | "verify-tag"
            | "version"
            | "whatchanged"
            | "worktree"
            | "write-tree"
    )
}

fn git_requires_special_path_analysis(args: &[String]) -> bool {
    if git_forbidden_global_override(args).is_some() || git_unknown_global_option(args).is_some() {
        return true;
    }
    git_subcommand(args).is_some_and(|(subcommand, _)| {
        matches!(subcommand, "clone" | "config") || !is_known_git_subcommand(subcommand)
    })
}

/// Enforce the Git argv/config-injection floor.
///
/// Boundary: this function proves only the current argv plus environment
/// assignments present in the command AST. It is not a process sandbox and
/// cannot prove the absence of helpers already configured in a repository or
/// inherited parent environment (hooks, clean/smudge/textconv filters,
/// fsmonitor, merge drivers, or a pre-existing pager/editor). Covering that
/// state requires repository/config inspection at execution time or OS-level
/// confinement; it must not be claimed as covered by this argv classifier.
fn check_git_mutator(args: &[String], cwd: &str) -> Option<PermissionResult> {
    if let Some(option) = git_forbidden_global_override(args) {
        return Some(inconclusive_mutator_ask(
            "git",
            &format!(
                "global option '{option}' can change the effective directory, repository, executable path, hooks, filters, or aliases"
            ),
        ));
    }
    if let Some(option) = git_unknown_global_option(args) {
        return Some(inconclusive_mutator_ask(
            "git",
            &format!("global option '{option}' is outside the closed safe option set"),
        ));
    }
    let (subcommand, subargs) = git_subcommand(args)?;
    if !is_known_git_subcommand(subcommand) {
        return Some(inconclusive_mutator_ask(
            "git",
            "an unknown subcommand may be an executable alias or external git-* helper",
        ));
    }
    if matches!(
        subcommand,
        "difftool"
            | "mergetool"
            | "credential"
            | "credential-cache"
            | "credential-store"
            | "filter-branch"
            | "send-email"
            | "remote-ext"
            | "merge-index"
            | "help"
            | "verify-commit"
            | "verify-tag"
    ) || git_subcommand_action_is(subcommand, subargs, "bisect", "run")
        || git_subcommand_action_is(subcommand, subargs, "submodule", "foreach")
        // `submodule update` may execute a custom `submodule.<name>.update`
        // command already present in repository config. The command is not
        // visible in argv, so the whole action stays approval-only.
        || git_subcommand_action_is(subcommand, subargs, "submodule", "update")
    {
        return Some(inconclusive_mutator_ask(
            "git",
            "the selected subcommand can delegate to a local tool, credential helper, or user-supplied shell fragment",
        ));
    }
    if git_has_delegating_option(subcommand, subargs) {
        return Some(inconclusive_mutator_ask(
            "git",
            "the selected option can launch an editor, signer, external diff/text conversion, pager, merge strategy, rebase command, or transport helper",
        ));
    }
    if subcommand == "commit" && !git_commit_has_noninteractive_message_source(subargs) {
        return Some(inconclusive_mutator_ask(
            "git commit",
            "no non-interactive message source was supplied, so Git may launch an editor",
        ));
    }
    match subcommand {
        "clone" => check_git_clone(subargs, cwd),
        "config" => check_git_config(subargs),
        _ => git_unknown_subcommand_option(subcommand, subargs).map(|option| {
            inconclusive_mutator_ask(
                "git",
                &format!(
                    "option '{option}' for subcommand '{subcommand}' is outside the closed safe option set"
                ),
            )
        }),
    }
}

fn git_commit_has_noninteractive_message_source(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            let value = arg.as_str();
            matches!(
                value,
                "-m" | "--message"
                    | "-F"
                    | "--file"
                    | "-C"
                    | "--reuse-message"
                    | "--no-edit"
                    | "--dry-run"
            ) || ["--message=", "--file=", "--reuse-message="]
                .iter()
                .any(|prefix| value.starts_with(prefix))
                || ["-m", "-F", "-C"]
                    .iter()
                    .any(|prefix| value.starts_with(prefix) && value.len() > prefix.len())
        })
}

fn git_subcommand_action_is(
    subcommand: &str,
    args: &[String],
    expected_subcommand: &str,
    expected_action: &str,
) -> bool {
    subcommand == expected_subcommand
        && args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .find(|arg| !arg.starts_with('-'))
            .is_some_and(|action| action == expected_action)
}

fn git_has_delegating_option(subcommand: &str, args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            let value = arg.as_str();
            matches!(value, "--ext-diff" | "--textconv")
                && matches!(
                    subcommand,
                    "diff"
                        | "diff-files"
                        | "diff-index"
                        | "diff-tree"
                        | "log"
                        | "show"
                        | "whatchanged"
                )
                || (value == "--open-files-in-pager" || value.starts_with("--open-files-in-pager="))
                    && subcommand == "grep"
                || (matches!(value, "--exec" | "-x")
                    || value.starts_with("--exec=")
                    || value.starts_with("-x") && value.len() > 2)
                    && subcommand == "rebase"
                || (matches!(value, "--upload-pack" | "-u")
                    || value.starts_with("--upload-pack=")
                    || value.starts_with("-u") && value.len() > 2)
                    && matches!(
                        subcommand,
                        "clone" | "fetch" | "fetch-pack" | "ls-remote" | "pull"
                    )
                || (matches!(value, "--receive-pack" | "--exec")
                    || value.starts_with("--receive-pack=")
                    || value.starts_with("--exec="))
                    && subcommand == "push"
                || (matches!(value, "--strategy" | "-s")
                    || value.starts_with("--strategy=")
                    || value.starts_with("-s") && value.len() > 2)
                    && matches!(
                        subcommand,
                        "merge" | "pull" | "rebase" | "cherry-pick" | "revert"
                    )
                || matches!(
                    value,
                    "--edit" | "-e" | "--interactive" | "-i" | "--edit-description"
                ) && matches!(
                    subcommand,
                    "commit" | "merge" | "pull" | "rebase" | "tag" | "branch"
                )
                || (matches!(
                    value,
                    "--gpg-sign" | "-S" | "--sign" | "--local-user" | "-u"
                ) || value.starts_with("--gpg-sign=")
                    || value.starts_with("-S") && value.len() > 2)
                    && matches!(
                        subcommand,
                        "commit" | "merge" | "pull" | "rebase" | "cherry-pick" | "revert" | "tag"
                    )
                || (value == "--signed" || value.starts_with("--signed=")) && subcommand == "push"
        })
}

/// Return the first option outside the closed, non-delegating set for the
/// selected Git subcommand. Long-option abbreviations are intentionally not
/// accepted: Git may resolve them today, but a future option can change that
/// resolution and inherit an unsafe automatic decision.
fn git_unknown_subcommand_option<'a>(subcommand: &str, args: &'a [String]) -> Option<&'a str> {
    let (boolean_options, value_options): (&[&str], &[&str]) = match subcommand {
        "status" => (
            &[
                "-s",
                "--short",
                "-b",
                "--branch",
                "--show-stash",
                "--porcelain",
                "--long",
                "-v",
                "--verbose",
                "-u",
                "--untracked-files",
                "--ignored",
                "--ignore-submodules",
                "--no-renames",
                "-z",
                "--null",
                "--ahead-behind",
                "--no-ahead-behind",
                "--renames",
            ],
            &["--find-renames"],
        ),
        "log" | "show" | "whatchanged" => (
            &[
                "--oneline",
                "--graph",
                "--stat",
                "--numstat",
                "--shortstat",
                "--name-only",
                "--name-status",
                "--summary",
                "--patch",
                "-p",
                "--no-patch",
                "--all",
                "--branches",
                "--tags",
                "--remotes",
                "--first-parent",
                "--merges",
                "--no-merges",
                "--reverse",
                "--date-order",
                "--author-date-order",
                "--topo-order",
                "--decorate",
                "--no-decorate",
                "--color",
                "--no-color",
                "--abbrev-commit",
                "--no-abbrev-commit",
            ],
            &[
                "-n",
                "--max-count",
                "--skip",
                "--since",
                "--after",
                "--until",
                "--before",
                "--author",
                "--committer",
                "--grep",
                "--format",
                "--pretty",
                "--date",
                "--abbrev",
            ],
        ),
        "diff" | "diff-files" | "diff-index" | "diff-tree" => (
            &[
                "--stat",
                "--numstat",
                "--shortstat",
                "--name-only",
                "--name-status",
                "--check",
                "--summary",
                "--patch",
                "-p",
                "--no-patch",
                "--cached",
                "--staged",
                "--quiet",
                "--exit-code",
                "--color",
                "--no-color",
                "--word-diff",
                "--minimal",
                "--patience",
                "--histogram",
                "--binary",
                "--full-index",
            ],
            &[
                "-U",
                "--unified",
                "--diff-filter",
                "--color-moved",
                "--color-moved-ws",
                "--word-diff-regex",
                "--src-prefix",
                "--dst-prefix",
                "--line-prefix",
            ],
        ),
        "add" => (
            &[
                "-n",
                "--dry-run",
                "-v",
                "--verbose",
                "-f",
                "--force",
                "-u",
                "--update",
                "-A",
                "--all",
                "--ignore-removal",
                "--refresh",
                "--ignore-errors",
                "--ignore-missing",
                "--renormalize",
                "-N",
                "--intent-to-add",
                "--sparse",
            ],
            &["--chmod"],
        ),
        "commit" => (
            &[
                "-a",
                "--all",
                "--amend",
                "--no-edit",
                "--allow-empty",
                "--allow-empty-message",
                "--no-verify",
                "-n",
                "--dry-run",
                "--short",
                "--branch",
                "--porcelain",
                "--long",
                "--null",
                "-z",
                "--quiet",
                "-q",
                "--verbose",
                "-v",
            ],
            &[
                "-m",
                "--message",
                "-F",
                "--file",
                "--author",
                "--date",
                "--cleanup",
                "--fixup",
                "--squash",
                "-C",
                "--reuse-message",
            ],
        ),
        "rev-parse" => (
            &[
                "--verify",
                "--quiet",
                "-q",
                "--short",
                "--abbrev-ref",
                "--symbolic",
                "--symbolic-full-name",
                "--show-toplevel",
                "--show-cdup",
                "--show-prefix",
                "--show-superproject-working-tree",
                "--git-dir",
                "--git-common-dir",
                "--is-inside-git-dir",
                "--is-inside-work-tree",
                "--is-bare-repository",
                "--is-shallow-repository",
                "--show-object-format",
                "--show-ref-format",
                "--revs-only",
                "--no-revs",
                "--flags",
                "--no-flags",
                "--default",
                "--local-env-vars",
                "--path-format=absolute",
                "--path-format=relative",
            ],
            &["--short", "--abbrev", "--disambiguate", "--prefix"],
        ),
        "branch" => (
            &[
                "-a",
                "--all",
                "-r",
                "--remotes",
                "-l",
                "--list",
                "-v",
                "--verbose",
                "--show-current",
                "--no-color",
                "--ignore-case",
                "--column",
                "--no-column",
            ],
            &[
                "--sort",
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
                "--format",
                "--color",
            ],
        ),
        "ls-files" => (
            &[
                "-c",
                "--cached",
                "-d",
                "--deleted",
                "-m",
                "--modified",
                "-o",
                "--others",
                "-i",
                "--ignored",
                "-s",
                "--stage",
                "-z",
                "--deduplicate",
                "--sparse",
                "--exclude-standard",
                "--full-name",
                "--recurse-submodules",
                "--error-unmatch",
                "--with-tree",
                "--eol",
                "--debug",
            ],
            &[
                "-x",
                "--exclude",
                "-X",
                "--exclude-from",
                "--exclude-per-directory",
                "--format",
                "--abbrev",
            ],
        ),
        "blame" => (
            &[
                "-l",
                "-t",
                "-p",
                "--porcelain",
                "--line-porcelain",
                "-w",
                "-M",
                "-C",
                "-s",
                "--show-stats",
                "--show-number",
                "--show-name",
                "--root",
                "--incremental",
            ],
            &[
                "-L",
                "--date",
                "--abbrev",
                "--ignore-rev",
                "--ignore-revs-file",
            ],
        ),
        "tag" => (
            &[
                "-l",
                "--list",
                "--no-color",
                "--ignore-case",
                "--column",
                "--no-column",
            ],
            &[
                "-n",
                "--sort",
                "--contains",
                "--no-contains",
                "--points-at",
                "--merged",
                "--no-merged",
                "--format",
                "--color",
            ],
        ),
        "remote" => (&["-v", "--verbose"], &[]),
        "describe" => (
            &[
                "--tags",
                "--all",
                "--long",
                "--always",
                "--exact-match",
                "--contains",
                "--first-parent",
                "--debug",
            ],
            &[
                "--abbrev",
                "--match",
                "--exclude",
                "--candidates",
                "--dirty",
                "--broken",
            ],
        ),
        "merge-base" => (
            &[
                "--all",
                "-a",
                "--octopus",
                "--independent",
                "--is-ancestor",
                "--fork-point",
            ],
            &[],
        ),
        _ => (&[], &[]),
    };

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            return None;
        }
        if !arg.starts_with('-') || arg == "-" {
            index += 1;
            continue;
        }
        if boolean_options.contains(&arg) {
            index += 1;
            continue;
        }
        if value_options.contains(&arg) {
            if index + 1 >= args.len() {
                return Some(arg);
            }
            index += 2;
            continue;
        }
        let equals_value = value_options.iter().find_map(|option| {
            arg.strip_prefix(option)
                .and_then(|rest| rest.strip_prefix('='))
        });
        if let Some(value) = equals_value {
            if value.is_empty() {
                if index + 1 >= args.len() {
                    return Some(arg);
                }
                // The shared shell parser can represent `--message='text'`
                // as `--message=` plus the quoted value. Preserve that argv
                // boundary while still rejecting a genuinely missing value.
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if matches!(subcommand, "log" | "show" | "whatchanged")
            && arg.strip_prefix("-n").is_some_and(|value| {
                !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
            })
            || matches!(
                subcommand,
                "diff" | "diff-files" | "diff-index" | "diff-tree"
            ) && arg.strip_prefix("-U").is_some_and(|value| {
                !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
            })
        {
            index += 1;
            continue;
        }
        return Some(arg);
    }
    None
}

fn check_git_clone(args: &[String], cwd: &str) -> Option<PermissionResult> {
    const SAFE_BOOLEAN_FLAGS: &[&str] = &[
        "-l",
        "--local",
        "--no-hardlinks",
        "-s",
        "--shared",
        "--dissociate",
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "--progress",
        "-n",
        "--no-checkout",
        "--reject-shallow",
        "--no-reject-shallow",
        "--bare",
        "--sparse",
        "--also-filter-submodules",
        "--single-branch",
        "--no-single-branch",
        "--no-tags",
        "--shallow-submodules",
        "--remote-submodules",
    ];
    const SAFE_VALUE_FLAGS: &[&str] = &[
        "-b",
        "--branch",
        "-o",
        "--origin",
        "--depth",
        "--filter",
        "--reference",
        "--reference-if-able",
        "--separate-git-dir",
        "-j",
        "--jobs",
        "--server-option",
        "--revision",
        "--ref-format",
        "--bundle-uri",
    ];
    const DELEGATING_VALUE_FLAGS: &[&str] =
        &["-c", "--config", "-u", "--upload-pack", "--template"];
    let mut positionals: Vec<&str> = Vec::new();
    let mut index = 0;
    let mut past_double_dash = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            past_double_dash = true;
            index += 1;
            continue;
        }
        if !past_double_dash
            && (SAFE_VALUE_FLAGS.contains(&arg) || DELEGATING_VALUE_FLAGS.contains(&arg))
        {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    "git clone",
                    "an option value is missing",
                ));
            }
            if DELEGATING_VALUE_FLAGS.contains(&arg) {
                return Some(inconclusive_mutator_ask(
                    "git clone",
                    "clone-time config, template, or upload command can alter hooks, filters, or delegated execution",
                ));
            }
            if arg == "--separate-git-dir"
                && let Some(result) =
                    validate_mutation_target("git separate directory", &args[index + 1], cwd)
            {
                return Some(result);
            }
            index += 2;
            continue;
        }
        if !past_double_dash && SAFE_BOOLEAN_FLAGS.contains(&arg) {
            index += 1;
            continue;
        }
        if !past_double_dash
            && (arg == "--recurse-submodules" || arg.starts_with("--recurse-submodules="))
        {
            index += 1;
            continue;
        }
        if !past_double_dash && let Some(mut path) = arg.strip_prefix("--separate-git-dir=") {
            let consumed_next;
            if path.is_empty() {
                let Some(next) = args.get(index + 1) else {
                    return Some(inconclusive_mutator_ask(
                        "git clone",
                        "the separate git directory is missing",
                    ));
                };
                path = next;
                consumed_next = true;
            } else {
                consumed_next = false;
            }
            if let Some(result) = validate_mutation_target("git separate directory", path, cwd) {
                return Some(result);
            }
            index += 1 + usize::from(consumed_next);
            continue;
        }
        if !past_double_dash && git_option_has_attached_value(arg, DELEGATING_VALUE_FLAGS) {
            return Some(inconclusive_mutator_ask(
                "git clone",
                "clone-time config, template, or upload command can alter hooks, filters, or delegated execution",
            ));
        }
        if !past_double_dash && git_option_has_attached_value(arg, SAFE_VALUE_FLAGS) {
            index += 1;
            continue;
        }
        if !past_double_dash && arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "git clone",
                &format!("option '{arg}' is outside the closed safe option set"),
            ));
        }
        positionals.push(arg);
        index += 1;
    }
    match positionals.as_slice() {
        [] => None,
        [_repo] => Some(inconclusive_mutator_ask(
            "git clone",
            "the local destination is inferred from remote metadata and may resolve through an existing symlink",
        )),
        [_repo, destination] => validate_mutation_target("git clone destination", destination, cwd),
        _ => Some(inconclusive_mutator_ask(
            "git clone",
            "more than one destination-like operand was present",
        )),
    }
}

fn git_option_has_attached_value(arg: &str, options: &[&str]) -> bool {
    options.iter().any(|option| {
        if option.starts_with("--") {
            arg.strip_prefix(option)
                .and_then(|rest| rest.strip_prefix('='))
                .is_some_and(|value| !value.is_empty())
        } else {
            arg.strip_prefix(option)
                .is_some_and(|value| !value.is_empty())
        }
    })
}

fn check_git_config(args: &[String]) -> Option<PermissionResult> {
    const VALUE_FLAGS: &[&str] = &[
        "--file",
        "--blob",
        "--type",
        "--default",
        "--comment",
        "--expiry-date",
    ];
    const SAFE_BOOLEAN_FLAGS: &[&str] = &[
        "--global",
        "--system",
        "--local",
        "--worktree",
        "--fixed-value",
        "--includes",
        "--no-includes",
        "-z",
        "--null",
        "--name-only",
        "--show-origin",
        "--show-names",
        "--show-scope",
        "--get",
        "--get-all",
        "--get-regexp",
        "--get-urlmatch",
        "--list",
        "-l",
        "--bool",
        "--int",
        "--bool-or-int",
        "--bool-or-str",
        "--path",
        "--color",
    ];
    let write_flag = args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--add"
                | "--replace-all"
                | "--unset"
                | "--unset-all"
                | "--rename-section"
                | "--remove-section"
                | "--edit"
        )
    });
    let mut positionals = 0;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if VALUE_FLAGS.contains(&arg) {
            if index + 1 >= args.len() {
                return Some(inconclusive_mutator_ask(
                    "git config",
                    "a recognized option value is missing",
                ));
            }
            index += 2;
            continue;
        }
        if git_option_has_attached_value(arg, VALUE_FLAGS) || SAFE_BOOLEAN_FLAGS.contains(&arg) {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Some(inconclusive_mutator_ask(
                "git config",
                &format!("option '{arg}' is outside the closed safe option set"),
            ));
        }
        if !arg.starts_with('-') {
            positionals += 1;
        }
        index += 1;
    }
    (write_flag || positionals >= 2).then(|| {
        inconclusive_mutator_ask(
            "git config",
            "configuration writes can change hooks, filters, credentials, or executable policy",
        )
    })
}

/// Argv-level normalization for wrappers already accepted by the shared shell
/// parser. Unknown wrapper syntax returns an error so the caller can fail
/// closed instead of examining the wrong executable.
fn strip_safe_wrappers_from_argv(mut argv: &[String]) -> Result<&[String], &'static str> {
    loop {
        let Some(command_index) = safe_wrapper_command_index(argv)? else {
            return Ok(argv);
        };
        argv = &argv[command_index..];
    }
}

/// Return the effective command index for one recognized wrapper. `None`
/// means argv already starts with the executable to inspect.
fn safe_wrapper_command_index(argv: &[String]) -> Result<Option<usize>, &'static str> {
    let Some(raw_name) = argv.first().map(String::as_str) else {
        return Ok(None);
    };
    let name = command_basename(raw_name);
    match name.as_str() {
        "time" | "nohup" => {
            if argv
                .get(1)
                .is_some_and(|arg| arg.starts_with('-') && arg != "--")
            {
                return Err("unrecognized time/nohup arguments");
            }
            let index = usize::from(argv.get(1).is_some_and(|arg| arg == "--")) + 1;
            (index < argv.len())
                .then_some(Some(index))
                .ok_or("wrapper command is missing")
        }
        "nice" => {
            let mut index = 1;
            if argv.get(index).is_some_and(|arg| arg == "-n") {
                if argv
                    .get(index + 1)
                    .is_none_or(|value| value.parse::<i32>().is_err())
                {
                    return Err("unrecognized nice adjustment");
                }
                index += 2;
            } else if argv.get(index).is_some_and(|arg| {
                arg.strip_prefix("-n")
                    .is_some_and(|value| !value.is_empty() && value.parse::<i32>().is_ok())
                    || arg
                        .strip_prefix("--adjustment=")
                        .is_some_and(|value| value.parse::<i32>().is_ok())
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|value| value.parse::<u32>().is_ok())
            }) {
                index += 1;
            } else if argv
                .get(index)
                .is_some_and(|arg| arg.starts_with('-') && arg != "--")
            {
                return Err("unrecognized nice arguments");
            }
            if argv.get(index).is_some_and(|arg| arg == "--") {
                index += 1;
            }
            (index < argv.len())
                .then_some(Some(index))
                .ok_or("wrapper command is missing")
        }
        "timeout" => timeout_wrapped_command_index(argv)
            .map(Some)
            .ok_or("unrecognized timeout arguments"),
        "stdbuf" => stdbuf_wrapped_command_index(argv)
            .map(Some)
            .ok_or("unrecognized stdbuf arguments"),
        "env" => env_wrapped_command_index(argv)
            .map(Some)
            .ok_or("unrecognized env arguments"),
        _ => Ok(None),
    }
}

fn timeout_wrapped_command_index(argv: &[String]) -> Option<usize> {
    let mut index = 1;
    while let Some(arg) = argv.get(index).map(String::as_str) {
        if matches!(
            arg,
            "--foreground" | "--preserve-status" | "--verbose" | "-v"
        ) {
            index += 1;
        } else if arg == "--kill-after" || arg == "--signal" || arg == "-k" || arg == "-s" {
            if !argv
                .get(index + 1)
                .is_some_and(|value| is_safe_wrapper_value(value))
            {
                return None;
            }
            index += 2;
        } else if arg.starts_with("--kill-after=")
            || arg.starts_with("--signal=")
            || arg.starts_with("-k") && arg.len() > 2
            || arg.starts_with("-s") && arg.len() > 2
        {
            let value = arg.split_once('=').map_or(&arg[2..], |(_, value)| value);
            if !is_safe_wrapper_value(value) {
                return None;
            }
            index += 1;
        } else if arg == "--" {
            index += 1;
            break;
        } else if arg.starts_with('-') {
            return None;
        } else {
            break;
        }
    }
    let duration = argv.get(index)?;
    if !is_timeout_duration(duration) {
        return None;
    }
    (index + 1 < argv.len()).then_some(index + 1)
}

fn stdbuf_wrapped_command_index(argv: &[String]) -> Option<usize> {
    let mut index = 1;
    while let Some(arg) = argv.get(index).map(String::as_str) {
        if matches!(arg, "-i" | "-o" | "-e") {
            argv.get(index + 1)?;
            index += 2;
        } else if arg.starts_with("--input=")
            || arg.starts_with("--output=")
            || arg.starts_with("--error=")
            || arg.len() > 2
                && arg.starts_with('-')
                && matches!(arg.as_bytes().get(1), Some(b'i' | b'o' | b'e'))
        {
            index += 1;
        } else if arg == "--" {
            index += 1;
            break;
        } else if arg.starts_with('-') {
            return None;
        } else {
            break;
        }
    }
    (index < argv.len()).then_some(index)
}

fn env_wrapped_command_index(argv: &[String]) -> Option<usize> {
    let mut index = 1;
    let mut options_open = true;
    while let Some(arg) = argv.get(index).map(String::as_str) {
        // POSIX/coreutils `--` ends option parsing, not the following
        // NAME=value assignment section. Continue over assignments so
        // `env -- PATH=/tmp command` cannot be mistaken for an executable
        // literally named `PATH=/tmp`.
        if !arg.starts_with('-') && arg.contains('=')
            || options_open && matches!(arg, "-i" | "-0" | "-v")
        {
            index += 1;
        } else if options_open && arg == "-u" {
            argv.get(index + 1)?;
            index += 2;
        } else if options_open && arg == "--" {
            options_open = false;
            index += 1;
        } else if options_open && arg.starts_with('-') {
            return None;
        } else {
            break;
        }
    }
    (index < argv.len()).then_some(index)
}

fn is_safe_wrapper_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '+' | '-'))
}

fn is_timeout_duration(value: &str) -> bool {
    let numeric = value.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(value);
    !numeric.is_empty()
        && numeric.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        && numeric.chars().filter(|ch| *ch == '.').count() <= 1
}

/// `PowerShell` 命令权限检查
///
/// 流程: deny/ask/allow 规则 → Ask（安全扫描由调用方执行）
#[must_use]
pub fn check_powershell_permission(command: &str, ctx: &PermissionContext) -> PermissionResult {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return PermissionResult::deny(
            "Empty command",
            PermissionDecisionReason::Other {
                reason: "empty command".to_string(),
            },
        );
    }

    // 1. DontAsk 模式。BypassPermissions 必须先经过显式规则检查。
    if ctx.mode == PermissionMode::DontAsk {
        return PermissionResult::deny(
            "Permission denied (DontAsk mode)",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::DontAsk,
            },
        );
    }

    // 2. 规则匹配
    if let Some((behavior, rule)) = match_command_against_rules(trimmed, &ctx.rules, "PowerShell") {
        match behavior {
            PermissionBehavior::Deny => {
                return PermissionResult::deny(
                    format!("Denied by rule: {:?}", rule.value),
                    PermissionDecisionReason::Rule { rule: rule.clone() },
                );
            }
            PermissionBehavior::Allow => {
                return PermissionResult::allow_with_reason(PermissionDecisionReason::Rule {
                    rule: rule.clone(),
                });
            }
            PermissionBehavior::Ask => {
                return PermissionResult::Ask {
                    message: format!("Approval required by rule: {:?}", rule.value),
                    suggestions: vec![],
                    blocked_path: None,
                };
            }
        }
    }

    // 3. PowerShell 的安全扫描由调用方执行；到达这里后 bypass 只跳过
    // 常规逐次审批，显式 deny/ask 规则仍然优先。
    if ctx.mode == PermissionMode::BypassPermissions {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::Mode {
            mode: PermissionMode::BypassPermissions,
        });
    }

    // 4. Plan 模式
    if ctx.mode == PermissionMode::Plan {
        return PermissionResult::deny(
            "Plan mode: command would be executed",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::Plan,
            },
        );
    }

    // 5. 默认 Ask
    PermissionResult::Ask {
        message: format!("Allow PowerShell command: {trimmed}"),
        suggestions: build_powershell_suggestions(trimmed),
        blocked_path: None,
    }
}

/// 命令与规则列表匹配
///
/// 规则优先级: deny > ask > allow
/// 返回匹配的行为和规则
#[must_use]
pub fn match_command_against_rules<'a>(
    command: &str,
    rules: &'a [PermissionRule],
    tool_name: &str,
) -> Option<(PermissionBehavior, &'a PermissionRule)> {
    // 分离 deny / ask / allow 规则
    let mut deny_match: Option<&'a PermissionRule> = None;
    let mut allow_match: Option<&'a PermissionRule> = None;
    let mut ask_match: Option<&'a PermissionRule> = None;

    for rule in rules {
        // 工具名匹配
        if !rule.value.tool_name.eq_ignore_ascii_case(tool_name) {
            continue;
        }

        // 规则内容匹配
        let matches = match &rule.value.rule_content {
            None => true, // 无内容 → 匹配所有该工具的命令
            Some(content) => {
                let shell_rule = parse_shell_rule(content);
                if matches_rule(&shell_rule, command) {
                    true
                } else if tool_name.eq_ignore_ascii_case("Bash")
                    && matches!(
                        rule.behavior,
                        PermissionBehavior::Deny | PermissionBehavior::Ask
                    )
                {
                    // Restrictive rules are deliberately harder to bypass than
                    // allow rules. A leading environment assignment may alter
                    // execution context, but it must not hide the underlying
                    // command from an explicit deny/ask decision.
                    let stripped = strip_all_leading_env_assignments(command);
                    stripped != command && matches_rule(&shell_rule, stripped)
                } else {
                    false
                }
            }
        };

        if !matches {
            continue;
        }

        match rule.behavior {
            PermissionBehavior::Deny => {
                if deny_match.is_none() {
                    deny_match = Some(rule);
                }
            }
            PermissionBehavior::Allow => {
                if allow_match.is_none() {
                    allow_match = Some(rule);
                }
            }
            PermissionBehavior::Ask => {
                if ask_match.is_none() {
                    ask_match = Some(rule);
                }
            }
        }
    }

    // 优先级: deny > ask > allow
    if let Some(rule) = deny_match {
        return Some((PermissionBehavior::Deny, rule));
    }
    if let Some(rule) = ask_match {
        return Some((PermissionBehavior::Ask, rule));
    }
    if let Some(rule) = allow_match {
        return Some((PermissionBehavior::Allow, rule));
    }

    None
}

/// Strip syntactically inert leading Bash environment assignments for
/// restrictive rule matching only.
///
/// Values containing expansion or shell operators are intentionally not
/// stripped. In that case the normal permission flow remains fail-closed at
/// Ask. Allow rules never use this helper, so an environment wrapper cannot
/// borrow a pre-existing allow rule.
fn strip_all_leading_env_assignments(command: &str) -> &str {
    static LEADING_ENV_ASSIGNMENT: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = LEADING_ENV_ASSIGNMENT.get_or_init(|| {
        regex::Regex::new(
            r#"^[A-Za-z_][A-Za-z0-9_]*\+?=(?:'[^'\n\r]*'|"(?:\\.|[^"$`\\\n\r])*"|\\.|[^ \t\n\r$`;|&()<>\\'"])*[ \t]+"#,
        )
        .expect("leading environment assignment regex must compile")
    });

    let mut stripped = command.trim();
    while let Some(prefix) = pattern.find(stripped) {
        stripped = stripped[prefix.end()..].trim_start();
    }
    stripped
}

/// 提取命令前缀（用于权限建议）
#[must_use]
pub fn extract_command_prefix(command: &str) -> String {
    let trimmed = command.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();

    if parts.is_empty() {
        return String::new();
    }

    let first = parts[0];

    // 如果第一个词是 shell 包装器，取第二个词
    let bare_shells = constants::bare_shell_prefixes();
    if bare_shells.contains(first) && parts.len() > 1 {
        // 对 env, sudo 等，跳过 flags
        let mut cmd_idx = 1;
        while cmd_idx < parts.len() && parts[cmd_idx].starts_with('-') {
            cmd_idx += 1;
        }
        if cmd_idx < parts.len() {
            return parts[cmd_idx].to_string();
        }
    }

    first.to_string()
}

// ─── Internal Helpers ───

/// 构建 Bash 权限建议
fn build_bash_suggestions(command: &str) -> Vec<PermissionUpdate> {
    let prefix = extract_command_prefix(command);
    let mut suggestions = Vec::new();

    if !prefix.is_empty() {
        // 建议: 允许以此前缀开头的所有命令
        suggestions.push(PermissionUpdate {
            behavior: PermissionBehavior::Allow,
            value: PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some(format!("{prefix} *")),
            },
            source: PermissionRuleSource::Session,
        });

        // 建议: 允许此精确命令
        suggestions.push(PermissionUpdate {
            behavior: PermissionBehavior::Allow,
            value: PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some(command.to_string()),
            },
            source: PermissionRuleSource::Session,
        });
    }

    suggestions
}

/// 构建 `PowerShell` 权限建议
fn build_powershell_suggestions(command: &str) -> Vec<PermissionUpdate> {
    let prefix = extract_command_prefix(command);
    let mut suggestions = Vec::new();

    if !prefix.is_empty() {
        suggestions.push(PermissionUpdate {
            behavior: PermissionBehavior::Allow,
            value: PermissionRuleValue {
                tool_name: "PowerShell".to_string(),
                rule_content: Some(format!("{prefix} *")),
            },
            source: PermissionRuleSource::Session,
        });

        suggestions.push(PermissionUpdate {
            behavior: PermissionBehavior::Allow,
            value: PermissionRuleValue {
                tool_name: "PowerShell".to_string(),
                rule_content: Some(command.to_string()),
            },
            source: PermissionRuleSource::Session,
        });
    }

    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }

        fn clear(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            #[allow(unsafe_code)]
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn make_context(mode: PermissionMode, rules: Vec<PermissionRule>) -> PermissionContext {
        PermissionContext {
            cwd: "/home/user/project".to_string(),
            original_cwd: None,
            mode,
            rules,
            tool_name: "Bash".to_string(),
        }
    }

    fn make_rule(
        behavior: PermissionBehavior,
        tool: &str,
        content: Option<&str>,
    ) -> PermissionRule {
        PermissionRule {
            source: PermissionRuleSource::Session,
            behavior,
            value: PermissionRuleValue {
                tool_name: tool.to_string(),
                rule_content: content.map(|s| s.to_string()),
            },
        }
    }

    fn assert_bypass_and_exact_allow_ask(command: &str) {
        let bypass = make_context(PermissionMode::BypassPermissions, vec![]);
        let bypass_result = check_bash_permission(command, &bypass);
        assert!(
            bypass_result.is_ask(),
            "BypassPermissions must not cross the safety floor: {command} => {bypass_result:?}"
        );

        let allow_rule = make_rule(PermissionBehavior::Allow, "Bash", Some(command));
        assert_eq!(
            match_command_against_rules(command, std::slice::from_ref(&allow_rule), "Bash")
                .expect("the exact Allow rule must match before exercising the floor")
                .0,
            PermissionBehavior::Allow
        );
        let explicitly_allowed = make_context(PermissionMode::Default, vec![allow_rule]);
        let allow_result = check_bash_permission(command, &explicitly_allowed);
        assert!(
            allow_result.is_ask(),
            "an explicit matching Allow must not cross the safety floor: {command} => {allow_result:?}"
        );
    }

    fn assert_bypass_and_exact_allow_allow(command: &str) {
        let bypass = make_context(PermissionMode::BypassPermissions, vec![]);
        let bypass_result = check_bash_permission(command, &bypass);
        assert!(
            bypass_result.is_allow(),
            "ordinary bypass flow should remain usable: {command} => {bypass_result:?}"
        );

        let allow_rule = make_rule(PermissionBehavior::Allow, "Bash", Some(command));
        assert_eq!(
            match_command_against_rules(command, std::slice::from_ref(&allow_rule), "Bash")
                .expect("the exact Allow rule must match")
                .0,
            PermissionBehavior::Allow
        );
        let explicitly_allowed = make_context(PermissionMode::Default, vec![allow_rule]);
        let allow_result = check_bash_permission(command, &explicitly_allowed);
        assert!(
            allow_result.is_allow(),
            "a proven ordinary command should retain its explicit Allow: {command} => {allow_result:?}"
        );
    }

    fn assert_bypass_and_exact_allow_ask_message(command: &str, fragment: &str) {
        let contexts = [
            make_context(PermissionMode::BypassPermissions, vec![]),
            make_context(
                PermissionMode::Default,
                vec![make_rule(PermissionBehavior::Allow, "Bash", Some(command))],
            ),
        ];
        for context in contexts {
            match check_bash_permission(command, &context) {
                PermissionResult::Ask { message, .. } => assert!(
                    message.contains(fragment),
                    "expected '{fragment}' in safety-floor message for {command}, got: {message}"
                ),
                result => panic!("expected safety-floor Ask for {command}, got {result:?}"),
            }
        }
    }

    #[test]
    fn awk_system_calls_with_whitespace_or_newlines_cross_neither_approval_path() {
        for command in [
            "awk 'BEGIN { system (\"id\") }' data.txt",
            "awk 'BEGIN { system\t(\"id\") }' data.txt",
            "awk 'BEGIN { system\n(\"id\") }' data.txt",
            "gawk 'BEGIN { SyStEm (\"id\") }' data.txt",
            r#"awk 'BEGIN { system \
("id") }' data.txt"#,
            r#"awk 'BEGIN { sys\
tem("id") }' data.txt"#,
        ] {
            assert_bypass_and_exact_allow_ask(command);
        }

        // Identifier boundaries avoid needlessly converting ordinary AWK data
        // fields into a privileged confirmation.
        for command in [
            "awk '{ print system_status }' data.txt",
            "awk '{ print ecosystem(value) }' data.txt",
        ] {
            assert_bypass_and_exact_allow_allow(command);
        }
    }

    #[test]
    fn sensitive_execution_environment_survives_ast_and_wrapper_normalization() {
        for command in [
            "PATH=/tmp git status",
            "HOME=/tmp git status",
            "XDG_CONFIG_HOME=/tmp git status",
            "LD_AUDIT=/tmp/audit.so git status",
            "LD_PRELOAD=/tmp/preload.so git status",
            "DYLD_INSERT_LIBRARIES=/tmp/inject.dylib git status",
            "GIT_EXEC_PATH=/tmp git status",
            "GIT_SSH_COMMAND=helper git status",
            "GIT_SEQUENCE_EDITOR=helper git status",
            "GIT_PAGER=helper git status",
            "GIT_CONFIG_COUNT=1 git status",
            "GIT_TRACE=/tmp/trace git status",
            "SSH_ASKPASS=helper git status",
            "LESSOPEN=helper git status",
            "LESSCLOSE=helper git status",
            "MANPAGER=helper git status",
            "SHELL=/tmp/helper git status",
            "env GIT_EXEC_PATH=/tmp git status",
            "env XDG_CONFIG_HOME=/tmp git status",
            "env -- GIT_EXEC_PATH=/tmp git status",
            "env -i -- XDG_CONFIG_HOME=/tmp git status",
            "env -u PATH git status",
            "env -i git status",
            "timeout 1 env PAGER=helper git status",
            "nice env GIT_EXTERNAL_DIFF=helper git status",
        ] {
            assert_bypass_and_exact_allow_ask(command);
        }

        for command in [
            "FOO=bar git status",
            "env FOO=bar git status",
            "timeout 1 env FOO=bar git status",
        ] {
            assert_bypass_and_exact_allow_allow(command);
        }

        // These names are not handled by the legacy text scanner, so this
        // assertion pins the AST/wrapper floor itself rather than only the
        // final decision kind.
        for command in [
            "GIT_SSH_COMMAND=helper git status",
            "env GIT_EXEC_PATH=/tmp git status",
            "env -u PATH git status",
        ] {
            assert_bypass_and_exact_allow_ask_message(command, "Execution environment override");
        }
    }

    #[test]
    fn git_delegated_execution_and_unknown_options_cross_neither_approval_path() {
        for command in [
            "git submodule foreach 'echo ok'",
            "git submodule update --init",
            "git bisect run ./tests.sh",
            "git filter-branch --tree-filter 'echo ok' HEAD",
            "git difftool --tool=custom",
            "git mergetool --tool=custom",
            "git send-email --smtp-server=/tmp/helper patch.eml",
            "git remote-ext origin helper",
            "git merge-index custom-helper -a",
            "git help status",
            "git verify-commit HEAD",
            "git diff --ext-diff",
            "git diff --textconv",
            "git grep --open-files-in-pager=helper needle",
            "git rebase --exec 'echo ok' main",
            "git rebase -x 'echo ok' main",
            "git fetch --upload-pack=/tmp/helper origin",
            "git push --receive-pack=/tmp/helper origin main",
            "git merge --strategy=custom topic",
            "git commit",
            "git commit -a",
            "git commit --edit",
            "git commit --gpg-sign=attacker",
            "git --paginate status",
            "git --bare status",
            "git --future-global-option status",
            "git status --future-subcommand-option",
            "git clone --future-clone-option https://example.com/repo.git build/repo",
            "git config --future-config-option --get user.name",
        ] {
            assert_bypass_and_exact_allow_ask(command);
        }
    }

    #[test]
    fn git_closed_option_set_preserves_proven_common_flows() {
        for command in [
            "git --no-pager status",
            "git status --short",
            "git log --oneline -n5",
            "git diff --stat",
            "git add -A",
            "git commit -m message",
            "git clone --depth 1 https://example.com/repo.git build/repo",
            "git config --get user.name",
            "git rev-parse --show-toplevel",
            "git branch --show-current",
            "git ls-files --others --exclude-standard",
        ] {
            assert_bypass_and_exact_allow_allow(command);
        }
    }

    #[test]
    fn test_bypass_mode_allows_only_after_security_floor() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        let result = check_bash_permission("echo hello", &ctx);
        assert!(result.is_allow());
    }

    #[test]
    fn test_bypass_mode_does_not_skip_security_scan() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        let result = check_bash_permission("echo safe\x01hidden", &ctx);
        assert!(result.is_ask());
    }

    #[test]
    fn bypass_mode_blocks_dangerous_removal_paths() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        for command in [
            "rm -rf /",
            "rmdir /etc",
            "/bin/rm -rf /usr",
            "rm -- -/../../etc",
        ] {
            let result = check_bash_permission(command, &ctx);
            assert!(
                result.is_ask(),
                "dangerous removal must remain approval-only: {command} => {result:?}"
            );
        }
    }

    #[test]
    fn bypass_mode_blocks_wrapped_compound_and_indirect_removals() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        for command in [
            "timeout 5 rm -rf /",
            "/usr/bin/timeout 5 rm -rf /",
            "nice -n 5 rm -rf /",
            "nice --adjustment=5 rm -rf /",
            "nohup -- rm -rf /",
            "stdbuf -o0 rm -rf /",
            "env -i rm -rf /",
            "/usr/bin/env -i rm -rf /",
            "echo ok && rm -rf /",
            "find . -delete",
            "xargs rm -rf",
            "busybox rm -rf /",
            "bash -c 'rm -rf /'",
            "sudo rm -rf /",
            "cmd /C echo ok",
            "cmd.exe /C echo ok",
            "'C:\\Windows\\System32\\CMD.EXE' /C echo ok",
            "powershell -Command 'Remove-Item C:\\data'",
            "PowerShell.exe -Command 'Remove-Item C:\\data'",
            "pwsh -Command 'Remove-Item /tmp/data'",
            "pwsh.exe -Command 'Remove-Item /tmp/data'",
            "/opt/PowerShell.EXE -Command 'Remove-Item /tmp/data'",
            "env -- pwsh -Command 'Remove-Item /tmp/data'",
            "FOO=bar pwsh -Command 'Remove-Item /tmp/data'",
            "timeout 5 cmd.exe /C echo ok",
        ] {
            let result = check_bash_permission(command, &ctx);
            assert!(
                result.is_ask(),
                "delegated or compound removal must fail closed: {command} => {result:?}"
            );
        }
    }

    #[test]
    fn bypass_mode_requires_confirmation_for_general_purpose_interpreters() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        for command in [
            "python -c 'open(\".crabcode/settings.json\", \"w\")'",
            "python3 script.py",
            "node -e 'require(\"fs\").rmSync(\"/tmp/x\")'",
            "bun run script.ts",
            "perl mutate.pl",
        ] {
            let result = check_bash_permission(command, &ctx);
            assert!(
                result.is_ask(),
                "uninspectable interpreter must require approval: {command} => {result:?}"
            );
        }
    }

    #[test]
    fn bypass_mode_blocks_sensitive_write_paths() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        for command in [
            "touch .crabcode/settings.json",
            "touch .CrAbCoDe/Settings.Local.JSON",
            "touch .crabcode/agents/reviewer.md",
            "cp source.txt .git/config",
            "sed -i s/old/new/ .crabcode/settings.local.json",
        ] {
            let result = check_bash_permission(command, &ctx);
            assert!(
                result.is_ask(),
                "sensitive write must remain approval-only: {command} => {result:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn bypass_command_floor_covers_custom_config_mutators_and_keeps_safe_flows() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let config = tmp.path().join("custom-config-store");
        std::fs::create_dir_all(workspace.join("build")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        symlink(&config, workspace.join("config-link")).unwrap();
        symlink(&config, workspace.join("config:alias")).unwrap();
        symlink(&config, workspace.join("local:")).unwrap();
        let config_value = config.to_str().unwrap().to_string();
        let _guards = [
            EnvGuard::clear("CRABCODE_STATE_DIR"),
            EnvGuard::clear("CRABCODE_HOME"),
            EnvGuard::clear("CRABCODE_PROFILE"),
            EnvGuard::set("CRABCODE_CONFIG_DIR", &config_value),
        ];
        let resolved_config = acosmi_config::paths::resolve_state_dir();
        let config_path = resolved_config.to_str().unwrap();
        let ctx = PermissionContext {
            cwd: workspace.to_string_lossy().into_owned(),
            original_cwd: None,
            mode: PermissionMode::BypassPermissions,
            rules: vec![],
            tool_name: "Bash".to_string(),
        };

        let sensitive_commands = [
            format!("opaque-mutator '{config_path}/plugins/demo/hooks/hooks.json'"),
            format!("opaque-mutator --output='{config_path}/output-styles/reviewer.md'"),
            "opaque-mutator config-link/templates/report.md".to_string(),
            format!("tee '{config_path}/settings.json'"),
            format!("git clone https://example.com/repo.git '{config_path}/plugins/cache/repo'"),
            "git clone https://example.com/repo.git".to_string(),
            "git config --global core.hooksPath /tmp/untrusted-hooks".to_string(),
            format!("awk 'BEGIN {{ print \"x\" > \"{config_path}/settings.json\" }}'"),
            "cd build && opaque-mutator generated.bin".to_string(),
            format!("printf x > '{config_path}/plugins/demo/hooks/hooks.json'"),
            format!("cp source.txt '{config_path}/output-styles/reviewer.md'"),
            format!("sed -i 's/a/b/' '{config_path}/workflows/release.md'"),
            "install source.txt build/installed.txt".to_string(),
            format!("rsync source.txt '{config_path}/templates/report.md'"),
            format!("dd if=/dev/null of='{config_path}/.credentials.json'"),
            format!("truncate -s 0 '{config_path}/known_marketplaces.json'"),
            format!("ln -s source.txt '{config_path}/plugins/link'"),
            "tar -xf archive.tar -C build".to_string(),
            "unzip archive.zip -d build".to_string(),
            format!("curl -o '{config_path}/settings.local.json' https://example.com/file"),
            format!("wget -O '{config_path}/agents/reviewer.md' https://example.com/file"),
            format!("scp host:file '{config_path}/skills/reviewer/SKILL.md'"),
            "tee 'config:alias/settings.json'".to_string(),
            "opaque-mutator 'local://settings.json'".to_string(),
            format!("ln '{config_path}/settings.json' build/settings-alias.json"),
            format!("git -C '{config_path}' clone https://example.com/repo.git child"),
            format!("git -c alias.x='!tee {config_path}/settings.json' x"),
            format!("git -c core.fsmonitor='!tee {config_path}/settings.json' status"),
            "git -c core.hooksPath=/tmp/untrusted-hooks commit".to_string(),
            "git --config-env=core.fsmonitor=HELPER status".to_string(),
            "git --exec-path=/tmp/untrusted status".to_string(),
            "git diff --ext-diff".to_string(),
            "git status --shor".to_string(),
            "git log --onelin".to_string(),
            "git diff --output=build/diff.txt".to_string(),
            "git add --patch README.md".to_string(),
            "git commit --exec='sh -c id' -m message".to_string(),
            format!("git submodule foreach 'tee {config_path}/settings.json'"),
            format!("awk 'BEGIN {{ print \"x\" | \"tee {config_path}/settings.json\" }}'"),
            format!("gawk 'BEGIN {{ print \"x\" > \"{config_path}/settings.json\" }}'"),
            "mawk 'BEGIN { cmd | getline value }'".to_string(),
            "nawk -f untrusted.awk data.txt".to_string(),
            "gawk --source='BEGIN { system(\"id\") }' data.txt".to_string(),
            "awk 'BEGIN { system (\"id\") }' data.txt".to_string(),
            format!("sed -n -e 'w {config_path}/settings.json' source.txt"),
            "sed -e 's/x/y/e' source.txt".to_string(),
            "sed -f untrusted.sed source.txt".to_string(),
            "tar -t --checkpoint=1 --checkpoint-action=exec='sh -c id' archive.tar".to_string(),
            "tar -t --to-command='sh -c id' archive.tar".to_string(),
            "tar -t -I 'sh -c id' archive.tar".to_string(),
            "tar -t --index-file=build/index.txt archive.tar".to_string(),
            "unzip -lT archive.zip".to_string(),
            format!("curl -K '{config_path}/curl.conf' https://example.com/file"),
            format!("curl -D '{config_path}/settings.json' https://example.com/file"),
            format!("curl -sD '{config_path}/settings.json' https://example.com/file"),
            format!("curl -c '{config_path}/.credentials.json' https://example.com/file"),
            format!("curl --trace='{config_path}/settings.json' https://example.com/file"),
            format!("curl --alt-svc '{config_path}/settings.json' https://example.com/file"),
            format!("curl --output-dir '{config_path}' -O https://example.com/file"),
            format!("curl -sw '%output{{{config_path}/settings.json}}' https://example.com/file"),
            "curl -w @untrusted-format.txt https://example.com/file".to_string(),
            format!(
                "curl --variable target='{config_path}/settings.json' --expand-output '{{{{target}}}}' https://example.com/file"
            ),
            format!(
                "curl --variable target='{config_path}/settings.json' --expand-output='{{{{target}}}}' https://example.com/file"
            ),
            format!(
                "curl --variable target='{config_path}/settings.json' --expand-write-out '%output{{{{{{target}}}}}}' https://example.com/file"
            ),
            format!(
                "curl --variable target='{config_path}/settings.json' --expand-write-out='%output{{{{{{target}}}}}}' https://example.com/file"
            ),
            format!(
                "wget --spider --output-file='{config_path}/settings.json' https://example.com"
            ),
            format!(
                "wget --spider --save-cookies='{config_path}/.credentials.json' https://example.com"
            ),
            format!("wget --spider -qo '{config_path}/settings.json' https://example.com"),
            format!("wget --spider --hsts-file='{config_path}/settings.json' https://example.com"),
            "rsync -e 'sh -c id' source.txt build/copied.txt".to_string(),
            "rsync --rsync-path='sh -c id' source.txt build/copied.txt".to_string(),
            format!("rsync --link-dest='{config_path}' source.txt build/copied.txt"),
            "scp -S /tmp/untrusted-ssh host:file build/copied.txt".to_string(),
            "scp -o ProxyCommand='sh -c id' host:file build/copied.txt".to_string(),
            "scp -J jump.example host:file build/copied.txt".to_string(),
            "scp -T host:file build/copied.txt".to_string(),
            "rg --pre 'sh -c id' needle .".to_string(),
        ];
        for command in sensitive_commands {
            let result = check_bash_permission(&command, &ctx);
            assert!(
                result.is_ask(),
                "sensitive or uninspectable mutator must ask: {command} => {result:?}"
            );
        }

        let safe_commands = [
            "echo ordinary".to_string(),
            "npm install".to_string(),
            "touch build/generated.txt".to_string(),
            format!("tee '{config_path}/plans/session-plan.md'"),
            format!("tee '{config_path}/scratchpad/notes.txt'"),
            format!("echo '{config_path}/settings.json'"),
            format!("git commit -m '{config_path}/settings.json'"),
            format!("git commit --message='{config_path}/settings.json'"),
            "awk '$1 > 3 { print $1 }' data.txt".to_string(),
            "opaque-mutator https://example.com/.crabcode/settings.json".to_string(),
            "curl https://example.com/file".to_string(),
            "wget --spider https://example.com/file".to_string(),
            "tar -tf archive.tar".to_string(),
            "unzip -l archive.zip".to_string(),
            "git clone https://example.com/repo.git build/repo".to_string(),
            "tee --version".to_string(),
            "tee value:ordinary".to_string(),
            "ln -s source.txt build/source-link".to_string(),
            "rsync -av source.txt build/copied.txt".to_string(),
            "scp -q host:file build/copied.txt".to_string(),
            "curl -D build/headers.txt https://example.com/file".to_string(),
            "curl -w '%{http_code}' https://example.com/file".to_string(),
            "wget --spider --timeout=2 https://example.com/file".to_string(),
            "sed -n -e p source.txt".to_string(),
            "sed 's/old/new/g' source.txt".to_string(),
            "git status".to_string(),
            "git status --short".to_string(),
            "git log --oneline -n5".to_string(),
            "git diff --stat".to_string(),
            "git add -A".to_string(),
            "git commit -m message".to_string(),
        ];
        for command in safe_commands {
            let result = check_bash_permission(&command, &ctx);
            assert!(
                result.is_allow(),
                "ordinary safe bypass flow should remain usable: {command} => {result:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn bypass_mode_rechecks_workspace_symlinks_but_keeps_inside_links_usable() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let inside = workspace.join("generated");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&inside, workspace.join("inside-link")).unwrap();
        symlink(&outside, workspace.join("escape-link")).unwrap();

        let ctx = PermissionContext {
            cwd: workspace.to_string_lossy().into_owned(),
            original_cwd: None,
            mode: PermissionMode::BypassPermissions,
            rules: vec![],
            tool_name: "Bash".to_string(),
        };
        assert!(check_bash_permission("touch inside-link/new.txt", &ctx).is_allow());
        assert!(check_bash_permission("touch escape-link/new.txt", &ctx).is_ask());
    }

    #[test]
    fn explicit_allow_rule_does_not_override_path_floor() {
        let ctx = make_context(
            PermissionMode::BypassPermissions,
            vec![make_rule(PermissionBehavior::Allow, "Bash", Some("rm *"))],
        );
        assert!(check_bash_permission("rm -rf /", &ctx).is_ask());
    }

    #[test]
    fn bypass_mode_still_allows_ordinary_safe_commands_and_paths() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        for command in [
            "echo hello",
            "npm install",
            "touch build/generated.txt",
            "rm build/stale.txt",
            "git commit -m 'safe message'",
            "echo ok >/dev/null",
            "python3 --version",
            "node --version",
        ] {
            let result = check_bash_permission(command, &ctx);
            assert!(
                result.is_allow(),
                "ordinary command should still bypass routine approval: {command} => {result:?}"
            );
        }
    }

    #[test]
    fn test_bypass_mode_does_not_skip_explicit_deny_rule() {
        let ctx = make_context(
            PermissionMode::BypassPermissions,
            vec![make_rule(
                PermissionBehavior::Deny,
                "Bash",
                Some("git push *"),
            )],
        );
        let result = check_bash_permission("git push origin main", &ctx);
        assert!(result.is_deny());
    }

    #[test]
    fn test_bypass_mode_does_not_skip_explicit_ask_rule() {
        let ctx = make_context(
            PermissionMode::BypassPermissions,
            vec![make_rule(
                PermissionBehavior::Ask,
                "Bash",
                Some("npm publish *"),
            )],
        );
        let result = check_bash_permission("npm publish --access public", &ctx);
        assert!(result.is_ask());
    }

    #[test]
    fn test_powershell_bypass_mode_does_not_skip_explicit_rules() {
        let denied = make_context(
            PermissionMode::BypassPermissions,
            vec![make_rule(
                PermissionBehavior::Deny,
                "PowerShell",
                Some("Remove-Item *"),
            )],
        );
        assert!(check_powershell_permission("Remove-Item C:\\data", &denied).is_deny());

        let asked = make_context(
            PermissionMode::BypassPermissions,
            vec![make_rule(
                PermissionBehavior::Ask,
                "PowerShell",
                Some("Publish-Module *"),
            )],
        );
        assert!(check_powershell_permission("Publish-Module Demo", &asked).is_ask());
    }

    #[test]
    fn test_dontask_mode() {
        let ctx = make_context(PermissionMode::DontAsk, vec![]);
        let result = check_bash_permission("echo hello", &ctx);
        assert!(result.is_deny());
    }

    #[test]
    fn test_empty_command() {
        let ctx = make_context(PermissionMode::Default, vec![]);
        let result = check_bash_permission("", &ctx);
        assert!(result.is_deny());
    }

    #[test]
    fn test_deny_rule_wins() {
        let ctx = make_context(
            PermissionMode::Default,
            vec![
                make_rule(PermissionBehavior::Allow, "Bash", Some("git *")),
                make_rule(PermissionBehavior::Deny, "Bash", Some("git push *")),
            ],
        );
        let result = check_bash_permission("git push --force", &ctx);
        assert!(result.is_deny());
    }

    #[test]
    fn test_allow_rule_with_safety() {
        let ctx = make_context(
            PermissionMode::Default,
            vec![make_rule(PermissionBehavior::Allow, "Bash", Some("echo *"))],
        );
        // echo hello 应该被规则允许（且通过安全扫描）
        let result = check_bash_permission("echo hello", &ctx);
        assert!(result.is_allow());
    }

    #[test]
    fn test_readonly_auto_allow() {
        let ctx = make_context(PermissionMode::Default, vec![]);
        let result = check_bash_permission("cat file.txt", &ctx);
        assert!(result.is_allow());
    }

    #[test]
    fn test_non_readonly_asks() {
        let ctx = make_context(PermissionMode::Default, vec![]);
        let result = check_bash_permission("npm install", &ctx);
        assert!(result.is_ask());
    }

    #[test]
    fn test_match_command_against_rules_deny_priority() {
        let rules = vec![
            make_rule(PermissionBehavior::Allow, "Bash", Some("git *")),
            make_rule(PermissionBehavior::Deny, "Bash", Some("git *")),
        ];
        let result = match_command_against_rules("git push", &rules, "Bash");
        assert!(result.is_some());
        let (behavior, _) = result.unwrap();
        assert_eq!(behavior, PermissionBehavior::Deny);
    }

    #[test]
    fn test_match_command_against_rules_ask_priority_over_allow() {
        let rules = vec![
            make_rule(PermissionBehavior::Allow, "Bash", Some("git *")),
            make_rule(PermissionBehavior::Ask, "Bash", Some("git *")),
        ];
        let result = match_command_against_rules("git status", &rules, "Bash")
            .expect("both rules should match");
        assert_eq!(result.0, PermissionBehavior::Ask);
    }

    #[test]
    fn test_match_command_against_rules_deny_priority_over_ask_and_allow() {
        let rules = vec![
            make_rule(PermissionBehavior::Allow, "Bash", Some("git *")),
            make_rule(PermissionBehavior::Ask, "Bash", Some("git *")),
            make_rule(PermissionBehavior::Deny, "Bash", Some("git *")),
        ];
        let result = match_command_against_rules("git status", &rules, "Bash")
            .expect("all rules should match");
        assert_eq!(result.0, PermissionBehavior::Deny);
    }

    #[test]
    fn explicit_ask_rule_is_not_bypassed_by_readonly_auto_allow() {
        let rules = vec![
            make_rule(PermissionBehavior::Allow, "Bash", Some("git *")),
            make_rule(PermissionBehavior::Ask, "Bash", Some("git *")),
        ];
        let bash_ctx = make_context(PermissionMode::Default, rules.clone());
        assert!(check_bash_permission("git status", &bash_ctx).is_ask());

        let powershell_ctx = PermissionContext {
            cwd: "/home/user".to_string(),
            original_cwd: None,
            mode: PermissionMode::Default,
            rules: vec![
                make_rule(PermissionBehavior::Allow, "PowerShell", Some("git *")),
                make_rule(PermissionBehavior::Ask, "PowerShell", Some("git *")),
            ],
            tool_name: "PowerShell".to_string(),
        };
        assert!(check_powershell_permission("git status", &powershell_ctx).is_ask());
    }

    #[test]
    fn restrictive_rules_match_through_env_assignments_but_allow_does_not() {
        for wrapped in [
            "ACOSMI_API_KEY=test-key-not-real git status",
            "FOO=a=b git status",
            "FOO='a b' git status",
        ] {
            let deny = vec![make_rule(
                PermissionBehavior::Deny,
                "Bash",
                Some("git status"),
            )];
            assert_eq!(
                match_command_against_rules(wrapped, &deny, "Bash")
                    .expect("deny must see through an environment assignment")
                    .0,
                PermissionBehavior::Deny
            );

            let ask = vec![make_rule(
                PermissionBehavior::Ask,
                "Bash",
                Some("git status"),
            )];
            assert_eq!(
                match_command_against_rules(wrapped, &ask, "Bash")
                    .expect("ask must see through an environment assignment")
                    .0,
                PermissionBehavior::Ask
            );

            let allow = vec![make_rule(
                PermissionBehavior::Allow,
                "Bash",
                Some("git status"),
            )];
            assert!(
                match_command_against_rules(wrapped, &allow, "Bash").is_none(),
                "allow must not be borrowed through an environment assignment"
            );
        }

        let deny = vec![make_rule(
            PermissionBehavior::Deny,
            "Bash",
            Some("git status"),
        )];
        for expansion in [
            "FOO=$(touch /tmp/never-run) git status",
            "ARR[$(touch /tmp/never-run)]=value git status",
        ] {
            assert!(
                match_command_against_rules(expansion, &deny, "Bash").is_none(),
                "expanding assignments must not be normalized for rule matching"
            );
        }
    }

    #[test]
    fn environment_dump_and_env_exec_require_approval() {
        let ctx = make_context(PermissionMode::Default, vec![]);
        for command in [
            "env",
            "printenv",
            "env rm -rf /",
            "env bash -lc 'echo x'",
            "env sh -c 'echo x'",
        ] {
            let result = check_bash_permission(command, &ctx);
            assert!(
                result.is_ask(),
                "{command} must reach an explicit approval decision, got {result:?}"
            );
        }
    }

    #[test]
    fn test_match_command_tool_filter() {
        let rules = vec![make_rule(
            PermissionBehavior::Allow,
            "PowerShell",
            Some("git *"),
        )];
        // Bash 工具不匹配 PowerShell 规则
        let result = match_command_against_rules("git push", &rules, "Bash");
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_command_prefix() {
        assert_eq!(extract_command_prefix("git commit -m 'msg'"), "git");
        assert_eq!(extract_command_prefix("npm install"), "npm");
        assert_eq!(extract_command_prefix("sudo apt-get install"), "apt-get");
        assert_eq!(extract_command_prefix("env -i bash"), "bash");
    }

    #[test]
    fn test_powershell_permission_bypass() {
        let ctx = PermissionContext {
            cwd: "/home/user".to_string(),
            original_cwd: None,
            mode: PermissionMode::BypassPermissions,
            rules: vec![],
            tool_name: "PowerShell".to_string(),
        };
        let result = check_powershell_permission("Remove-Item -Recurse", &ctx);
        assert!(result.is_allow());
    }

    #[test]
    fn test_powershell_permission_deny_rule() {
        let ctx = PermissionContext {
            cwd: "/home/user".to_string(),
            original_cwd: None,
            mode: PermissionMode::Default,
            rules: vec![make_rule(
                PermissionBehavior::Deny,
                "PowerShell",
                Some("Remove-Item *"),
            )],
            tool_name: "PowerShell".to_string(),
        };
        let result = check_powershell_permission("Remove-Item -Recurse .", &ctx);
        assert!(result.is_deny());
    }

    #[test]
    fn test_plan_mode() {
        let ctx = make_context(PermissionMode::Plan, vec![]);
        // Even non-dangerous commands are denied in plan mode (unless readonly)
        let result = check_bash_permission("npm install", &ctx);
        assert!(result.is_deny());
    }

    #[test]
    fn test_plan_mode_allows_readonly() {
        let ctx = make_context(PermissionMode::Plan, vec![]);
        let result = check_bash_permission("cat file.txt", &ctx);
        // Readonly commands pass even in plan mode
        assert!(result.is_allow());
    }

    #[test]
    fn test_rule_no_content_matches_all() {
        let rules = vec![make_rule(PermissionBehavior::Allow, "Bash", None)];
        let result = match_command_against_rules("anything here", &rules, "Bash");
        assert!(result.is_some());
        let (behavior, _) = result.unwrap();
        assert_eq!(behavior, PermissionBehavior::Allow);
    }

    #[test]
    fn test_max_subcommands_constant() {
        assert_eq!(MAX_SUBCOMMANDS_FOR_SECURITY_CHECK, 50);
    }
}
