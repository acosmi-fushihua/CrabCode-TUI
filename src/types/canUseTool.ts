import type {
  Tool as ToolType,
  ToolUseContext,
} from '../Tool.js'
import type { AssistantMessage } from './message.js'
import type { PermissionDecision } from '../utils/permissions/PermissionResult.js'

/**
 * Backend permission callback contract.
 *
 * This type belongs to the query/tool execution boundary. It must not depend
 * on any particular terminal renderer.
 */
export type CanUseToolFn<
  Input extends Record<string, unknown> = Record<string, unknown>,
> = (
  tool: ToolType,
  input: Input,
  toolUseContext: ToolUseContext,
  assistantMessage: AssistantMessage,
  toolUseID: string,
  forceDecision?: PermissionDecision<Input>,
) => Promise<PermissionDecision<Input>>
