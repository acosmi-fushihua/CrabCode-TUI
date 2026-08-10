export const WORKFLOW_TOOL_NAME = 'Workflow'
export const LEGACY_WORKFLOW_TOOL_NAME = 'WorkflowTool'
// The security workflow can panel 45 findings with three voters (135 jobs) in
// one bounded fan-out, so 100 silently truncates a valid upstream shape.
export const WORKFLOW_MAX_STEPS = 512
export const WORKFLOW_MAX_AGENT_CALLS = 1024
/** Total wall-clock budget for one direct workflow run. */
export const WORKFLOW_MAX_RUNTIME_MS = 2 * 60 * 60 * 1000
/** Grace period for in-flight agents to finish before a partial result returns. */
export const WORKFLOW_DRAIN_GRACE_MS = 60_000
/** Maximum event-level silence for one unattended workflow agent. */
export const WORKFLOW_AGENT_IDLE_TIMEOUT_MS = 600_000
/** Largest delay representable by Bun/Node timers without overflow-to-1ms. */
export const WORKFLOW_AGENT_IDLE_TIMEOUT_MAX_MS = 2_147_483_647

export function parseWorkflowAgentIdleTimeoutMs(
  raw: string | undefined | null,
): number | null {
  if (raw === undefined || raw === null) return WORKFLOW_AGENT_IDLE_TIMEOUT_MS
  const trimmed = String(raw).trim()
  if (trimmed.length === 0 || !/^\d+$/.test(trimmed)) {
    return WORKFLOW_AGENT_IDLE_TIMEOUT_MS
  }
  const parsed = Number.parseInt(trimmed, 10)
  if (
    !Number.isSafeInteger(parsed) ||
    parsed > WORKFLOW_AGENT_IDLE_TIMEOUT_MAX_MS
  ) {
    return WORKFLOW_AGENT_IDLE_TIMEOUT_MS
  }
  return parsed === 0 ? null : parsed
}
export const WORKFLOW_HEARTBEAT_TIMEOUT_MS = 3_000
export const WORKFLOW_MAX_ARGS_BYTES = 1_000_000
export const WORKFLOW_MAX_AGENT_PROMPT_BYTES = 200_000
export const WORKFLOW_MAX_AGENT_SCHEMA_BYTES = 500_000
export const WORKFLOW_MAX_RESULT_BYTES = 2_000_000
export const WORKFLOW_MAX_SOURCE_BYTES = 1_000_000
export const WORKFLOW_AGENT_DRAIN_TIMEOUT_MS = 10_000
export const WORKFLOW_WORKER_RESOURCE_LIMITS = Object.freeze({
  maxOldGenerationSizeMb: 256,
  maxYoungGenerationSizeMb: 32,
  codeRangeSizeMb: 32,
  stackSizeMb: 8,
})
