import type {
  PermissionRuleSource,
  ToolPermissionRulesBySource,
} from '../types/permissions.js'

/**
 * Choose command-scoped permission rules for one QueryEngine submission.
 * Private in-process workflow rules intentionally override the empty rules
 * produced by plain-text input parsing; ordinary callers preserve the parsed
 * slash-command result byte-for-byte.
 */
export function selectTurnCommandAllowRules(
  parsedCommandRules: string[] | undefined,
  privateCommandRules: readonly string[] | undefined,
): string[] | undefined {
  return privateCommandRules === undefined
    ? parsedCommandRules
    : [...privateCommandRules]
}

/**
 * Apply one turn's command rules. A defined private scope replaces every
 * ambient allow source (CLI, session, user, project command); otherwise the
 * ordinary merge behavior is preserved. Deny/ask rules live in separate maps
 * and remain untouched by this helper.
 */
export function selectTurnAlwaysAllowRules(
  existing: Partial<Record<PermissionRuleSource, readonly string[]>>,
  effectiveCommandRules: readonly string[] | undefined,
  privateCommandRules: readonly string[] | undefined,
): ToolPermissionRulesBySource {
  if (privateCommandRules !== undefined) {
    return { command: effectiveCommandRules ? [...effectiveCommandRules] : [] }
  }
  const copied: ToolPermissionRulesBySource = {}
  for (const [source, rules] of Object.entries(existing)) {
    if (rules) copied[source as PermissionRuleSource] = [...rules]
  }
  copied.command = effectiveCommandRules
    ? [...effectiveCommandRules]
    : undefined
  return copied
}
