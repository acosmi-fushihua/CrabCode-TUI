/**
 * hooks.ts -- Backward-compatible re-export barrel.
 *
 * This file was a 5022-line monolith. It has been split into:
 *   - hooks/hooks-types.ts     (type definitions, interfaces, constants, message formatters)
 *   - hooks/hooks-matching.ts  (hook matching, pattern resolution, config assembly, trust checks)
 *   - hooks/hooks-executor.ts  (command execution, output parsing, JSON validation, orchestration)
 *   - hooks/hooks-events.ts    (event-specific hook entry points for each lifecycle event)
 *
 * All public exports are re-exported here so that downstream imports remain unchanged.
 */

// ---------------------------------------------------------------------------
// hooks/hooks-types.ts -- types, interfaces, constants, message formatters
// ---------------------------------------------------------------------------
export {
  TOOL_HOOK_EXECUTION_TIMEOUT_MS,
  getSessionEndHookTimeoutMs,
  type HookBlockingError,
  type ElicitationResponse,
  type HookResult,
  type AggregatedHookResult,
  type HookOutsideReplResult,
  hasBlockingResult,
  type ConfigChangeSource,
  type InstructionsLoadReason,
  type InstructionsMemoryType,
  type ElicitationHookResult,
  type ElicitationResultHookResult,
  getPreToolHookBlockingMessage,
  getStopHookMessage,
  getTeammateIdleHookMessage,
  getTaskCreatedHookMessage,
  getTaskCompletedHookMessage,
  getUserPromptSubmitHookBlockingMessage,
} from './hooks/hooks-types.js'

// ---------------------------------------------------------------------------
// hooks/hooks-matching.ts -- matching, trust, base input, config assembly
// ---------------------------------------------------------------------------
export {
  shouldSkipHookDueToTrust,
  createBaseHookInput,
  getMatchingHooks,
} from './hooks/hooks-matching.js'

// ---------------------------------------------------------------------------
// hooks/hooks-events.ts -- event-specific hook entry points
// ---------------------------------------------------------------------------
export {
  executePreToolHooks,
  executePostToolHooks,
  executePostToolUseFailureHooks,
  executePermissionDeniedHooks,
  executeNotificationHooks,
  executeStopFailureHooks,
  executeStopHooks,
  executeTeammateIdleHooks,
  executeTaskCreatedHooks,
  executeTaskCompletedHooks,
  executeUserPromptSubmitHooks,
  executeSessionStartHooks,
  executeSetupHooks,
  executeSubagentStartHooks,
  executePreCompactHooks,
  executePostCompactHooks,
  executeSessionEndHooks,
  executePermissionRequestHooks,
  executeConfigChangeHooks,
  executeCwdChangedHooks,
  executeFileChangedHooks,
  hasInstructionsLoadedHook,
  executeInstructionsLoadedHooks,
  executeElicitationHooks,
  executeElicitationResultHooks,
  executeStatusLineCommand,
  executeFileSuggestionCommand,
  hasWorktreeCreateHook,
  executeWorktreeCreateHook,
  executeWorktreeRemoveHook,
} from './hooks/hooks-events.js'
