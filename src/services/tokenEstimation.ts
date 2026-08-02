import type { Acosmi, BetaMessageParam as MessageParam } from '../types/api-types.js'
import type { Attachment } from '../utils/attachments.js'
import { logError } from '../utils/log.js'
import { normalizeAttachmentForAPI } from '../utils/messages.js'
import {
  getMainLoopModel,
  getSmallFastModel,
  normalizeModelStringForAPI,
} from '../utils/model/model.js'
import { assertRuntimeModel } from '../utils/model/runtimeModelResolution.js'
import { isNonGatewayModelReference } from '../utils/model/nonGatewayModelReference.js'
import { jsonStringify } from '../utils/slowOperations.js'
import { isToolReferenceBlock } from '../utils/toolSearch.js'
import { withTokenCountVCR } from './vcr.js'

// Minimal values for token counting with thinking enabled
// API constraint: max_tokens must be greater than thinking.budget_tokens
const TOKEN_COUNT_THINKING_BUDGET = 1024
const TOKEN_COUNT_MAX_TOKENS = 2048

/**
 * Check if messages contain thinking blocks
 */
function hasThinkingBlocks(
  messages: Acosmi.Beta.Messages.BetaMessageParam[],
): boolean {
  for (const message of messages) {
    if (message.role === 'assistant' && Array.isArray(message.content)) {
      for (const block of message.content) {
        if (
          typeof block === 'object' &&
          block !== null &&
          'type' in block &&
          (block.type === 'thinking' || block.type === 'redacted_thinking')
        ) {
          return true
        }
      }
    }
  }
  return false
}

/**
 * Strip tool search-specific fields from messages before sending for token counting.
 * This removes 'caller' from tool_use blocks and 'tool_reference' from tool_result content.
 * These fields are only valid with the tool search beta and will cause errors otherwise.
 *
 * Note: We use 'as unknown as' casts because the SDK types don't include tool search beta fields,
 * but at runtime these fields may exist from API responses when tool search was enabled.
 */
function stripToolSearchFieldsFromMessages(
  messages: Acosmi.Beta.Messages.BetaMessageParam[],
): Acosmi.Beta.Messages.BetaMessageParam[] {
  return messages.map(message => {
    if (!Array.isArray(message.content)) {
      return message
    }

    const normalizedContent = message.content.map(block => {
      // Strip 'caller' from tool_use blocks (assistant messages)
      if (block.type === 'tool_use') {
        // Destructure to exclude any extra fields like 'caller'
        const toolUse =
          block as Acosmi.Beta.Messages.BetaToolUseBlockParam & {
            caller?: unknown
          }
        return {
          type: 'tool_use' as const,
          id: toolUse.id,
          name: toolUse.name,
          input: toolUse.input,
        }
      }

      // Strip tool_reference blocks from tool_result content (user messages)
      if (block.type === 'tool_result') {
        const toolResult =
          block as Acosmi.Beta.Messages.BetaToolResultBlockParam
        if (Array.isArray(toolResult.content)) {
          const filteredContent = (toolResult.content as unknown[]).filter(
            c => !isToolReferenceBlock(c),
          ) as typeof toolResult.content

          if (filteredContent.length === 0) {
            return {
              ...toolResult,
              content: [{ type: 'text' as const, text: '[tool references]' }],
            }
          }
          if (filteredContent.length !== toolResult.content.length) {
            return {
              ...toolResult,
              content: filteredContent,
            }
          }
        }
      }

      return block
    })

    return {
      ...message,
      content: normalizedContent,
    }
  })
}

export async function countTokensWithAPI(
  content: string,
): Promise<number | null> {
  // Special case for empty content - API doesn't accept empty messages
  if (!content) {
    return 0
  }

  const message: Acosmi.Beta.Messages.BetaMessageParam = {
    role: 'user',
    content: content,
  }

  return countMessagesTokensWithAPI([message], [])
}

export async function countMessagesTokensWithAPI(
  messages: Acosmi.Beta.Messages.BetaMessageParam[],
  tools: Acosmi.Beta.Messages.BetaToolUnion[],
): Promise<number | null> {
  return withTokenCountVCR(messages, tools, async () => {
    try {
      // SDK 直调 token counting. countTokens API 无独立 SDK 端点, 用 chat.complete
      // max_tokens=1 触发服务端预估, 从 AcosmiUsage.input_tokens 读取.
      const model = assertRuntimeModel(getMainLoopModel(), {
        source: 'tokenEstimation.countMessagesTokensWithAPI',
      })
      // This sends the full message list to the gateway. Non-gateway models
      // must use the local rough estimator instead.
      if (isNonGatewayModelReference(model)) {
        return null
      }
      const containsThinking = hasThinkingBlocks(messages)

      const { chatComplete } = await import('./acosmi/client.js')
      const messagesToSend = messages.length > 0
        ? messages
        : [{ role: 'user' as const, content: 'foo' }]

      const resp = await chatComplete(normalizeModelStringForAPI(model), {
        rawMessages: messagesToSend,
        tools: tools.length > 0 ? (tools as unknown) : undefined,
        max_tokens: containsThinking ? TOKEN_COUNT_MAX_TOKENS : 1,
        thinking: containsThinking
          ? { type: 'enabled', budget_tokens: TOKEN_COUNT_THINKING_BUDGET }
          : undefined,
      })

      const inputTokens = resp.usage?.input_tokens
      if (typeof inputTokens !== 'number') {
        return null
      }
      return inputTokens
    } catch (error) {
      logError(error)
      return null
    }
  })
}

export function roughTokenCountEstimation(
  content: string,
  bytesPerToken: number = 4,
): number {
  return Math.round(content.length / bytesPerToken)
}

/**
 * Returns an estimated bytes-per-token ratio for a given file extension.
 * Dense JSON has many single-character tokens (`{`, `}`, `:`, `,`, `"`)
 * which makes the real ratio closer to 2 rather than the default 4.
 */
export function bytesPerTokenForFileType(fileExtension: string): number {
  switch (fileExtension) {
    case 'json':
    case 'jsonl':
    case 'jsonc':
      return 2
    default:
      return 4
  }
}

/**
 * Like {@link roughTokenCountEstimation} but uses a more accurate
 * bytes-per-token ratio when the file type is known.
 *
 * This matters when the API-based token count is unavailable and we
 * fall back to the rough estimate — an underestimate can
 * let an oversized tool result slip into the conversation.
 */
export function roughTokenCountEstimationForFileType(
  content: string,
  fileExtension: string,
): number {
  return roughTokenCountEstimation(
    content,
    bytesPerTokenForFileType(fileExtension),
  )
}

/**
 * Estimates token count for a Message object by extracting and analyzing its text content.
 * This provides a more reliable estimate than getTokenUsage for messages that may have been compacted.
 * Uses the SDK fast-mode helper for token counting (supports thinking blocks).
 */
export async function countTokensViaFastModeFallback(
  messages: Acosmi.Beta.Messages.BetaMessageParam[],
  tools: Acosmi.Beta.Messages.BetaToolUnion[],
): Promise<number | null> {
  // Check if messages contain thinking blocks
  const containsThinking = hasThinkingBlocks(messages)

  // Use the SDK fast-mode helper for token counting (supports thinking blocks).
  // WARNING: if you change this to use a non-fast-mode model, this request will fail in 1P unless it uses getCLISyspromptPrefix.
  // Note: We don't need the default-tier model for tool_reference blocks because we strip them via
  // stripToolSearchFieldsFromMessages() before sending.
  const model = assertRuntimeModel(getSmallFastModel(), {
    source: 'tokenEstimation.estimateTokensWithAPI',
  })
  // PR-2: same gateway-only invariant as countMessagesTokensWithAPI above.
  if (isNonGatewayModelReference(model)) {
    return null
  }

  // Strip tool search-specific fields (caller, tool_reference) before sending
  // These fields are only valid with the tool search beta header
  const normalizedMessages = stripToolSearchFieldsFromMessages(messages)

  const messagesToSend: MessageParam[] =
    normalizedMessages.length > 0
      ? (normalizedMessages as MessageParam[])
      : [{ role: 'user', content: 'count' }]

  // SDK 直调 token estimation
  const { chatComplete } = await import('./acosmi/client.js')
  const resp = await chatComplete(normalizeModelStringForAPI(model), {
    rawMessages: messagesToSend,
    tools: tools.length > 0 ? (tools as unknown) : undefined,
    max_tokens: containsThinking ? TOKEN_COUNT_MAX_TOKENS : 1,
    thinking: containsThinking
      ? { type: 'enabled', budget_tokens: TOKEN_COUNT_THINKING_BUDGET }
      : undefined,
  })
  const u = resp.usage
  return (u?.input_tokens ?? 0) +
    (u?.cache_creation_input_tokens ?? 0) +
    (u?.cache_read_input_tokens ?? 0)
}

export function roughTokenCountEstimationForMessages(
  messages: readonly {
    type: string
    message?: { content?: unknown }
    attachment?: Attachment
  }[],
): number {
  let totalTokens = 0
  for (const message of messages) {
    totalTokens += roughTokenCountEstimationForMessage(message)
  }
  return totalTokens
}

export function roughTokenCountEstimationForMessage(message: {
  type: string
  message?: { content?: unknown }
  attachment?: Attachment
}): number {
  if (
    (message.type === 'assistant' || message.type === 'user') &&
    message.message?.content
  ) {
    return roughTokenCountEstimationForContent(
      message.message?.content as
        | string
        | Array<Acosmi.ContentBlock>
        | Array<Acosmi.ContentBlockParam>
        | undefined,
    )
  }

  if (message.type === 'attachment' && message.attachment) {
    const userMessages = normalizeAttachmentForAPI(message.attachment)
    let total = 0
    for (const userMsg of userMessages) {
      total += roughTokenCountEstimationForContent(userMsg.message.content)
    }
    return total
  }

  return 0
}

function roughTokenCountEstimationForContent(
  content:
    | string
    | Array<Acosmi.ContentBlock>
    | Array<Acosmi.ContentBlockParam>
    | undefined,
): number {
  if (!content) {
    return 0
  }
  if (typeof content === 'string') {
    return roughTokenCountEstimation(content)
  }
  let totalTokens = 0
  for (const block of content) {
    totalTokens += roughTokenCountEstimationForBlock(block)
  }
  return totalTokens
}

function roughTokenCountEstimationForBlock(
  block: string | Acosmi.ContentBlock | Acosmi.ContentBlockParam,
): number {
  if (typeof block === 'string') {
    return roughTokenCountEstimation(block)
  }
  if (block.type === 'text') {
    return roughTokenCountEstimation(block.text)
  }
  if (block.type === 'image' || block.type === 'document') {
    // Image token estimate: tokens = (width px * height px)/750
    // Images are resized to max 2000x2000 (5333 tokens). Use a conservative
    // estimate that matches microCompact's IMAGE_MAX_TOKEN_SIZE to avoid
    // underestimating and triggering auto-compact too late.
    //
    // document: base64 PDF in source.data.  Must NOT reach the
    // jsonStringify catch-all — a 1MB PDF is ~1.33M base64 chars →
    // ~325k estimated tokens, vs the ~2000 the API actually charges.
    // Same constant as microCompact's calculateToolResultTokens.
    return 2000
  }
  if (block.type === 'tool_result') {
    return roughTokenCountEstimationForContent(block.content)
  }
  if (block.type === 'tool_use') {
    // input is the JSON the model generated — arbitrarily large (bash
    // commands, Edit diffs, file contents).  Stringify once for the
    // char count; the API re-serializes anyway so this is what it sees.
    return roughTokenCountEstimation(
      block.name + jsonStringify(block.input ?? {}),
    )
  }
  if (block.type === 'thinking') {
    return roughTokenCountEstimation(block.thinking)
  }
  if (block.type === 'redacted_thinking') {
    return roughTokenCountEstimation(block.data)
  }
  // server_tool_use, web_search_tool_result, mcp_tool_use, etc. —
  // text-like payloads (tool inputs, search results, no base64).
  // Stringify-length tracks the serialized form the API sees; the
  // key/bracket overhead is single-digit percent on real blocks.
  return roughTokenCountEstimation(jsonStringify(block))
}
