import {
  getIsInteractive,
  getUsesStructuredIoTransport,
} from '../bootstrap/state.js'
import type { PermissionDecision } from '../utils/permissions/PermissionResult.js'

/**
 * The native TUI renderer owns permission interaction over its private stdio
 * bridge. This is a route invariant, not a user-selectable prompt tool.
 */
export const DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME = 'stdio' as const

export function withDirectTuiPermissionBridge<T extends object>(
  options: T,
): T & { permissionPromptToolName: typeof DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME } {
  return {
    ...options,
    permissionPromptToolName: DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME,
  }
}

export const UNRESOLVED_DIRECT_TUI_PERMISSION_MESSAGE =
  'Native TUI permission bridge invariant violated: unresolved ask reached tool execution'

type PermissionBridgeRuntimeState = {
  interactive: boolean
  structuredIo: boolean
}

/**
 * An interactive StructuredIO session is the native TUI route. Its
 * canUseTool bridge must resolve every ask to allow/deny before tool
 * execution. Throwing here fails closed and prevents an unresolved prompt
 * from being converted into a normal tool error that the model may retry.
 */
export function assertInteractivePermissionDecisionResolved(
  decision: PermissionDecision,
  runtimeState: PermissionBridgeRuntimeState = {
    interactive: getIsInteractive(),
    structuredIo: getUsesStructuredIoTransport(),
  },
): void {
  if (
    decision.behavior === 'ask' &&
    runtimeState.interactive &&
    runtimeState.structuredIo
  ) {
    throw new Error(UNRESOLVED_DIRECT_TUI_PERMISSION_MESSAGE)
  }
}
