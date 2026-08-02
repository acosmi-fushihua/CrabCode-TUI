/**
 * normalize-helpers.ts -- Filter and strip helper functions for message normalization.
 *
 * Contains stripToolReferenceBlocksFromUserMessage, stripCallerFieldFromAssistantMessage,
 * filterUnresolvedToolUses, filterWhitespaceOnlyAssistantMessages,
 * filterOrphanedThinkingOnlyMessages, stripSignatureBlocks, stripAdvisorBlocks,
 * and isSystemLocalCommandMessage.
 */

import type {
  BetaContentBlock,
  BetaRedactedThinkingBlock,
  BetaThinkingBlock,
  ContentBlock,
  ContentBlockParam,
  RedactedThinkingBlock,
  RedactedThinkingBlockParam,
  ThinkingBlock,
  ThinkingBlockParam,
} from '../../types/api-types.js'
import { feature } from '../featurePolyfill.js'

import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'

import { isConnectorTextBlock } from '../../types/connectorText.js'
import type {
  AssistantMessage,
  Message,
  SystemLocalCommandMessage,
  UserMessage,
} from '../../types/message.js'
import { isAdvisorBlock } from '../advisor.js'
import {
  isToolReferenceBlock,
} from '../toolSearch.js'

import { mergeUserMessages } from './normalize-core.js'

// ---------------------------------------------------------------------------
// Strip / filter helpers
// ---------------------------------------------------------------------------

/**
 * Strips tool_reference blocks from tool_result content in a user message.
 * tool_reference blocks are only valid when the tool search beta is enabled.
 */
export function stripToolReferenceBlocksFromUserMessage(
  message: UserMessage,
): UserMessage {
  const content = message.message.content
  if (!Array.isArray(content)) {
    return message
  }

  const hasToolReference = content.some(
    block =>
      block.type === 'tool_result' &&
      Array.isArray(block.content) &&
      block.content.some(isToolReferenceBlock),
  )

  if (!hasToolReference) {
    return message
  }

  return {
    ...message,
    message: {
      ...message.message,
      content: content.map(block => {
        if (block.type !== 'tool_result' || !Array.isArray(block.content)) {
          return block
        }

        // Filter out tool_reference blocks from tool_result content
        const filteredContent = block.content.filter(
          c => !isToolReferenceBlock(c),
        )

        // If all content was tool_reference blocks, replace with a placeholder
        if (filteredContent.length === 0) {
          return {
            ...block,
            content: [
              {
                type: 'text' as const,
                text: '[Tool references removed - tool search not enabled]',
              },
            ],
          }
        }

        return {
          ...block,
          content: filteredContent,
        }
      }),
    },
  }
}

/**
 * Strips the 'caller' field from tool_use blocks in an assistant message.
 */
export function stripCallerFieldFromAssistantMessage(
  message: AssistantMessage,
): AssistantMessage {
  const hasCallerField = message.message.content.some(
    block =>
      block.type === 'tool_use' && 'caller' in block && block.caller !== null,
  )

  if (!hasCallerField) {
    return message
  }

  return {
    ...message,
    message: {
      ...message.message,
      content: message.message.content.map(block => {
        if (block.type !== 'tool_use') {
          return block
        }
        // Explicitly construct with only standard API fields
        return {
          type: 'tool_use' as const,
          id: block.id,
          name: block.name,
          input: block.input,
        }
      }),
    },
  }
}

export function filterUnresolvedToolUses(messages: Message[]): Message[] {
  // Collect all tool_use IDs and tool_result IDs directly from message content blocks.
  const toolUseIds = new Set<string>()
  const toolResultIds = new Set<string>()

  for (const msg of messages) {
    if (msg.type !== 'user' && msg.type !== 'assistant') continue
    const content = msg.message.content
    if (!Array.isArray(content)) continue
    for (const block of content) {
      if (block.type === 'tool_use') {
        toolUseIds.add(block.id)
      }
      if (block.type === 'tool_result') {
        toolResultIds.add(block.tool_use_id)
      }
    }
  }

  const unresolvedIds = new Set(
    [...toolUseIds].filter(id => !toolResultIds.has(id)),
  )

  if (unresolvedIds.size === 0) {
    return messages
  }

  // Filter out assistant messages whose tool_use blocks are all unresolved
  return messages.filter(msg => {
    if (msg.type !== 'assistant') return true
    const content = msg.message.content
    if (!Array.isArray(content)) return true
    const toolUseBlockIds: string[] = []
    for (const b of content) {
      if (b.type === 'tool_use') {
        toolUseBlockIds.push(b.id)
      }
    }
    if (toolUseBlockIds.length === 0) return true
    // Remove message only if ALL its tool_use blocks are unresolved
    return !toolUseBlockIds.every(id => unresolvedIds.has(id))
  })
}

export function filterWhitespaceOnlyAssistantMessages(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[]
export function filterWhitespaceOnlyAssistantMessages(
  messages: Message[],
): Message[]
export function filterWhitespaceOnlyAssistantMessages(
  messages: Message[],
): Message[] {
  let hasChanges = false

  const filtered = messages.filter(message => {
    if (message.type !== 'assistant') {
      return true
    }

    const content = message.message.content
    // Keep messages with empty arrays (handled elsewhere) or that have real content
    if (!Array.isArray(content) || content.length === 0) {
      return true
    }

    if (hasOnlyWhitespaceTextContent(content)) {
      hasChanges = true
      logEvent('tengu_filtered_whitespace_only_assistant', {
        messageUUID:
          message.uuid as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })
      return false
    }

    return true
  })

  if (!hasChanges) {
    return messages
  }

  // Removing assistant messages may leave adjacent user messages that need
  // merging (the API requires alternating user/assistant roles).
  const merged: Message[] = []
  for (const message of filtered) {
    const prev = merged.at(-1)
    if (message.type === 'user' && prev?.type === 'user') {
      merged[merged.length - 1] = mergeUserMessages(prev, message) // lvalue
    } else {
      merged.push(message)
    }
  }
  return merged
}

export function filterOrphanedThinkingOnlyMessages(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[]
export function filterOrphanedThinkingOnlyMessages(
  messages: Message[],
): Message[]
export function filterOrphanedThinkingOnlyMessages(
  messages: Message[],
): Message[] {
  // First pass: collect message.ids that have non-thinking content
  const messageIdsWithNonThinkingContent = new Set<string>()
  for (const msg of messages) {
    if (msg.type !== 'assistant') continue

    const content = msg.message.content
    if (!Array.isArray(content)) continue

    const hasNonThinking = content.some(
      block => block.type !== 'thinking' && block.type !== 'redacted_thinking',
    )
    if (hasNonThinking && msg.message.id) {
      messageIdsWithNonThinkingContent.add(msg.message.id)
    }
  }

  // Second pass: filter out thinking-only messages that are truly orphaned
  const filtered = messages.filter(msg => {
    if (msg.type !== 'assistant') {
      return true
    }

    const content = msg.message.content
    if (!Array.isArray(content) || content.length === 0) {
      return true
    }

    // Check if ALL content blocks are thinking blocks
    const allThinking = content.every(
      block => block.type === 'thinking' || block.type === 'redacted_thinking',
    )

    if (!allThinking) {
      return true // Has non-thinking content, keep it
    }

    // It's thinking-only. Keep it if there's another message with same id
    // that has non-thinking content (they'll be merged later)
    if (
      msg.message.id &&
      messageIdsWithNonThinkingContent.has(msg.message.id)
    ) {
      return true
    }

    // Truly orphaned - no other message with same id has content to merge with
    logEvent('tengu_filtered_orphaned_thinking_message', {
      messageUUID:
        msg.uuid as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      messageId: msg.message
        .id as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      blockCount: content.length,
    })
    return false
  })

  return filtered
}

/**
 * Strip signature-bearing blocks (thinking, redacted_thinking, connector_text)
 * from all assistant messages.
 */
export function stripSignatureBlocks(messages: Message[]): Message[] {
  let changed = false
  const result = messages.map(msg => {
    if (msg.type !== 'assistant') return msg

    const content = msg.message.content
    if (!Array.isArray(content)) return msg

    const filtered = content.filter(block => {
      if (isThinkingBlock(block)) return false
      if (feature('CONNECTOR_TEXT')) {
        if (isConnectorTextBlock(block)) return false
      }
      return true
    })
    if (filtered.length === content.length) return msg

    changed = true
    return {
      ...msg,
      message: { ...msg.message, content: filtered },
    } as typeof msg
  })

  return changed ? result : messages
}

/**
 * Strip advisor blocks from messages.
 */
export function stripAdvisorBlocks(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  let changed = false
  const result = messages.map(msg => {
    if (msg.type !== 'assistant') return msg
    const content = msg.message.content
    const filtered = content.filter(b => !isAdvisorBlock(b))
    if (filtered.length === content.length) return msg
    changed = true
    if (
      filtered.length === 0 ||
      filtered.every(
        b =>
          b.type === 'thinking' ||
          b.type === 'redacted_thinking' ||
          (b.type === 'text' && (!b.text || !b.text.trim())),
      )
    ) {
      filtered.push({
        type: 'text' as const,
        text: '[Advisor response]',
        citations: [],
      })
    }
    return { ...msg, message: { ...msg.message, content: filtered } }
  })
  return changed ? result : messages
}

// ---------------------------------------------------------------------------
// Internal filter/strip helpers
// ---------------------------------------------------------------------------

type ThinkingBlockType =
  | ThinkingBlock
  | RedactedThinkingBlock
  | ThinkingBlockParam
  | RedactedThinkingBlockParam
  | BetaThinkingBlock
  | BetaRedactedThinkingBlock

export function isThinkingBlock(
  block: ContentBlockParam | ContentBlock | BetaContentBlock,
): block is ThinkingBlockType {
  return block.type === 'thinking' || block.type === 'redacted_thinking'
}

export function hasOnlyWhitespaceTextContent(
  content: Array<{ type: string; text?: string }>,
): boolean {
  if (content.length === 0) {
    return false
  }

  for (const block of content) {
    if (block.type !== 'text') {
      return false
    }
    if (block.text !== undefined && block.text.trim() !== '') {
      return false
    }
  }

  return true
}

// ---------------------------------------------------------------------------
// isSystemLocalCommandMessage
// ---------------------------------------------------------------------------

export function isSystemLocalCommandMessage(
  message: Message,
): message is SystemLocalCommandMessage {
  return message.type === 'system' && message.subtype === 'local_command'
}
