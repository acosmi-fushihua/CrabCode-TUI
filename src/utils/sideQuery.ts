
import type { Acosmi, BetaToolUnion } from '../types/api-types.js'
import {
  getLastApiCompletionTimestamp,
  setLastApiCompletionTimestamp,
} from '../bootstrap/state.js'
import type { QuerySource } from '../constants/querySource.js'
import type {
  ExactChatDestination,
  ProviderBoundEgressValidator,
} from '../services/acosmi/client.js'
import {
  getCLISyspromptPrefix,
} from '../constants/system.js'
import { HTTPError } from '@acosmi/sdk-ts'
import {
  getRateLimitKindFromError,
  getRetryAfter,
  getRetryDelay,
  TRANSIENT_429_MAX_ATTEMPTS,
} from '../services/api/withRetry.js'
import { logEvent } from '../services/analytics/index.js'
import type { AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS } from '../services/analytics/metadata.js'
import { isAcosmiSubscriber, isEnterpriseSubscriber } from './auth.js'
import { sleep } from './sleep.js'
import { normalizeModelStringForAPI } from './model/model.js'
import { isNonGatewayModelReference } from './model/nonGatewayModelReference.js'
import { assertRuntimeModel } from './model/runtimeModelResolution.js'

type MessageParam = Acosmi.MessageParam
type TextBlockParam = Acosmi.TextBlockParam
type Tool = Acosmi.Tool
type ToolChoice = Acosmi.ToolChoice
type BetaMessage = Acosmi.Beta.Messages.BetaMessage
// BetaJSONOutputFormat is not yet in the SDK typings; use a local placeholder.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type BetaJSONOutputFormat = any
type BetaThinkingConfigParam = Acosmi.Beta.Messages.BetaThinkingConfigParam

export type SideQueryOptions = {
  /** Model to use for the query */
  model: string
  /**
   * System prompt - string or array of text blocks (will be prefixed with CLI attribution).
   *
   * The attribution header is always placed in its own TextBlockParam block to ensure
   * server-side parsing correctly extracts the cc_entrypoint value without including
   * system prompt content.
   */
  system?: string | TextBlockParam[]
  /** Messages to send (supports cache_control on content blocks) */
  messages: MessageParam[]
  /** Optional tools (supports both standard Tool[] and BetaToolUnion[] for custom tool types) */
  tools?: Tool[] | BetaToolUnion[]
  /** Optional tool choice (use { type: 'tool', name: 'x' } for forced output) */
  tool_choice?: ToolChoice
  /** Optional JSON output format for structured responses */
  output_format?: BetaJSONOutputFormat
  /** Max tokens (default: 1024) */
  max_tokens?: number
  /** Max retries (default: 2) */
  maxRetries?: number
  /** Abort signal */
  signal?: AbortSignal
  /**
   * Exact provider/model approved for this privacy-sensitive egress. When
   * present, the real chat boundary revalidates the SDK's live catalog route
   * and performs a single non-replayed provider POST. Required for desktop
   * visual understanding; ordinary sideQuery callers remain unchanged.
   */
  exactDestination?: ExactChatDestination
  /** Canonical authorization re-check run synchronously at the final POST boundary. */
  validateEgress?: ProviderBoundEgressValidator
  /** Skip CLI system prompt prefix (keeps attribution header for OAuth). For internal classifiers that provide their own prompt. */
  skipSystemPromptPrefix?: boolean
  /** Temperature override */
  temperature?: number
  /** Thinking budget (enables thinking), or `false` to send `{ type: 'disabled' }`. */
  thinking?: number | false
  /** Stop sequences — generation stops when any of these strings is emitted */
  stop_sequences?: string[]
  /** Attributes this call in tengu_api_success for COGS joining against reporting.sampling_calls. */
  querySource: QuerySource
}

/**
 * Extract text from first user message for fingerprint computation.
 */
function extractFirstUserMessageText(messages: MessageParam[]): string {
  const firstUserMessage = messages.find(m => m.role === 'user')
  if (!firstUserMessage) return ''

  const content = firstUserMessage.content
  if (typeof content === 'string') return content

  // Array of content blocks - find first text block
  const textBlock = content.find(block => block.type === 'text')
  return textBlock?.type === 'text' ? textBlock.text : ''
}

/**
 * Lightweight API wrapper for "side queries" outside the main conversation loop.
 *
 * Use this instead of direct client.beta.messages.create() calls to ensure
 * proper OAuth token validation with fingerprint attribution headers.
 *
 * This handles:
 * - Fingerprint computation for OAuth validation
 * - Attribution header injection
 * - CLI system prompt prefix
 * - Proper betas for the model
 * - API metadata
 * - Model string normalization (strips [1m] suffix for API)
 *
 * @example
 * // Permission explainer
 * await sideQuery({ querySource: 'permission_explainer', model, system: SYSTEM_PROMPT, messages, tools, tool_choice })
 *
 * @example
 * // Session search
 * await sideQuery({ querySource: 'session_search', model, system: SEARCH_PROMPT, messages })
 *
 * @example
 * // Model validation
 * await sideQuery({ querySource: 'model_validation', model, max_tokens: 1, messages: [{ role: 'user', content: 'Hi' }] })
 */
export type SideQueryDeps = {
  /** Defaults to the real `chatComplete` from services/acosmi/client.js. */
  chatComplete?: (
    modelID: string,
    req: Record<string, unknown>,
    signal?: AbortSignal,
    exactDestination?: ExactChatDestination,
    validateEgress?: ProviderBoundEgressValidator,
  ) => Promise<unknown>
  /** Defaults to the real `systemToString` from services/acosmi/client.js. */
  systemToString?: (system: unknown) => string | undefined
  /** Defaults to the real abort-responsive `sleep` from ./sleep.js. */
  sleep?: (ms: number, signal?: AbortSignal) => Promise<void>
  /** Defaults to the real `isAcosmiSubscriber` from ./auth.js. */
  isAcosmiSubscriber?: () => boolean
  /** Defaults to the real `isEnterpriseSubscriber` from ./auth.js. */
  isEnterpriseSubscriber?: () => boolean
}

export async function sideQuery(
  opts: SideQueryOptions,
  deps: SideQueryDeps = {},
): Promise<BetaMessage> {
  const {
    model,
    system,
    messages,
    tools,
    tool_choice,
    output_format,
    max_tokens = 1024,
    maxRetries = 2,
    signal,
    exactDestination,
    validateEgress,
    skipSystemPromptPrefix,
    temperature,
    thinking,
    stop_sequences,
  } = opts

  // Build system blocks for prompt assembly
  const systemBlocks: TextBlockParam[] = [
    ...(skipSystemPromptPrefix
      ? []
      : [
          {
            type: 'text' as const,
            text: getCLISyspromptPrefix({
              isNonInteractive: false,
              hasAppendSystemPrompt: false,
            }),
          },
        ]),
    ...(Array.isArray(system)
      ? system
      : system
        ? [{ type: 'text' as const, text: system }]
        : []),
  ].filter((block): block is TextBlockParam => block !== null)

  let thinkingConfig: { type: string; budget_tokens?: number } | undefined
  if (thinking === false) {
    thinkingConfig = { type: 'disabled' }
  } else if (thinking !== undefined) {
    thinkingConfig = {
      type: 'enabled',
      budget_tokens: Math.min(thinking, max_tokens - 1),
    }
  }

  const runtimeModel = assertRuntimeModel(model, {
    source: 'sideQuery',
    querySource: opts.querySource,
  })
  const normalizedModel = normalizeModelStringForAPI(runtimeModel)
  if (
    opts.querySource === 'chat_media_understanding' &&
    exactDestination?.mainModelId === undefined
  ) {
    throw new Error(
      'sideQuery: chat media understanding requires a live-bound main provider plus exact sidecar destination',
    )
  }
  if (
    opts.querySource === 'chat_media_understanding' &&
    validateEgress === undefined
  ) {
    throw new Error(
      `sideQuery: ${opts.querySource} requires a final canonical egress validator`,
    )
  }
  if (
    exactDestination !== undefined &&
    exactDestination.modelId !== normalizedModel
  ) {
    throw new Error(
      `sideQuery: exact destination model does not match runtime model (${exactDestination.modelId} != ${normalizedModel})`,
    )
  }
  const start = Date.now()

  // Side queries ride the gateway SDK. A custom/local/account reference must
  // degrade locally instead of sending content to a destination the user did
  // not select.
  if (isNonGatewayModelReference(normalizedModel)) {
    console.warn(
      '[sideQuery] non-gateway model reference cannot ride the gateway; returning an empty result (destination details omitted)',
    )
    return {
      id: 'custom_local_model_side_query_skipped',
      type: 'message',
      role: 'assistant',
      model: normalizedModel,
      content: [],
      stop_reason: 'end_turn',
      usage: {
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
      },
    } as unknown as BetaMessage
  }

  // SDK 直调 — side queries 走 SDK Client.chatMessages.
  // SDK NetworkError catch 保留降级语义 (返回空 BetaMessage), 防止 7+
  // 依赖组件 (permission explainer / session search / model validation /
  // transcript classifier) 级联失败.
  const needsRealClient = !deps.chatComplete || !deps.systemToString
  const real = needsRealClient
    ? await import('../services/acosmi/client.js')
    : undefined
  const chatComplete = deps.chatComplete ?? real!.chatComplete
  const systemToString = deps.systemToString ?? real!.systemToString
  const doSleep = deps.sleep ?? ((ms: number, s?: AbortSignal) => sleep(ms, s))
  const checkAcosmiSubscriber = deps.isAcosmiSubscriber ?? isAcosmiSubscriber
  const checkEnterpriseSubscriber =
    deps.isEnterpriseSubscriber ?? isEnterpriseSubscriber

  // W-SIDEQUERY-RETRY (X1): `maxRetries` used to be a dead option (destructured,
  // never read) — callers like yoloClassifier.ts passed it believing sideQuery
  // retried transient errors internally (a false claim in its own docstring).
  // This bounds retries to 429s, only when
  // classified `transient` (quota still fails fast, unchanged), capped at
  // TRANSIENT_429_MAX_ATTEMPTS regardless of what a caller passes in, so no
  // caller can accidentally exceed the §8 ceiling.
  // V116.1 P1-3 (2026-07-24) 订正:识别类从本地 APIError(全仓零构造点,生产
  // 从未命中 —— X1 落地以来只在测试里跑过)改为真实 SDK HTTPError。429 是
  // 请求被拒、无副作用,短重试不违反 402/403/5xx 防重放不变量。
  const boundedMaxRetries = exactDestination
    ? 0
    : Math.max(0, Math.min(maxRetries, TRANSIENT_429_MAX_ATTEMPTS))

  let sdkResp
  for (let attempt = 0; ; attempt++) {
    try {
      sdkResp = await chatComplete(
        normalizedModel,
        {
          rawMessages: messages,
          system: systemToString(systemBlocks),
          tools: tools ? (tools as unknown) : undefined,
          max_tokens,
          temperature,
          thinking: thinkingConfig as never,
        },
        signal,
        exactDestination,
        validateEgress,
      )
      break
    } catch (err) {
      if (err instanceof Error && err.name === 'NetworkError') {
        // Provider-bound media/desktop calls are security-sensitive transactions:
        // swallowing the transport failure would let callers mistake an empty
        // observation for a successful, policy-bound response. Preserve the
        // generic side-query fail-soft contract only when no exact destination
        // was asserted.
        if (exactDestination) throw err
        console.warn(
          '[sideQuery] SDK network unavailable; returning an empty result (provider details omitted)',
        )
        // Return a minimal BetaMessage-compatible empty response so callers
        // degrade gracefully instead of crashing.
        return {
          id: 'sdk_network_unavailable',
          type: 'message',
          role: 'assistant',
          model: normalizedModel,
          content: [],
          stop_reason: 'end_turn',
          usage: {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
          },
        } as unknown as BetaMessage
      }

      const isRetryableTransient429 =
        err instanceof HTTPError &&
        err.statusCode === 429 &&
        checkAcosmiSubscriber() &&
        !checkEnterpriseSubscriber() &&
        getRateLimitKindFromError(err) === 'transient' &&
        attempt < boundedMaxRetries
      if (isRetryableTransient429) {
        await doSleep(getRetryDelay(attempt + 1, getRetryAfter(err)), signal)
        continue
      }
      throw err
    }
  }

  const now = Date.now()
  const lastCompletion = getLastApiCompletionTimestamp()
  const resp = sdkResp as unknown as BetaMessage
  logEvent('tengu_api_success', {
    requestId: undefined,
    querySource:
      opts.querySource as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    model:
      normalizedModel as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    inputTokens: resp.usage.input_tokens,
    outputTokens: resp.usage.output_tokens,
    cachedInputTokens: resp.usage.cache_read_input_tokens ?? 0,
    uncachedInputTokens: resp.usage.cache_creation_input_tokens ?? 0,
    durationMsIncludingRetries: now - start,
    timeSinceLastApiCallMs:
      lastCompletion !== null ? now - lastCompletion : undefined,
  })
  setLastApiCompletionTimestamp(now)

  return resp
}
