export const WORKFLOW_TOOL_NAME = 'Workflow'
export const LEGACY_WORKFLOW_TOOL_NAME = 'WorkflowTool'
// The security workflow can panel 45 findings with three voters (135 jobs) in
// one bounded fan-out, so 100 silently truncates a valid upstream shape.
export const WORKFLOW_MAX_STEPS = 512
export const WORKFLOW_MAX_AGENT_CALLS = 1024
export const WORKFLOW_MAX_RUNTIME_MS = 24 * 60 * 60 * 1000
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
