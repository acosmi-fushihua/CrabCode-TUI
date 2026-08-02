
import type { BetaUsage as Usage } from '../types/api-types.js'
import { roughTokenCountEstimationForMessages } from '../services/tokenEstimation.js'
import type { AssistantMessage, Message } from '../types/message.js'
import { SYNTHETIC_MESSAGES, SYNTHETIC_MODEL } from './messages.js'
import { jsonStringify } from './slowOperations.js'

export function getTokenUsage(message: Message): Usage | undefined {
  if (
    message?.type === 'assistant' &&
    'usage' in message.message &&
    !(
      message.message.content[0]?.type === 'text' &&
      SYNTHETIC_MESSAGES.has(message.message.content[0].text)
    ) &&
    message.message.model !== SYNTHETIC_MODEL
  ) {
    return message.message.usage
  }
  return undefined
}

/**
 * Get the API response id for an assistant message with real (non-synthetic) usage.
 * Used to identify split assistant records that came from the same API response —
 * when parallel tool calls are streamed, each content block becomes a separate
 * AssistantMessage record, but they all share the same message.id.
 */
function getAssistantMessageId(message: Message): string | undefined {
  if (
    message?.type === 'assistant' &&
    'id' in message.message &&
    message.message.model !== SYNTHETIC_MODEL
  ) {
    return message.message.id
  }
  return undefined
}

/**
 * Calculate total context window tokens from an API response's usage data.
 * Includes input_tokens + cache tokens + output_tokens.
 *
 * This represents the full context size at the time of that API call.
 * Use tokenCountWithEstimation() when you need context size from messages.
 */
export function getTokenCountFromUsage(usage: Usage): number {
  return (
    usage.input_tokens +
    (usage.cache_creation_input_tokens ?? 0) +
    (usage.cache_read_input_tokens ?? 0) +
    usage.output_tokens
  )
}

export function tokenCountFromLastAPIResponse(messages: Message[]): number {
  let i = messages.length - 1
  while (i >= 0) {
    const message = messages[i]
    const usage = message ? getTokenUsage(message) : undefined
    if (usage) {
      return getTokenCountFromUsage(usage)
    }
    i--
  }
  return 0
}

/**
 * Final context window size from the last API response's usage.iterations[-1].
 * Used for task_budget.remaining computation across compaction boundaries —
 * the server's budget countdown is context-based, so remaining decrements by
 * the pre-compact final window, not billing spend. See monorepo
 * api/api/sampling/prompt/renderer.py:292 for the server-side computation.
 *
 * Falls back to top-level input_tokens + output_tokens when iterations is
 * absent (no server-side tool loops, so top-level usage IS the final window).
 * Both paths exclude cache tokens to match #304930's formula.
 */
export function finalContextTokensFromLastResponse(
  messages: Message[],
): number {
  let i = messages.length - 1
  while (i >= 0) {
    const message = messages[i]
    const usage = message ? getTokenUsage(message) : undefined
    if (usage) {
      // Stainless types don't include iterations yet — cast like advisor.ts:43
      const iterations = (
        usage as {
          iterations?: Array<{
            input_tokens: number
            output_tokens: number
          }> | null
        }
      ).iterations
      if (iterations && iterations.length > 0) {
        const last = iterations.at(-1)!
        return last.input_tokens + last.output_tokens
      }
      // No iterations → no server tool loop → top-level usage IS the final
      // window. Match the iterations path's formula (input + output, no cache)
      // rather than getTokenCountFromUsage — #304930 defines final window as
      // non-cache input + output. Whether the server's budget countdown
      // (renderer.py:292 calculate_context_tokens) counts cache the same way
      // is an open question; aligning with the iterations path keeps the two
      // branches consistent until that's resolved.
      return usage.input_tokens + usage.output_tokens
    }
    i--
  }
  return 0
}

/**
 * Get only the output_tokens from the last API response.
 * This excludes input context (system prompt, tools, prior messages).
 *
 * WARNING: Do NOT use this for threshold comparisons (autocompact, session memory).
 * Use tokenCountWithEstimation() instead, which measures full context size.
 * This function is only useful for measuring how many tokens CrabCode generated
 * in a single response, not how full the context window is.
 */
export function messageTokenCountFromLastAPIResponse(
  messages: Message[],
): number {
  let i = messages.length - 1
  while (i >= 0) {
    const message = messages[i]
    const usage = message ? getTokenUsage(message) : undefined
    if (usage) {
      return usage.output_tokens
    }
    i--
  }
  return 0
}

export function getCurrentUsage(messages: Message[]): {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number
  cache_read_input_tokens: number
} | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i]
    const usage = message ? getTokenUsage(message) : undefined
    if (usage) {
      return {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens ?? 0,
        cache_read_input_tokens: usage.cache_read_input_tokens ?? 0,
      }
    }
  }
  return null
}

export function doesMostRecentAssistantMessageExceed200k(
  messages: Message[],
): boolean {
  const THRESHOLD = 200_000

  const lastAsst = messages.findLast(m => m.type === 'assistant')
  if (!lastAsst) return false
  const usage = getTokenUsage(lastAsst)
  return usage ? getTokenCountFromUsage(usage) > THRESHOLD : false
}

/**
 * Calculate the character content length of an assistant message.
 * Used for spinner token estimation (characters / 4 ≈ tokens).
 * This is used when subagent streaming events are filtered out and we
 * need to count content from completed messages instead.
 *
 * Counts the same content that handleMessageFromStream would count via deltas:
 * - text (text_delta)
 * - thinking (thinking_delta)
 * - redacted_thinking data
 * - tool_use input (input_json_delta)
 * Note: signature_delta is excluded from streaming counts (not model output).
 */
export function getAssistantMessageContentLength(
  message: AssistantMessage,
): number {
  let contentLength = 0
  for (const block of message.message.content) {
    if (block.type === 'text') {
      contentLength += block.text.length
    } else if (block.type === 'thinking') {
      contentLength += block.thinking.length
    } else if (block.type === 'redacted_thinking') {
      contentLength += block.data.length
    } else if (block.type === 'tool_use') {
      contentLength += jsonStringify(block.input).length
    }
  }
  return contentLength
}

/**
 * Get the current context window size in tokens.
 *
 * This is the CANONICAL function for measuring context size when checking
 * thresholds (autocompact, session memory init, etc.). Uses the last API
 * response's token count (input + output + cache) plus estimates for any
 * messages added since.
 *
 * Always use this instead of:
 * - Cumulative token counting (which double-counts as context grows)
 * - messageTokenCountFromLastAPIResponse (which only counts output_tokens)
 * - tokenCountFromLastAPIResponse (which doesn't estimate new messages)
 *
 * Implementation note on parallel tool calls: when the model makes multiple
 * tool calls in one response, the streaming code emits a SEPARATE assistant
 * record per content block (all sharing the same message.id and usage), and
 * the query loop interleaves each tool_result immediately after its tool_use.
 * So the messages array looks like:
 *   [..., assistant(id=A), user(result), assistant(id=A), user(result), ...]
 * If we stop at the LAST assistant record, we only estimate the one tool_result
 * after it and miss all the earlier interleaved tool_results — which will ALL
 * be in the next API request. To avoid undercounting, after finding a usage-
 * bearing record we walk back to the FIRST sibling with the same message.id
 * so every interleaved tool_result is included in the rough estimate.
 */
export function tokenCountWithEstimation(messages: readonly Message[]): number {
  let i = messages.length - 1
  while (i >= 0) {
    const message = messages[i]
    const usage = message ? getTokenUsage(message) : undefined
    // Defense-in-depth: a usage whose total
    // is 0 is never a real API response (input_tokens is always > 0 — system
    // prompt + tools are always present), so it can only be a fabricated/zero
    // usage that escaped the synthetic filter. Anchoring on it would zero the
    // context size (the root-cause class). Skip such anchors and keep walking
    // back / fall through to the content estimate, independent of how the
    // 0-usage was introduced. The primary fix is the SYNTHETIC_MODEL marker in
    // `threadItemsToMessages`; this guard closes the whole class structurally.
    if (message && usage && getTokenCountFromUsage(usage) > 0) {
      // Walk back past any earlier sibling records split from the same API
      // response (same message.id) so interleaved tool_results between them
      // are included in the estimation slice.
      const responseId = getAssistantMessageId(message)
      if (responseId) {
        let j = i - 1
        while (j >= 0) {
          const prior = messages[j]
          const priorId = prior ? getAssistantMessageId(prior) : undefined
          if (priorId === responseId) {
            // Earlier split of the same API response — anchor here instead.
            i = j
          } else if (priorId !== undefined) {
            // Hit a different API response — stop walking.
            break
          }
          // priorId === undefined: a user/tool_result/attachment message,
          // possibly interleaved between splits — keep walking.
          j--
        }
      }
      return (
        getTokenCountFromUsage(usage) +
        roughTokenCountEstimationForMessages(messages.slice(i + 1) as Parameters<typeof roughTokenCountEstimationForMessages>[0])
      )
    }
    i--
  }
  return roughTokenCountEstimationForMessages(messages as Parameters<typeof roughTokenCountEstimationForMessages>[0])
}

/**
 * Does `messages` contain at least one REAL API-response usage anchor?
 *
 * "Real" = a message whose `getTokenUsage` is defined AND whose total
 * (`getTokenCountFromUsage`) is > 0 — the same predicate
 * `tokenCountWithEstimation` uses to decide whether it can anchor on real usage
 * instead of falling through to a pure content estimate.
 *
 * Non-interactive runtimes may rebuild a fresh engine per turn and seed history
 * via `threadItemsToMessages`, which
 * fabricates all-ZERO usage (now SYNTHETIC_MODEL). At TURN ENTRY — before the
 * first API response of the turn — `engine.getMessages()` therefore contains
 * NO real usage anchor, so `tokenCountWithEstimation` falls to a pure CONTENT
 * estimate. But the dominant slice of real context (system prompt + tools +
 * memory ≈ 15K) lives in the request the gateway caches and reports back as
 * `cache_read_input_tokens` — it is NOT part of any reconstructed message's
 * content, so the content estimate STRUCTURALLY undercounts it (e.g. 604 vs a
 * real 15.7K). Emitting that deflated estimate as `effectiveContextTokens`
 * makes the topbar appear to "reset" each turn and the auto-compact threshold
 * unreachable. The worker uses this predicate to emit `effectiveContextTokens`
 * ONLY when anchored on real usage; with no anchor it emits nothing so the TUI
 * retains the previous turn's real value (or the `thread.lastTokenUsage`
 * placeholder), never the deflated estimate.
 */
export function hasRealUsageAnchor(messages: readonly Message[]): boolean {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i]
    const usage = message ? getTokenUsage(message) : undefined
    if (usage && getTokenCountFromUsage(usage) > 0) return true
  }
  return false
}
