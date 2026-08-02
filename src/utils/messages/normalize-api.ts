/**
 * normalize-api.ts -- API normalization pipeline and tool result pairing.
 *
 * Contains reorderAttachmentsForAPI, normalizeMessagesForAPI,
 * ensureToolResultPairing, and their internal helper functions.
 */

import type {
  ContentBlock,
  ContentBlockParam,
  TextBlockParam,
  ToolResultBlockParam,
  ToolUseBlock,
  ToolUseBlockParam,
} from '../../types/api-types.js'
import { feature } from '../featurePolyfill.js'

import last from 'lodash-es/last.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'

import { NO_CONTENT_MESSAGE } from '../../constants/messages.js'
import { getStrictToolResultPairing } from '../../bootstrap/state.js'
import {
  checkStatsigFeatureGate_CACHED_MAY_BE_STALE,
} from '../../services/analytics/growthbook.js'
import {
  getImageTooLargeErrorMessage,
  getPdfInvalidErrorMessage,
  getPdfPasswordProtectedErrorMessage,
  getPdfTooLargeErrorMessage,
  getRequestTooLargeErrorMessage,
} from '../../services/api/errors.js'
import {
  type Tools,
  toolMatchesName,
} from '../../Tool.js'
import type {
  AssistantMessage,
  AttachmentMessage,
  Message,
  SystemLocalCommandMessage,
  UserMessage,
} from '../../types/message.js'
import { normalizeToolInputForAPI } from '../api.js'
import { logForDebugging } from '../debug.js'
import { validateImagesForAPI } from '../imageValidation.js'
import { logError } from '../log.js'
import { normalizeLegacyToolName } from '../permissions/permissionRuleParser.js'
import {
  isToolReferenceBlock,
  isToolSearchEnabledOptimistic,
} from '../toolSearch.js'

import {
  SYNTHETIC_TOOL_RESULT_PLACEHOLDER,
  deriveShortMessageId,
  isToolResultMessage,
  isSyntheticApiErrorMessage,
} from './envelope.js'

import {
  createUserMessage,
} from './factory.js'

import {
  mergeUserMessages,
  mergeAssistantMessages,
  mergeUserMessagesAndToolResults,
  mergeAdjacentUserMessages,
  smooshIntoToolResult,
  TOOL_REFERENCE_TURN_BOUNDARY,
} from './normalize-core.js'

import {
  stripToolReferenceBlocksFromUserMessage,
  filterOrphanedThinkingOnlyMessages,
  filterWhitespaceOnlyAssistantMessages,
  isThinkingBlock,
  isSystemLocalCommandMessage,
} from './normalize-helpers.js'

import {
  normalizeAttachmentForAPI,
} from './normalize-attachments.js'

import {
  wrapInSystemReminder,
} from './normalize-compact.js'

// ---------------------------------------------------------------------------
// Internal filter/strip helpers used by normalizeMessagesForAPI
// ---------------------------------------------------------------------------

/**
 * Consolidate tool_use blocks within each assistant message.
 *
 * DeepSeek / Qwen multi-batch fan-out emit non-canonical content shapes:
 *   [thinking, text, tu_set1×N, thinking, text, tu_set2×M]
 * with thinking/text blocks BETWEEN tool_use groups (and sometimes AFTER the
 * last group). Acosmi gateway's strict pairing scanner treats any non-tu
 * block following tu blocks as an "implicit segment break" and rejects the
 * next turn with "tool_use ids were found without tool_result blocks
 * immediately after" — even though every tu has its tr in the next user
 * message (the gateway scans set1's 9 tu, hits the inter-batch text, treats
 * set1 as closed, and demands set1's tr arrive before set2 — but set1+set2
 * tr are interleaved in one user msg).
 *
 * Fix: keep head reasoning (everything before the FIRST tool_use), then
 * collect ALL tool_use blocks consecutively, dropping any non-tool_use
 * content that appears AFTER the first tool_use (inter-batch + tail).
 * Result: canonical [head_blocks..., tu_block+] shape that the gateway
 * accepts. Persistence keeps the inter-batch thinking/text in JSONL so TUI
 * UX (progress narration) is unaffected — only the API request is
 * normalized.
 *
 * server_tool_use / mcp_tool_use are treated as tu-class (kept, count as
 * "tool_use" for the firstToolUseIdx scan) — consistent with downstream
 * ensureToolResultPairing's handling at normalize-api.ts:991-997.
 */
function consolidateToolUseBlocksInAssistant(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  let changed = false
  const result = messages.map(msg => {
    if (msg.type !== 'assistant') return msg
    const content = msg.message.content
    if (!Array.isArray(content) || content.length === 0) return msg

    let firstToolUseIdx = -1
    for (let i = 0; i < content.length; i++) {
      const t = content[i]?.type
      if (t === 'tool_use' || t === 'server_tool_use' || t === 'mcp_tool_use') {
        firstToolUseIdx = i
        break
      }
    }
    if (firstToolUseIdx === -1) {
      return msg
    }

    const head = content.slice(0, firstToolUseIdx)
    const tail = content.slice(firstToolUseIdx)
    const tuBlocks = tail.filter(b => {
      const t = b?.type
      return t === 'tool_use' || t === 'server_tool_use' || t === 'mcp_tool_use'
    })

    if (tuBlocks.length === tail.length) {
      return msg
    }

    changed = true
    return {
      ...msg,
      message: {
        ...msg.message,
        content: [...head, ...tuBlocks],
      },
    }
  })
  return changed ? result : messages
}

/**
 * Filter trailing thinking blocks from the last message if it's an assistant message.
 */
function filterTrailingThinkingFromLastAssistant(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  const lastMessage = messages.at(-1)
  if (!lastMessage || lastMessage.type !== 'assistant') {
    return messages
  }

  const content = lastMessage.message.content
  const lastBlock = content.at(-1)
  if (!lastBlock || !isThinkingBlock(lastBlock)) {
    return messages
  }

  // Find last non-thinking block
  let lastValidIndex = content.length - 1
  while (lastValidIndex >= 0) {
    const block = content[lastValidIndex]
    if (!block || !isThinkingBlock(block)) {
      break
    }
    lastValidIndex--
  }

  logEvent('tengu_filtered_trailing_thinking_block', {
    messageUUID:
      lastMessage.uuid as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    blocksRemoved: content.length - lastValidIndex - 1,
    remainingBlocks: lastValidIndex + 1,
  })

  // Insert placeholder if all blocks were thinking
  const filteredContent =
    lastValidIndex < 0
      ? [{ type: 'text' as const, text: '[No message content]', citations: [] }]
      : content.slice(0, lastValidIndex + 1)

  const result = [...messages]
  result[messages.length - 1] = {
    ...lastMessage,
    message: {
      ...lastMessage.message,
      content: filteredContent,
    },
  }
  return result
}

function ensureNonEmptyAssistantContent(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  if (messages.length === 0) {
    return messages
  }

  let hasChanges = false
  const result = messages.map((message, index) => {
    if (message.type !== 'assistant') {
      return message
    }

    if (index === messages.length - 1) {
      return message
    }

    const content = message.message.content
    if (Array.isArray(content) && content.length === 0) {
      hasChanges = true
      logEvent('tengu_fixed_empty_assistant_content', {
        messageUUID:
          message.uuid as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        messageIndex: index,
      })

      return {
        ...message,
        message: {
          ...message.message,
          content: [
            { type: 'text' as const, text: NO_CONTENT_MESSAGE, citations: [] },
          ],
        },
      }
    }

    return message
  })

  return hasChanges ? result : messages
}

/**
 * Does the content array have a tool_result block whose inner content
 * contains tool_reference (ToolSearch loaded tools)?
 */
function contentHasToolReference(
  content: ReadonlyArray<ContentBlockParam>,
): boolean {
  return content.some(
    block =>
      block.type === 'tool_result' &&
      Array.isArray(block.content) &&
      block.content.some(isToolReferenceBlock),
  )
}

/**
 * Strips tool_reference blocks for tools that no longer exist from tool_result content.
 */
function stripUnavailableToolReferencesFromUserMessage(
  message: UserMessage,
  availableToolNames: Set<string>,
): UserMessage {
  const content = message.message.content
  if (!Array.isArray(content)) {
    return message
  }

  const hasUnavailableReference = content.some(
    block =>
      block.type === 'tool_result' &&
      Array.isArray(block.content) &&
      block.content.some(c => {
        if (!isToolReferenceBlock(c)) return false
        const toolName = (c as { tool_name?: string }).tool_name
        return (
          toolName && !availableToolNames.has(normalizeLegacyToolName(toolName))
        )
      }),
  )

  if (!hasUnavailableReference) {
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

        const filteredContent = block.content.filter(c => {
          if (!isToolReferenceBlock(c)) return true
          const rawToolName = (c as { tool_name?: string }).tool_name
          if (!rawToolName) return true
          const toolName = normalizeLegacyToolName(rawToolName)
          const isAvailable = availableToolNames.has(toolName)
          if (!isAvailable) {
            logForDebugging(
              `Filtering out tool_reference for unavailable tool: ${toolName}`,
              { level: 'warn' },
            )
          }
          return isAvailable
        })

        if (filteredContent.length === 0) {
          return {
            ...block,
            content: [
              {
                type: 'text' as const,
                text: '[Tool references removed - tools no longer available]',
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
 * Appends a [id:...] message ID tag to the last text block of a user message.
 */
function appendMessageTagToUserMessage(message: UserMessage): UserMessage {
  if (message.isMeta) {
    return message
  }

  const tag = `\n[id:${deriveShortMessageId(message.uuid)}]`

  const content = message.message.content

  if (typeof content === 'string') {
    return {
      ...message,
      message: {
        ...message.message,
        content: content + tag,
      },
    }
  }

  if (!Array.isArray(content) || content.length === 0) {
    return message
  }

  let lastTextIdx = -1
  for (let i = content.length - 1; i >= 0; i--) {
    if (content[i]!.type === 'text') {
      lastTextIdx = i
      break
    }
  }
  if (lastTextIdx === -1) {
    return message
  }

  const newContent = [...content]
  const textBlock = newContent[lastTextIdx] as TextBlockParam
  newContent[lastTextIdx] = {
    ...textBlock,
    text: textBlock.text + tag,
  }

  return {
    ...message,
    message: {
      ...message.message,
      content: newContent as typeof content,
    },
  }
}

/**
 * Ensure all text content in attachment-origin messages carries the
 * <system-reminder> wrapper.
 */
function ensureSystemReminderWrap(msg: UserMessage): UserMessage {
  const content = msg.message.content
  if (typeof content === 'string') {
    if (content.startsWith('<system-reminder>')) return msg
    return {
      ...msg,
      message: { ...msg.message, content: wrapInSystemReminder(content) },
    }
  }
  let changed = false
  const newContent = content.map(b => {
    if (b.type === 'text' && !b.text.startsWith('<system-reminder>')) {
      changed = true
      return { ...b, text: wrapInSystemReminder(b.text) }
    }
    return b
  })
  return changed
    ? { ...msg, message: { ...msg.message, content: newContent } }
    : msg
}

/**
 * Final pass: smoosh any `<system-reminder>`-prefixed text siblings into the
 * last tool_result of the same user message.
 */
function smooshSystemReminderSiblings(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  return messages.map(msg => {
    if (msg.type !== 'user') return msg
    const content = msg.message.content
    if (!Array.isArray(content)) return msg

    const hasToolResult = content.some(b => b.type === 'tool_result')
    if (!hasToolResult) return msg

    const srText: TextBlockParam[] = []
    const kept: ContentBlockParam[] = []
    for (const b of content) {
      if (b.type === 'text' && b.text.startsWith('<system-reminder>')) {
        srText.push(b)
      } else {
        kept.push(b)
      }
    }
    if (srText.length === 0) return msg

    const lastTrIdx = kept.findLastIndex(b => b.type === 'tool_result')
    const lastTr = kept[lastTrIdx] as ToolResultBlockParam
    const smooshed = smooshIntoToolResult(lastTr, srText)
    if (smooshed === null) return msg

    const newContent = [
      ...kept.slice(0, lastTrIdx),
      smooshed,
      ...kept.slice(lastTrIdx + 1),
    ]
    return {
      ...msg,
      message: { ...msg.message, content: newContent },
    }
  })
}

/**
 * Strip non-text blocks from is_error tool_results.
 */
function sanitizeErrorToolResultContent(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  return messages.map(msg => {
    if (msg.type !== 'user') return msg
    const content = msg.message.content
    if (!Array.isArray(content)) return msg

    let changed = false
    const newContent = content.map(b => {
      if (b.type !== 'tool_result' || !b.is_error) return b
      const trContent = b.content
      if (!Array.isArray(trContent)) return b
      if (trContent.every(c => c.type === 'text')) return b
      changed = true
      const texts = trContent.filter(c => c.type === 'text').map(c => c.text)
      const textOnly: TextBlockParam[] =
        texts.length > 0 ? [{ type: 'text', text: texts.join('\n\n') }] : []
      return { ...b, content: textOnly }
    })
    if (!changed) return msg
    return { ...msg, message: { ...msg.message, content: newContent } }
  })
}

function relocateToolReferenceSiblings(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  const result = [...messages]

  for (let i = 0; i < result.length; i++) {
    const msg = result[i]!
    if (msg.type !== 'user') continue
    const content = msg.message.content
    if (!Array.isArray(content)) continue
    if (!contentHasToolReference(content)) continue

    const textSiblings = content.filter(b => b.type === 'text')
    if (textSiblings.length === 0) continue

    let targetIdx = -1
    for (let j = i + 1; j < result.length; j++) {
      const cand = result[j]!
      if (cand.type !== 'user') continue
      const cc = cand.message.content
      if (!Array.isArray(cc)) continue
      if (!cc.some(b => b.type === 'tool_result')) continue
      if (contentHasToolReference(cc)) continue
      targetIdx = j
      break
    }

    if (targetIdx === -1) continue

    result[i] = {
      ...msg,
      message: {
        ...msg.message,
        content: content.filter(b => b.type !== 'text'),
      },
    }
    const target = result[targetIdx] as UserMessage
    result[targetIdx] = {
      ...target,
      message: {
        ...target.message,
        content: [
          ...(target.message.content as ContentBlockParam[]),
          ...textSiblings,
        ],
      },
    }
  }

  return result
}

// ---------------------------------------------------------------------------
// reorderAttachmentsForAPI
// ---------------------------------------------------------------------------

export function reorderAttachmentsForAPI(messages: Message[]): Message[] {
  const result: Message[] = []
  const pendingAttachments: AttachmentMessage[] = []

  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i]!

    if (message.type === 'attachment') {
      pendingAttachments.push(message)
    } else {
      const isStoppingPoint =
        message.type === 'assistant' ||
        (message.type === 'user' &&
          Array.isArray(message.message.content) &&
          message.message.content[0]?.type === 'tool_result')

      if (isStoppingPoint && pendingAttachments.length > 0) {
        for (let j = 0; j < pendingAttachments.length; j++) {
          result.push(pendingAttachments[j]!)
        }
        result.push(message)
        pendingAttachments.length = 0
      } else {
        result.push(message)
      }
    }
  }

  for (let j = 0; j < pendingAttachments.length; j++) {
    result.push(pendingAttachments[j]!)
  }

  result.reverse()
  return result
}

// ---------------------------------------------------------------------------
// normalizeMessagesForAPI
// ---------------------------------------------------------------------------

export function normalizeMessagesForAPI(
  messages: Message[],
  tools: Tools = [],
): (UserMessage | AssistantMessage)[] {
  // Build set of available tool names for filtering unavailable tool references
  const availableToolNames = new Set(tools.map(t => t.name))

  // First, reorder attachments to bubble up until they hit a tool result or assistant message
  // Then strip virtual messages -- they're display-only
  const reorderedMessages = reorderAttachmentsForAPI(messages).filter(
    m => !((m.type === 'user' || m.type === 'assistant') && m.isVirtual),
  )

  // Build a map from error text -> which block types to strip from the preceding user message.
  const errorToBlockTypes: Record<string, Set<string>> = {
    [getPdfTooLargeErrorMessage()]: new Set(['document']),
    [getPdfPasswordProtectedErrorMessage()]: new Set(['document']),
    [getPdfInvalidErrorMessage()]: new Set(['document']),
    [getImageTooLargeErrorMessage()]: new Set(['image']),
    [getRequestTooLargeErrorMessage()]: new Set(['document', 'image']),
  }

  const stripTargets = new Map<string, Set<string>>()
  for (let i = 0; i < reorderedMessages.length; i++) {
    const msg = reorderedMessages[i]!
    if (!isSyntheticApiErrorMessage(msg)) {
      continue
    }
    const errorText =
      Array.isArray(msg.message.content) &&
      msg.message.content[0]?.type === 'text'
        ? msg.message.content[0].text
        : undefined
    if (!errorText) {
      continue
    }
    const blockTypesToStrip = errorToBlockTypes[errorText]
    if (!blockTypesToStrip) {
      continue
    }
    for (let j = i - 1; j >= 0; j--) {
      const candidate = reorderedMessages[j]!
      if (candidate.type === 'user' && candidate.isMeta) {
        const existing = stripTargets.get(candidate.uuid)
        if (existing) {
          for (const t of blockTypesToStrip) {
            existing.add(t)
          }
        } else {
          stripTargets.set(candidate.uuid, new Set(blockTypesToStrip))
        }
        break
      }
      if (isSyntheticApiErrorMessage(candidate)) {
        continue
      }
      break
    }
  }

  const result: (UserMessage | AssistantMessage)[] = []
  reorderedMessages
    .filter(
      (
        _,
      ): _ is
        | UserMessage
        | AssistantMessage
        | AttachmentMessage
        | SystemLocalCommandMessage => {
        if (
          _.type === 'progress' ||
          (_.type === 'system' && !isSystemLocalCommandMessage(_)) ||
          isSyntheticApiErrorMessage(_)
        ) {
          return false
        }
        return true
      },
    )
    .forEach(message => {
      switch (message.type) {
        case 'system': {
          // local_command system messages need to be included as user messages
          const userMsg = createUserMessage({
            content: message.content,
            uuid: message.uuid,
            timestamp: message.timestamp,
          })
          const lastMessage = last(result)
          if (lastMessage?.type === 'user') {
            result[result.length - 1] = mergeUserMessages(lastMessage, userMsg)
            return
          }
          result.push(userMsg)
          return
        }
        case 'user': {
          let normalizedMessage = message
          if (!isToolSearchEnabledOptimistic()) {
            normalizedMessage = stripToolReferenceBlocksFromUserMessage(message)
          } else {
            normalizedMessage = stripUnavailableToolReferencesFromUserMessage(
              message,
              availableToolNames,
            )
          }

          const typesToStrip = stripTargets.get(normalizedMessage.uuid)
          if (typesToStrip && normalizedMessage.isMeta) {
            const content = normalizedMessage.message.content
            if (Array.isArray(content)) {
              const filtered = content.filter(
                block => !typesToStrip.has(block.type),
              )
              if (filtered.length === 0) {
                return
              }
              if (filtered.length < content.length) {
                normalizedMessage = {
                  ...normalizedMessage,
                  message: {
                    ...normalizedMessage.message,
                    content: filtered,
                  },
                }
              }
            }
          }

          if (
            !checkStatsigFeatureGate_CACHED_MAY_BE_STALE(
              'tengu_toolref_defer_j8m',
            )
          ) {
            const contentAfterStrip = normalizedMessage.message.content
            if (
              Array.isArray(contentAfterStrip) &&
              !contentAfterStrip.some(
                b =>
                  b.type === 'text' &&
                  b.text.startsWith(TOOL_REFERENCE_TURN_BOUNDARY),
              ) &&
              contentHasToolReference(contentAfterStrip)
            ) {
              normalizedMessage = {
                ...normalizedMessage,
                message: {
                  ...normalizedMessage.message,
                  content: [
                    ...contentAfterStrip,
                    { type: 'text', text: TOOL_REFERENCE_TURN_BOUNDARY },
                  ],
                },
              }
            }
          }

          const lastMessage = last(result)
          if (lastMessage?.type === 'user') {
            result[result.length - 1] = mergeUserMessages(
              lastMessage,
              normalizedMessage,
            )
            return
          }

          result.push(normalizedMessage)
          return
        }
        case 'assistant': {
          const toolSearchEnabled = isToolSearchEnabledOptimistic()
          const normalizedMessage: AssistantMessage = {
            ...message,
            message: {
              ...message.message,
              content: message.message.content.map(block => {
                if (block.type === 'tool_use') {
                  const tool = tools.find(t => toolMatchesName(t, block.name))
                  const normalizedInput = tool
                    ? normalizeToolInputForAPI(
                        tool,
                        block.input as Record<string, unknown>,
                      )
                    : block.input
                  const canonicalName = tool?.name ?? block.name

                  if (toolSearchEnabled) {
                    return {
                      ...block,
                      name: canonicalName,
                      input: normalizedInput,
                    }
                  }

                  return {
                    type: 'tool_use' as const,
                    id: block.id,
                    name: canonicalName,
                    input: normalizedInput,
                  }
                }
                return block
              }),
            },
          }

          for (let i = result.length - 1; i >= 0; i--) {
            const msg = result[i]!

            if (msg.type !== 'assistant' && !isToolResultMessage(msg)) {
              break
            }

            if (msg.type === 'assistant') {
              if (msg.message.id === normalizedMessage.message.id) {
                result[i] = mergeAssistantMessages(msg, normalizedMessage)
                return
              }
              continue
            }
          }

          result.push(normalizedMessage)
          return
        }
        case 'attachment': {
          const rawAttachmentMessage = normalizeAttachmentForAPI(
            message.attachment,
          )
          const attachmentMessage = checkStatsigFeatureGate_CACHED_MAY_BE_STALE(
            'tengu_chair_sermon',
          )
            ? rawAttachmentMessage.map(ensureSystemReminderWrap)
            : rawAttachmentMessage

          const lastMessage = last(result)
          if (lastMessage?.type === 'user') {
            result[result.length - 1] = attachmentMessage.reduce(
              (p, c) => mergeUserMessagesAndToolResults(p, c),
              lastMessage,
            )
            return
          }

          result.push(...attachmentMessage)
          return
        }
      }
    })

  const relocated = checkStatsigFeatureGate_CACHED_MAY_BE_STALE(
    'tengu_toolref_defer_j8m',
  )
    ? relocateToolReferenceSiblings(result)
    : result

  const withFilteredOrphans = filterOrphanedThinkingOnlyMessages(relocated)

  // Consolidate multi-batch tool_use blocks (DeepSeek/Qwen fan-out behavior).
  // Strips inter-batch + post-batch non-tu blocks so all tool_use blocks are
  // consecutive at the tail, matching the canonical Anthropic content shape.
  const withTruncatedPostToolUse =
    consolidateToolUseBlocksInAssistant(withFilteredOrphans)

  const withFilteredThinking =
    filterTrailingThinkingFromLastAssistant(withTruncatedPostToolUse)
  const withFilteredWhitespace =
    filterWhitespaceOnlyAssistantMessages(withFilteredThinking)
  const withNonEmpty = ensureNonEmptyAssistantContent(withFilteredWhitespace)

  const smooshed = checkStatsigFeatureGate_CACHED_MAY_BE_STALE(
    'tengu_chair_sermon',
  )
    ? smooshSystemReminderSiblings(mergeAdjacentUserMessages(withNonEmpty))
    : withNonEmpty

  const sanitized = sanitizeErrorToolResultContent(smooshed)

  if (feature('HISTORY_SNIP') && process.env.NODE_ENV !== 'test') {
    const { isSnipRuntimeEnabled } =
      // eslint-disable-next-line @typescript-eslint/no-require-imports
      require('../../services/compact/snipCompact.js') as typeof import('../../services/compact/snipCompact.js')
    if (isSnipRuntimeEnabled()) {
      for (let i = 0; i < sanitized.length; i++) {
        if (sanitized[i]!.type === 'user') {
          sanitized[i] = appendMessageTagToUserMessage(
            sanitized[i] as UserMessage,
          )
        }
      }
    }
  }

  validateImagesForAPI(sanitized)

  return sanitized
}

// ---------------------------------------------------------------------------
// ensureToolResultPairing
// ---------------------------------------------------------------------------

/**
 * Defensive validation: ensure tool_use/tool_result pairing is correct.
 *
 * Globally scans the transcript so that tool_results scattered across
 * non-adjacent user messages (DAG-style replays where parentUuid chains
 * skip past matching frames) get moved into the correct pairing position.
 * Genuinely missing pairs are filled with synthetic is_error placeholders;
 * orphan tool_results (no matching tool_use anywhere) are dropped. This is
 * the request-side defensive net protecting against streaming-merge or
 * persistence-layer pairing breaks.
 */
export function ensureToolResultPairing(
  messages: (UserMessage | AssistantMessage)[],
): (UserMessage | AssistantMessage)[] {
  // ----- Pass 1: globally collect tool_use ids and tool_result blocks -----
  const globalToolUseIds = new Set<string>()
  const globalResultBlocks = new Map<string, ToolResultBlockParam>()
  for (const m of messages) {
    if (m.type === 'assistant') {
      for (const b of m.message.content) {
        if (b.type === 'tool_use') globalToolUseIds.add(b.id)
      }
    } else if (m.type === 'user' && Array.isArray(m.message.content)) {
      for (const b of m.message.content) {
        if (
          typeof b === 'object' &&
          'type' in b &&
          b.type === 'tool_result'
        ) {
          const tr = b as ToolResultBlockParam
          if (!globalResultBlocks.has(tr.tool_use_id)) {
            globalResultBlocks.set(tr.tool_use_id, tr)
          }
        }
      }
    }
  }

  // ----- Pass 2: walk and emit paired sequence -----
  const result: (UserMessage | AssistantMessage)[] = []
  let repaired = false
  const allSeenToolUseIds = new Set<string>()
  const consumedResultIds = new Set<string>()

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i]!

    if (msg.type !== 'assistant') {
      // user (or other): drop tool_result blocks that have been moved to a
      // nearer pairing position, are orphans (no matching tool_use anywhere),
      // or duplicate another within the same message.
      if (msg.type === 'user' && Array.isArray(msg.message.content)) {
        const seenLocal = new Set<string>()
        const filtered = msg.message.content.filter(block => {
          if (
            typeof block !== 'object' ||
            !('type' in block) ||
            block.type !== 'tool_result'
          ) {
            return true
          }
          const trId = (block as ToolResultBlockParam).tool_use_id
          if (consumedResultIds.has(trId)) return false
          if (!globalToolUseIds.has(trId)) return false
          // Forward-move anomaly: tool_result appears before its tool_use in
          // input order. The matching asst will pair it via global lookup
          // when we reach it; emitting it here would duplicate.
          if (!allSeenToolUseIds.has(trId)) return false
          if (seenLocal.has(trId)) return false
          seenLocal.add(trId)
          return true
        })
        if (filtered.length !== msg.message.content.length) {
          repaired = true
          if (filtered.length === 0) {
            // At conversation start, an orphan-only user msg becomes a
            // sentinel; mid-conversation it just gets dropped.
            if (result.length === 0) {
              result.push({
                ...msg,
                message: {
                  ...msg.message,
                  content: [
                    {
                      type: 'text' as const,
                      text: '[Orphaned tool result removed due to conversation resume]',
                    },
                  ],
                },
              })
            }
            continue
          }
          result.push({
            ...msg,
            message: { ...msg.message, content: filtered },
          })
          continue
        }
      }
      result.push(msg)
      continue
    }

    // ----- assistant -----
    const serverResultIds = new Set<string>()
    for (const c of msg.message.content) {
      if ('tool_use_id' in c && typeof c.tool_use_id === 'string') {
        serverResultIds.add(c.tool_use_id)
      }
    }

    const seenToolUseIds = new Set<string>()
    const finalContent = msg.message.content.filter(block => {
      if (block.type === 'tool_use') {
        if (allSeenToolUseIds.has(block.id)) {
          repaired = true
          return false
        }
        allSeenToolUseIds.add(block.id)
        seenToolUseIds.add(block.id)
      }
      if (
        (block.type === 'server_tool_use' || block.type === 'mcp_tool_use') &&
        !serverResultIds.has((block as { id: string }).id)
      ) {
        repaired = true
        return false
      }
      return true
    })

    const assistantContentChanged =
      finalContent.length !== msg.message.content.length

    if (finalContent.length === 0) {
      finalContent.push({
        type: 'text' as const,
        text: '[Tool use interrupted]',
        citations: [],
      })
    }

    const assistantMsg = assistantContentChanged
      ? {
          ...msg,
          message: { ...msg.message, content: finalContent },
        }
      : msg

    result.push(assistantMsg)

    const toolUseIds = [...seenToolUseIds]
    if (toolUseIds.length === 0) continue

    // ----- pair this asst's tool_uses with results -----
    const nextMsg = messages[i + 1]
    const nextIsUser =
      nextMsg?.type === 'user' && Array.isArray(nextMsg.message.content)
    const inlineMap = new Map<string, ToolResultBlockParam>()
    const inlineNonToolResult: (ContentBlockParam | ContentBlock)[] = []
    let inlineHasOrphan = false
    let inlineHasDup = false
    if (nextIsUser) {
      const seenInlineLocal = new Set<string>()
      for (const block of (nextMsg as UserMessage).message
        .content as (ContentBlockParam | ContentBlock)[]) {
        if (
          typeof block === 'object' &&
          'type' in block &&
          block.type === 'tool_result'
        ) {
          const tr = block as ToolResultBlockParam
          if (!seenToolUseIds.has(tr.tool_use_id)) {
            inlineHasOrphan = true
            continue
          }
          if (seenInlineLocal.has(tr.tool_use_id)) {
            inlineHasDup = true
            continue
          }
          seenInlineLocal.add(tr.tool_use_id)
          if (!inlineMap.has(tr.tool_use_id)) {
            inlineMap.set(tr.tool_use_id, tr)
          }
        } else {
          inlineNonToolResult.push(block)
        }
      }
    }

    let usedFarAwayOrSynth = false
    const pairedBlocks: (ContentBlockParam | ContentBlock)[] = []
    for (const id of toolUseIds) {
      const fromInline = inlineMap.get(id)
      if (fromInline) {
        pairedBlocks.push(fromInline)
        continue
      }
      const fromGlobal = globalResultBlocks.get(id)
      if (fromGlobal && !consumedResultIds.has(id)) {
        pairedBlocks.push(fromGlobal)
        consumedResultIds.add(id)
        usedFarAwayOrSynth = true
        continue
      }
      pairedBlocks.push({
        type: 'tool_result' as const,
        tool_use_id: id,
        content: SYNTHETIC_TOOL_RESULT_PLACEHOLDER,
        is_error: true,
      })
      usedFarAwayOrSynth = true
    }

    if (nextIsUser) {
      const allFulfilledInline = toolUseIds.every(id => inlineMap.has(id))
      if (
        !usedFarAwayOrSynth &&
        allFulfilledInline &&
        !inlineHasOrphan &&
        !inlineHasDup
      ) {
        // Fast path: existing inline pairing already correct.
        result.push(nextMsg as UserMessage)
        i++
        continue
      }
      repaired = true
      const patchedContent: (ContentBlockParam | ContentBlock)[] = [
        ...pairedBlocks,
        ...inlineNonToolResult,
      ]
      if (patchedContent.length > 0) {
        const patchedNext: UserMessage = {
          ...(nextMsg as UserMessage),
          message: {
            ...(nextMsg as UserMessage).message,
            content: patchedContent,
          },
        }
        result.push(
          checkStatsigFeatureGate_CACHED_MAY_BE_STALE('tengu_chair_sermon')
            ? smooshSystemReminderSiblings([patchedNext])[0]!
            : patchedNext,
        )
      } else {
        result.push(
          createUserMessage({
            content: NO_CONTENT_MESSAGE,
            isMeta: true,
          }),
        )
      }
      i++
    } else {
      repaired = true
      result.push(
        createUserMessage({
          content: pairedBlocks,
          isMeta: true,
        }),
      )
    }
  }

  if (repaired) {
    const messageTypes = messages.map((m, idx) => {
      if (m.type === 'assistant') {
        const toolUses = m.message.content
          .filter(b => b.type === 'tool_use')
          .map(b => (b as ToolUseBlock | ToolUseBlockParam).id)
        const serverToolUses = m.message.content
          .filter(
            b => b.type === 'server_tool_use' || b.type === 'mcp_tool_use',
          )
          .map(b => (b as { id: string }).id)
        const parts = [
          `id=${m.message.id}`,
          `tool_uses=[${toolUses.join(',')}]`,
        ]
        if (serverToolUses.length > 0) {
          parts.push(`server_tool_uses=[${serverToolUses.join(',')}]`)
        }
        return `[${idx}] assistant(${parts.join(', ')})`
      }
      if (m.type === 'user' && Array.isArray(m.message.content)) {
        const toolResults = m.message.content
          .filter(
            b =>
              typeof b === 'object' && 'type' in b && b.type === 'tool_result',
          )
          .map(b => (b as ToolResultBlockParam).tool_use_id)
        if (toolResults.length > 0) {
          return `[${idx}] user(tool_results=[${toolResults.join(',')}])`
        }
      }
      return `[${idx}] ${m.type}`
    })

    if (getStrictToolResultPairing()) {
      throw new Error(
        `ensureToolResultPairing: tool_use/tool_result pairing mismatch detected (strict mode). ` +
          `Refusing to repair -- would inject synthetic placeholders into model context. ` +
          `Message structure: ${messageTypes.join('; ')}. See inc-4977.`,
      )
    }

    logEvent('tengu_tool_result_pairing_repaired', {
      messageCount: messages.length,
      repairedMessageCount: result.length,
      messageTypes: messageTypes.join(
        '; ',
      ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    })
    logError(
      new Error(
        `ensureToolResultPairing: repaired missing tool_result blocks (${messages.length} -> ${result.length} messages). Message structure: ${messageTypes.join('; ')}`,
      ),
    )
  }

  return result
}
