export const AGENT_TOOL_NAME = 'Agent'
// Legacy wire name for backward compat (permission rules, hooks, resumed sessions)
export const LEGACY_AGENT_TOOL_NAME = 'Task'
export const VERIFICATION_AGENT_TYPE = 'verification'

/**
 * Custom/plugin agents may delegate only through an explicit
 * `Agent(type-a, type-b)` frontmatter allowlist. Bound the resulting chain so
 * a pair of mutually-referential third-party agents cannot recurse forever.
 *
 * Depth 2 permits the supported shape main → worker → read-only explorer.
 */
export const MAX_NESTED_AGENT_DEPTH = 2

// Built-in agents that run once and return a report — the parent never
// SendMessages back to continue them. Skip the agentId/SendMessage/usage
// trailer for these to save tokens (~135 chars × 34M Explore runs/week).
export const ONE_SHOT_BUILTIN_AGENT_TYPES: ReadonlySet<string> = new Set([
  'Explore',
  'Plan',
])
