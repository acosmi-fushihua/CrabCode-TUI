//! 权限决策引擎
//!
//! 等价于 TS bashPermissions.ts / powershellPermissions.ts 中的主决策逻辑
//! 负责：规则匹配 → 安全扫描 → 只读检查 → 最终决策

use crate::constants;
use crate::readonly_validator;
use crate::rule_matching::{matches_rule, parse_shell_rule};
use crate::security_scanner::{self, BashSecurityResult};
use crate::types::{
    PermissionBehavior, PermissionContext, PermissionDecisionReason, PermissionMode,
    PermissionResult, PermissionRule, PermissionRuleSource, PermissionRuleValue, PermissionUpdate,
};
use std::sync::OnceLock;

/// 安全检查子命令上限
pub const MAX_SUBCOMMANDS_FOR_SECURITY_CHECK: usize = 50;

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

    // 1. BypassPermissions 模式直接允许
    if ctx.mode == PermissionMode::BypassPermissions {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::Mode {
            mode: PermissionMode::BypassPermissions,
        });
    }

    // 2. DontAsk 模式直接拒绝
    if ctx.mode == PermissionMode::DontAsk {
        return PermissionResult::deny(
            "Permission denied (DontAsk mode)",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::DontAsk,
            },
        );
    }

    // 3. 规则匹配（deny → ask → allow）
    if let Some((behavior, rule)) = match_command_against_rules(trimmed, &ctx.rules, "Bash") {
        match behavior {
            PermissionBehavior::Deny => {
                return PermissionResult::deny(
                    format!("Denied by rule: {:?}", rule.value),
                    PermissionDecisionReason::Rule { rule: rule.clone() },
                );
            }
            PermissionBehavior::Allow => {
                // 即使规则允许，仍需安全扫描
                match security_scanner::bash_security_check(trimmed) {
                    BashSecurityResult::Ask(violation) => {
                        return PermissionResult::Ask {
                            message: format!(
                                "Rule allows but security check failed:\n{}",
                                violation.message
                            ),
                            suggestions: vec![],
                            blocked_path: None,
                        };
                    }
                    _ => {
                        return PermissionResult::allow_with_reason(
                            PermissionDecisionReason::Rule { rule: rule.clone() },
                        );
                    }
                }
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

    // 4. 安全扫描
    match security_scanner::bash_security_check(trimmed) {
        BashSecurityResult::Allow => {
            // 早期允许路径（如 git commit -m "safe"）
            return PermissionResult::allow_with_reason(PermissionDecisionReason::SafetyCheck {
                reason: "Safe command (early allow)".to_string(),
                classifier_approvable: true,
            });
        }
        BashSecurityResult::Ask(violation) => {
            return PermissionResult::Ask {
                message: format!("Security check: {}", violation.message),
                suggestions: build_bash_suggestions(trimmed),
                blocked_path: None,
            };
        }
        BashSecurityResult::Passthrough => {
            // 继续后续检查
        }
    }

    // 5. 只读命令检查
    if readonly_validator::is_command_readonly(trimmed) {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::SafetyCheck {
            reason: "Command is readonly".to_string(),
            classifier_approvable: true,
        });
    }

    // 6. Plan 模式 — 不执行
    if ctx.mode == PermissionMode::Plan {
        return PermissionResult::deny(
            "Plan mode: command would be executed",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::Plan,
            },
        );
    }

    // 7. 默认 Ask
    PermissionResult::Ask {
        message: format!("Allow bash command: {trimmed}"),
        suggestions: build_bash_suggestions(trimmed),
        blocked_path: None,
    }
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

    // 1. BypassPermissions 模式
    if ctx.mode == PermissionMode::BypassPermissions {
        return PermissionResult::allow_with_reason(PermissionDecisionReason::Mode {
            mode: PermissionMode::BypassPermissions,
        });
    }

    // 2. DontAsk 模式
    if ctx.mode == PermissionMode::DontAsk {
        return PermissionResult::deny(
            "Permission denied (DontAsk mode)",
            PermissionDecisionReason::Mode {
                mode: PermissionMode::DontAsk,
            },
        );
    }

    // 3. 规则匹配
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

    #[test]
    fn test_bypass_mode() {
        let ctx = make_context(PermissionMode::BypassPermissions, vec![]);
        let result = check_bash_permission("rm -rf /", &ctx);
        assert!(result.is_allow());
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
