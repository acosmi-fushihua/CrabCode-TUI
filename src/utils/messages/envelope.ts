/**
 * envelope.ts -- Three-language shared: sentinel constants, message predicates, ID derivation.
 *
 * This module is the future codegen target for Go/Rust. It contains only
 * pure functions and constants with no UI or API-pipeline dependencies.
 */

import type {
  BetaContentBlock,
  BetaToolUseBlock,
  ContentBlockParam,
  ToolUseBlock,
} from '../../types/api-types.js'
import type { UUID } from 'crypto'

import type {
  AssistantMessage,
  Message,
  NormalizedAssistantMessage,
  NormalizedMessage,
  NormalizedUserMessage,
} from '../../types/message.js'
import { NO_CONTENT_MESSAGE } from '../../constants/messages.js'

// ---------------------------------------------------------------------------
// Sentinel constants
// ---------------------------------------------------------------------------

export const INTERRUPT_MESSAGE = '[Request interrupted by user]'
export const INTERRUPT_MESSAGE_FOR_TOOL_USE =
  '[Request interrupted by user for tool use]'
export const CANCEL_MESSAGE =
  "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed."
export const REJECT_MESSAGE =
  "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed."
export const REJECT_MESSAGE_WITH_REASON_PREFIX =
  "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\n"
export const SUBAGENT_REJECT_MESSAGE =
  'Permission for this tool use was denied. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). Try a different approach or report the limitation to complete your task.'
export const SUBAGENT_REJECT_MESSAGE_WITH_REASON_PREFIX =
  'Permission for this tool use was denied. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). The user said:\n'
export const PLAN_REJECTION_PREFIX =
  'The agent proposed a plan that was rejected by the user. The user chose to stay in plan mode rather than proceed with implementation.\n\nRejected plan:\n'

/**
 * Shared guidance for permission denials, instructing the model on appropriate workarounds.
 */
export const DENIAL_WORKAROUND_GUIDANCE =
  `IMPORTANT: You *may* attempt to accomplish this action using other tools that might naturally be used to accomplish this goal, ` +
  `e.g. using head instead of cat. But you *should not* attempt to work around this denial in malicious ways, ` +
  `e.g. do not use your ability to run tests to execute non-test actions. ` +
  `You should only try to work around this restriction in reasonable ways that do not attempt to bypass the intent behind this denial. ` +
  `If you believe this capability is essential to complete the user's request, STOP and explain to the user ` +
  `what you were trying to do and why you need this permission. Let the user decide how to proceed.`

export function AUTO_REJECT_MESSAGE(toolName: string): string {
  return `Permission to use ${toolName} has been denied. ${DENIAL_WORKAROUND_GUIDANCE}`
}
export function DONT_ASK_REJECT_MESSAGE(toolName: string): string {
  return `Permission to use ${toolName} has been denied because CrabCode is running in don't ask mode. ${DENIAL_WORKAROUND_GUIDANCE}`
}
export const NO_RESPONSE_REQUESTED = 'No response requested.'

// Synthetic tool_result content inserted by ensureToolResultPairing when a
// tool_use block has no matching tool_result. Exported so HFI submission can
// reject any payload containing it -- placeholder satisfies pairing structurally
// but the content is fake, which poisons training data if submitted.
export const SYNTHETIC_TOOL_RESULT_PLACEHOLDER =
  '[Tool result missing due to internal error]'

export const SYNTHETIC_MODEL = '<synthetic>'

export const SYNTHETIC_MESSAGES = new Set([
  INTERRUPT_MESSAGE,
  INTERRUPT_MESSAGE_FOR_TOOL_USE,
  CANCEL_MESSAGE,
  REJECT_MESSAGE,
  NO_RESPONSE_REQUESTED,
])

// ---------------------------------------------------------------------------
// Message predicates
// ---------------------------------------------------------------------------

export function isSyntheticMessage(message: Message): boolean {
  return (
    message.type !== 'progress' &&
    message.type !== 'attachment' &&
    message.type !== 'system' &&
    message.type !== 'stream_event' &&
    message.type !== 'stream_request_start' &&
    message.type !== 'tombstone' &&
    message.type !== 'tool_use_summary' &&
    Array.isArray(message.message.content) &&
    message.message.content[0]?.type === 'text' &&
    SYNTHETIC_MESSAGES.has(message.message.content[0].text)
  )
}

export function isNotEmptyMessage(message: Message): boolean {
  if (
    message.type === 'progress' ||
    message.type === 'attachment' ||
    message.type === 'system' ||
    message.type === 'stream_event' ||
    message.type === 'stream_request_start' ||
    message.type === 'tombstone' ||
    message.type === 'tool_use_summary'
  ) {
    return true
  }

  if (typeof message.message.content === 'string') {
    return message.message.content.trim().length > 0
  }

  if (message.message.content.length === 0) {
    return false
  }

  // Skip multi-block messages for now
  if (message.message.content.length > 1) {
    return true
  }

  if (message.message.content[0]!.type !== 'text') {
    return true
  }

  return (
    message.message.content[0]!.text.trim().length > 0 &&
    message.message.content[0]!.text !== NO_CONTENT_MESSAGE &&
    message.message.content[0]!.text !== INTERRUPT_MESSAGE_FOR_TOOL_USE
  )
}

type ToolUseRequestMessage = NormalizedAssistantMessage & {
  message: { content: [ToolUseBlock] }
}

export function isToolUseRequestMessage(
  message: Message,
): message is ToolUseRequestMessage {
  return (
    message.type === 'assistant' &&
    // Note: stop_reason === 'tool_use' is unreliable -- it's not always set correctly
    message.message.content.some(_ => _.type === 'tool_use')
  )
}

type ToolUseResultMessage = NormalizedUserMessage & {
  message: { content: [import('../../types/api-types.js').ToolResultBlockParam] }
}

export function isToolUseResultMessage(
  message: Message,
): message is ToolUseResultMessage {
  return (
    message.type === 'user' &&
    ((Array.isArray(message.message.content) &&
      message.message.content[0]?.type === 'tool_result') ||
      Boolean(message.toolUseResult))
  )
}

export function isCompactBoundaryMessage(
  message: Message | NormalizedMessage,
): message is import('../../types/message.js').SystemCompactBoundaryMessage {
  return message?.type === 'system' && message.subtype === 'compact_boundary'
}

export function isThinkingMessage(message: Message): boolean {
  if (message.type !== 'assistant') return false
  if (!Array.isArray(message.message.content)) return false
  return message.message.content.every(
    block => block.type === 'thinking' || block.type === 'redacted_thinking',
  )
}

export function getLastAssistantMessage(
  messages: Message[],
): AssistantMessage | undefined {
  // findLast exits early from the end -- much faster than filter + last for
  // large message arrays (called on every REPL render via useFeedbackSurvey).
  return messages.findLast(
    (msg): msg is AssistantMessage => msg.type === 'assistant',
  )
}

export function hasToolCallsInLastAssistantTurn(messages: Message[]): boolean {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i]
    if (message && message.type === 'assistant') {
      const assistantMessage = message as AssistantMessage
      const content = assistantMessage.message.content
      if (Array.isArray(content)) {
        return content.some(block => block.type === 'tool_use')
      }
    }
  }
  return false
}

/**
 * Count total calls to a specific tool in message history
 * Stops early at maxCount for efficiency
 */
export function countToolCalls(
  messages: Message[],
  toolName: string,
  maxCount?: number,
): number {
  let count = 0
  for (const msg of messages) {
    if (!msg) continue
    if (msg.type === 'assistant' && Array.isArray(msg.message.content)) {
      const hasToolUse = msg.message.content.some(
        (block): block is ToolUseBlock =>
          block.type === 'tool_use' && block.name === toolName,
      )
      if (hasToolUse) {
        count++
        if (maxCount && count >= maxCount) {
          return count
        }
      }
    }
  }
  return count
}

/**
 * Check if the most recent tool call succeeded (has result without is_error)
 * Searches backwards for efficiency.
 */
export function hasSuccessfulToolCall(
  messages: Message[],
  toolName: string,
): boolean {
  // Search backwards to find most recent tool_use for this tool
  let mostRecentToolUseId: string | undefined
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i]
    if (!msg) continue
    if (msg.type === 'assistant' && Array.isArray(msg.message.content)) {
      const toolUse = msg.message.content.find(
        (block): block is ToolUseBlock =>
          block.type === 'tool_use' && block.name === toolName,
      )
      if (toolUse) {
        mostRecentToolUseId = toolUse.id
        break
      }
    }
  }

  if (!mostRecentToolUseId) return false

  // Find the corresponding tool_result (search backwards)
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i]
    if (!msg) continue
    if (msg.type === 'user' && Array.isArray(msg.message.content)) {
      const toolResult = msg.message.content.find(
        (block): block is import('../../types/api-types.js').ToolResultBlockParam =>
          block.type === 'tool_result' &&
          block.tool_use_id === mostRecentToolUseId,
      )
      if (toolResult) {
        // Success if is_error is false or undefined
        return toolResult.is_error !== true
      }
    }
  }

  // Tool called but no result yet (shouldn't happen in practice)
  return false
}

// ---------------------------------------------------------------------------
// ID derivation
// ---------------------------------------------------------------------------

// Deterministic UUID derivation. Produces a stable UUID-shaped string from a
// parent UUID + content block index so that the same input always produces the
// same key across calls. Used by normalizeMessages and synthetic message creation.
//
// TS-only: currently used by normalize-core.ts. Any future cross-language
// implementation must add conformance tests for identical derivation.
export function deriveUUID(parentUUID: UUID, index: number): UUID {
  const hex = index.toString(16).padStart(12, '0')
  return `${parentUUID.slice(0, 24)}${hex}` as UUID
}

/**
 * Derive a short stable message ID (6-char base36 string) from a UUID.
 * Used for snip tool referencing -- injected into API-bound messages as [id:...] tags.
 * Deterministic: same UUID always produces the same short ID.
 *
 * TS-only: 同 deriveUUID — 跨语言一致性需求出现前保持 TS 侧实现。
 */
export function deriveShortMessageId(uuid: string): string {
  // Take first 10 hex chars from the UUID (skipping dashes)
  const hex = uuid.replace(/-/g, '').slice(0, 10)
  // Convert to base36 for shorter representation, take 6 chars
  return parseInt(hex, 16).toString(36).slice(0, 6)
}

// ---------------------------------------------------------------------------
// Text extraction
// ---------------------------------------------------------------------------

export function getAssistantMessageText(message: Message): string | null {
  if (message.type !== 'assistant') {
    return null
  }

  // For content blocks array, extract and concatenate text blocks
  if (Array.isArray(message.message.content)) {
    return (
      message.message.content
        .filter(block => block.type === 'text')
        .map(block => (block.type === 'text' ? block.text : ''))
        .join('\n')
        .trim() || null
    )
  }
  return null
}

export function getUserMessageText(
  message: Message | NormalizedMessage,
): string | null {
  if (message.type !== 'user') {
    return null
  }

  const content = message.message.content

  return getContentText(content)
}

/**
 * Extract text from an array of content blocks, joining text blocks with the
 * given separator. Works with ContentBlock, ContentBlockParam, BetaContentBlock,
 * and their readonly/DeepImmutable variants via structural typing.
 */
export function extractTextContent(
  blocks: readonly { readonly type: string }[],
  separator = '',
): string {
  return blocks
    .filter((b): b is { type: 'text'; text: string } => b.type === 'text')
    .map(b => b.text)
    .join(separator)
}

export function getContentText(
  content: string | import('../../types/utils.js').DeepImmutable<Array<ContentBlockParam>>,
): string | null {
  if (typeof content === 'string') {
    return content
  }
  if (Array.isArray(content)) {
    return extractTextContent(content, '\n').trim() || null
  }
  return null
}

export function getToolUseID(message: NormalizedMessage): string | null {
  switch (message.type) {
    case 'attachment':
      if (isHookAttachmentMessage(message)) {
        return message.attachment.toolUseID
      }
      return null
    case 'assistant':
      if (message.message.content[0]?.type !== 'tool_use') {
        return null
      }
      return message.message.content[0].id
    case 'user':
      if (message.sourceToolUseID) {
        return message.sourceToolUseID
      }

      const _c0 = message.message.content[0]
      if (typeof _c0 === 'string' || _c0?.type !== 'tool_result') {
        return null
      }
      return _c0.tool_use_id
    case 'progress':
      return message.toolUseID
    case 'system':
      return message.subtype === 'informational'
        ? (message.toolUseID ?? null)
        : null
  }
}

export function getToolResultIDs(normalizedMessages: NormalizedMessage[]): {
  [toolUseID: string]: boolean
} {
  return Object.fromEntries(
    normalizedMessages.flatMap(_ => {
      const c0 = _.type === 'user' ? _.message.content[0] : undefined
      return typeof c0 !== 'string' && c0?.type === 'tool_result'
        ? [
            [
              c0.tool_use_id,
              c0.is_error ?? false,
            ] as [string, boolean],
          ]
        : ([] as [string, boolean][])
    }),
  )
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type StreamingToolUse = {
  index: number
  contentBlock: BetaToolUseBlock
  unparsedToolInput: string
}

export type StreamingThinking = {
  thinking: string
  isStreaming: boolean
  streamingEndedAt?: number
}

// ---------------------------------------------------------------------------
// Internal helpers (used by getToolUseID above and render.ts)
// ---------------------------------------------------------------------------

import type { AttachmentMessage } from '../../types/message.js'
import type { HookAttachment } from '../attachments.js'

export function isHookAttachmentMessage(
  message: Message,
): message is AttachmentMessage<HookAttachment> {
  return (
    message.type === 'attachment' &&
    (message.attachment.type === 'hook_blocking_error' ||
      message.attachment.type === 'hook_cancelled' ||
      message.attachment.type === 'hook_error_during_execution' ||
      message.attachment.type === 'hook_non_blocking_error' ||
      message.attachment.type === 'hook_success' ||
      message.attachment.type === 'hook_system_message' ||
      message.attachment.type === 'hook_additional_context' ||
      message.attachment.type === 'hook_stopped_continuation')
  )
}

/**
 * Internal check used by factory.ts (isSyntheticApiErrorMessage)
 */
export function isSyntheticApiErrorMessage(
  message: Message,
): message is AssistantMessage & { isApiErrorMessage: true } {
  return (
    message.type === 'assistant' &&
    message.isApiErrorMessage === true &&
    message.message.model === SYNTHETIC_MODEL
  )
}

/**
 * Internal check used by normalize.ts (isToolResultMessage)
 */
export function isToolResultMessage(msg: Message): boolean {
  if (msg.type !== 'user') {
    return false
  }
  const content = msg.message.content
  if (typeof content === 'string') return false
  return content.some(block => block.type === 'tool_result')
}
