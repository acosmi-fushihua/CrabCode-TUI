
// Stub types for SDK features not yet in the installed version
type BetaJSONOutputFormat = Record<string, unknown>
type BetaOutputConfig = Record<string, unknown>

import type {
  Acosmi,
  BetaContentBlock,
  BetaContentBlockParam,
  BetaMessage,
  BetaMessageParam as MessageParam,
  BetaMessageStreamParams,
  NormalizedAcosmiChatStreamEvent,
  BetaStopReason,
  BetaToolUnion,
  Stream,
} from '../../types/api-types.js'
import { randomUUID } from 'crypto'
import {
  getAPIProvider,
  isFirstPartyAcosmiBaseUrl,
  isOfficialProvider,
} from 'src/utils/model/providers.js'
import {
  getAttributionHeader,
  getCLISyspromptPrefix,
} from '../../constants/system.js'
import {
  getEmptyToolPermissionContext,
  type Tool,
  type Tools,
  toolMatchesName,
} from '../../Tool.js'
import type {
  ConnectorTextBlock,
} from '../../types/connectorText.js'
type ConnectorTextDelta = Record<string, unknown>
import { isConnectorTextBlock } from '../../types/connectorText.js'
import type {
  AssistantMessage,
  Message,
  StreamEvent,
  SystemAPIErrorMessage,
} from '../../types/message.js'
import {
  logAPIPrefix,
  toolToAPISchema,
} from '../../utils/api.js'
import {
  getMergedBetas,
} from '../../utils/betas.js'
import { resolveAppliedEffort } from '../../utils/effort.js'
import { isEnvDefinedFalsy, isEnvTruthy } from '../../utils/envUtils.js'
import { errorMessage } from '../../utils/errors.js'
import { computeFingerprintFromMessages } from '../../utils/fingerprint.js'
import { captureAPIRequest, logError } from '../../utils/log.js'
import {
  createAssistantAPIErrorMessage,
  createUserMessage,
  ensureToolResultPairing,
  normalizeContentFromAPI,
  normalizeMessagesForAPI,
  stripAdvisorBlocks,
  stripCallerFieldFromAssistantMessage,
  stripToolReferenceBlocksFromUserMessage,
} from '../../utils/messages.js'
import {
  getSmallFastModel,
  isMaxEffortModel,
} from '../../utils/model/model.js'
import { isLocalModelReference } from '../../utils/model/localModelReference.js'
import { parseAccountBridgeReference } from '../../utils/model/accountBridgeReference.js'
import { localModelChatStreamAdapter } from '../localModel/localModelChatStream.js'
import { resolveCustomModelRuntime } from '../../utils/model/customModelResolver.js'
import { customModelChatStreamAdapter } from '../customModel/customModelChatStream.js'
import { accountBridgeChatStreamAdapter } from '../accountBridge/accountBridgeChatStream.js'
import { cacheAccountBridgeRouteCapability } from '../accountBridge/capabilityCache.js'
import { resolveSessionAuxiliaryRoute } from './sessionAuxiliaryRouting.js'
import { canUseAccountBridge } from '../../utils/entitlements/accountBridge.js'
import { canUseCustomModels } from '../../utils/entitlements/customModels.js'
import { canUseLocalModels } from '../../utils/entitlements/localModels.js'
import { isNonGatewayModelReference } from '../../utils/model/nonGatewayModelReference.js'
import { DanglingModelReferenceError } from './danglingModelReferenceError.js'
import { assertRuntimeModel } from '../../utils/model/runtimeModelResolution.js'
import {
  asSystemPrompt,
  type SystemPrompt,
} from '../../utils/systemPromptType.js'
import { tokenCountFromLastAPIResponse } from '../../utils/tokens.js'
import { getDynamicConfig_BLOCKS_ON_INIT } from '../analytics/growthbook.js'
import {
  currentLimits,
  extractQuotaStatusFromError,
  extractQuotaStatusFromHeaders,
} from '../acosmiLimits.js'
import { getAPIContextManagement } from '../compact/apiMicrocompact.js'

/* eslint-disable @typescript-eslint/no-require-imports */
const autoModeStateModule = feature('TRANSCRIPT_CLASSIFIER')
  ? (require('../../utils/permissions/autoModeState.js') as typeof import('../../utils/permissions/autoModeState.js'))
  : null

import { feature } from '../../utils/featurePolyfill.js'
import {
  APIConnectionTimeoutError,
  APIUserAbortError,
} from '../../errors/api-errors.js'
import {
  normalizeGatewayError,
  normalizeSseErrorEvent,
} from './gatewayErrorNormalizer.js'
import {
  getAfkModeHeaderLatched,
  getCacheEditingHeaderLatched,
  getFastModeHeaderLatched,
  getLastApiCompletionTimestamp,
  getPromptCache1hAllowlist,
  getSessionId,
  getThinkingClearLatched,
  setAfkModeHeaderLatched,
  setCacheEditingHeaderLatched,
  setFastModeHeaderLatched,
  setLastMainRequestId,
  setThinkingClearLatched,
} from 'src/bootstrap/state.js'
import {
  AFK_MODE_BETA_HEADER,
  CONTEXT_MANAGEMENT_BETA_HEADER,
  FAST_MODE_BETA_HEADER,
  PROMPT_CACHING_SCOPE_BETA_HEADER,
  REDACT_THINKING_BETA_HEADER,
  STRUCTURED_OUTPUTS_BETA_HEADER,
} from 'src/constants/betas.js'
import type { QuerySource } from 'src/constants/querySource.js'
import { addToTotalSessionCost } from 'src/cost-tracker.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from 'src/services/analytics/growthbook.js'
import {
  ADVISOR_TOOL_INSTRUCTIONS,
  getExperimentAdvisorModels,
  isAdvisorEnabled,
  isValidAdvisorModel,
  modelSupportsAdvisor,
} from 'src/utils/advisor.js'
import { getAgentContext } from 'src/utils/agentContext.js'
import { clearKeychainCache, getAcosmiOAuthTokens, getMembershipGateInput, isAcosmiSubscriber } from 'src/utils/auth.js'
import { probeOauthProfile } from '../oauth/getOauthProfile.js'
import { getGlobalConfig } from '../../utils/config.js'
import {
  getToolSearchBetaHeader,
  modelSupportsStructuredOutputs,
  shouldIncludeFirstPartyOnlyBetas,
  shouldUseGlobalCacheScope,
} from 'src/utils/betas.js'
import { getMaxThinkingTokensForModel } from 'src/utils/context.js'
import { logForDebugging } from 'src/utils/debug.js'
import { logForDiagnosticsNoPII } from 'src/utils/diagLogs.js'
import {
  isFastModeAvailable,
  isFastModeCooldown,
  isFastModeEnabled,
  isFastModeSupportedByModel,
} from 'src/utils/fastMode.js'
import { returnValue } from 'src/utils/generators.js'
import { headlessProfilerCheckpoint } from 'src/utils/headlessProfiler.js'
import { isMcpInstructionsDeltaEnabled } from 'src/utils/mcpInstructionsDelta.js'
import { calculateUSDCost } from 'src/utils/modelCost.js'
import { endQueryProfile, queryCheckpoint } from 'src/utils/queryProfiler.js'
import {
  modelSupportsAdaptiveThinking,
  modelSupportsThinking,
  type ThinkingConfig,
} from 'src/utils/thinking.js'
import {
  extractDiscoveredToolNames,
  isDeferredToolsDeltaEnabled,
  isToolSearchEnabled,
} from 'src/utils/toolSearch.js'
import { API_MAX_MEDIA_PER_REQUEST } from '../../constants/apiLimits.js'
import { ADVISOR_BETA_HEADER } from '../../constants/betas.js'
import {
  formatDeferredToolLine,
  isDeferredTool,
  TOOL_SEARCH_TOOL_NAME,
} from '../../tools/ToolSearchTool/prompt.js'
import { count } from '../../utils/array.js'
import { getModelBetas } from '../../utils/betas.js'
import {
  normalizeModelStringForAPI,
  parseUserSpecifiedModel,
} from '../../utils/model/model.js'
import {
  startSessionActivity,
  stopSessionActivity,
} from '../../utils/sessionActivity.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import {
  isBetaTracingEnabled,
  type LLMRequestNewContext,
  startLLMRequestSpan,
} from '../../utils/telemetry/sessionTracing.js'
/* eslint-enable @typescript-eslint/no-require-imports */
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import {
  consumePendingCacheEdits,
  getPinnedCacheEdits,
  markToolsSentToAPIState,
  pinCacheEdits,
} from '../compact/microCompact.js'
import { getInitializationStatus } from '../lsp/manager.js'
import { withStreamingVCR, withVCR } from '../vcr.js'
import { HTTPError } from '@acosmi/sdk-ts'
import {
  AcosmiStreamDecodeError,
  chatComplete,
  chatStreamAdapter,
  systemToString,
  type ChatRequest,
} from '../acosmi/client.js'
// Y 路径 step 9 (2026-05-01): IPC 中转层下线, 所有 model call 走 SDK 直调.
// M1+M2 (2026-05-01): chat 401/403 → handleAuthExpiredInQuery 同步触发 cache cleanup,
// 替代旧 hub 的 auth.expired push (hub auth.* 已下线后不会从 chat 路径推送).
import { clearAuthRelatedCaches } from '../auth/localAuthState.js'
import {
  API_ERROR_MESSAGE_PREFIX,
  CUSTOM_OFF_SWITCH_MESSAGE,
  getAssistantMessageFromError,
  getErrorMessageIfRefusal,
  getGatewayEntitlementErrorMessage,
  getGatewayErrorDisplay,
  PHONE_BINDING_REQUIRED_MESSAGE,
  WINDOW_LIMIT_EXCEEDED_MESSAGE,
} from './errors.js'
import { formatMaxOutputTokensError } from './maxOutputTokensError.js'
import { isLockedGatewayModel } from '../../utils/model/modelCapabilities.js'
import {
  EMPTY_USAGE,
  type GlobalCacheStrategy,
  logAPIError,
  logAPIQuery,
  logAPISuccessAndDuration,
  type NonNullableUsage,
} from './logging.js'
import {
  CACHE_TTL_1HOUR_MS,
  checkResponseForCacheBreak,
  recordPromptState,
} from './promptCacheBreakDetection.js'
import {
  CannotRetryError,
  FallbackTriggeredError,
  type RetryContext,
  withRetry,
} from './withRetry.js'
import type { Options, FastModeOptions, QueryWithModelOptions, TaskBudgetParam } from './types.js'
import {
  getExtraBodyParams,
  getAPIMetadata,
  configureEffortParams,
  configureTaskBudgetParams,
  getMaxOutputTokensForModel,
  MAX_NON_STREAMING_TOKENS,
  adjustParamsForNonStreaming,
  getPromptCachingEnabled,
} from './params.js'
import {
  addCacheBreakpoints,
  buildSystemPromptBlocks,
  getCacheControl,
} from './cacheUtils.js'
import { updateUsage } from './usageUtils.js'
import { stripExcessMediaItems } from './messageProcessing.js'
import {
  applyMediaCapabilityPolicy,
  buildNoEligibleVisionPlaceholder,
} from './mediaCapabilityPolicy.js'
import {
  peekChatMediaSidecarCacheDetailed,
  runChatMediaSidecarDetailed,
} from './chatMediaSidecar.js'

/**
 * M1+M2 K6 (2026-05-01): chat 401/403 → 触发原 hub auth.expired push 的等价副作用.
 * M6 (2026-05-02) 后 setAuthExpiredHandler / hub push 链路全删, 本 fn 是其唯一替代:
 * clearKeychainCache + getAcosmiOAuthTokens.cache.clear + clearAuthRelatedCaches.
 *
 * SDK Client 内部 ensureToken 已对 401 自动 forceRefresh + 一次重试; 调用方收到
 * HTTPError 401/403 = SDK 重试后仍失败 (refresh token 已 revoke / 过期). 此处只清
 * cache, 不再 retry — 避免与 SDK 内部 retry 双层冲突. error 由调用方决定 re-throw
 * (走 withRetry 链路) 或 return false (verifyApiKey 路径).
 */
async function handleAuthExpiredInQuery(): Promise<void> {
  try {
    getAcosmiOAuthTokens.cache?.clear?.()
    clearKeychainCache()
    await clearAuthRelatedCaches()
  } catch {
    /* best-effort, 不阻塞 error propagation */
  }
}

const ACOSMI_CHAT_REQUEST_EXTRA_BODY_OWNED_KEYS = new Set([
  'model',
  'messages',
  'max_tokens',
  'system',
  'tools',
  'temperature',
  'thinking',
  'stream',
  'metadata',
  'betas',
  'speed',
  'output_config',
])

/**
 * Gateway effort upload contract (2026-07-30 audit).
 *
 * The Acosmi gateway consumes reasoning effort as a **top-level `effort:
 * {level}`** (4 tiers: low/medium/high/max, validated against its own
 * `validEffortLevels` allowlist), then translates it per provider preset in
 * sanitizer step 4.5 `normalizeThinkingAndEffort` — DeepSeek takes
 * `EffortToOutputConfig` + `EffortLevelAlias{low,medium→high}`, others take
 * `EffortPassthrough` or `EffortStrip`.
 *
 * `output_config.effort` is the **native Anthropic Messages** spelling, which
 * stays correct for the BYO / account-bridge / compatible-endpoint exits (see
 * `utils/effort.ts::modelSupportsEffort`) — it is simply not the gateway's
 * vocabulary. Sending it to the gateway lost the tier two ways: every
 * non-DeepSeek preset declares `SupportsOutputConfig=false`, so step 4
 * `stripTopLevelFields` dropped the whole object (tier silently gone), and the
 * DeepSeek lane never reached the alias table, forwarding the dialect-illegal
 * value `medium` (the Pro/Max/Team auto default) upstream verbatim.
 *
 * So the lift happens here and only here — this function is the sole gateway
 * boundary; `local:` / `custom:` / `account:` adapters all receive the raw
 * `params` and keep the Anthropic spelling untouched.
 */
function splitGatewayEffortFromOutputConfig(
  outputConfig: BetaMessageStreamParams['output_config'],
): {
  effort: ChatRequest['effort']
  outputConfig: ChatRequest['outputConfig'] | undefined
} {
  const source = outputConfig as Record<string, unknown> | undefined
  const rawEffort = source?.effort
  // Non-string tiers (the ant-only numeric override rides `acosmi_internal`,
  // never here) are left in place rather than silently dropped.
  if (typeof rawEffort !== 'string' || rawEffort === '') {
    return {
      effort: undefined,
      outputConfig: source as ChatRequest['outputConfig'] | undefined,
    }
  }
  const { effort: _lifted, ...rest } = source as Record<string, unknown>
  return {
    effort: { level: rawEffort },
    outputConfig:
      Object.keys(rest).length > 0
        ? (rest as ChatRequest['outputConfig'])
        : undefined,
  }
}

export function buildAcosmiChatRequestFromParams(
  params: BetaMessageStreamParams,
): ChatRequest {
  const extraBody: Record<string, unknown> = {}
  for (const [key, value] of Object.entries(params)) {
    if (
      value !== undefined &&
      !ACOSMI_CHAT_REQUEST_EXTRA_BODY_OWNED_KEYS.has(key)
    ) {
      extraBody[key] = value
    }
  }

  const { effort, outputConfig } = splitGatewayEffortFromOutputConfig(
    params.output_config,
  )

  return {
    rawMessages: params.messages,
    system: systemToString(params.system),
    tools: params.tools as unknown,
    max_tokens: params.max_tokens,
    ...(typeof params.stream === 'boolean' ? { stream: params.stream } : {}),
    temperature: params.temperature,
    thinking: params.thinking as ChatRequest['thinking'],
    metadata: params.metadata as Record<string, string> | undefined,
    ...(Array.isArray(params.betas) ? { betas: params.betas } : {}),
    ...(typeof params.speed === 'string' ? { speed: params.speed } : {}),
    ...(effort !== undefined ? { effort } : {}),
    ...(outputConfig !== undefined ? { outputConfig } : {}),
    ...(Object.keys(extraBody).length > 0 ? { extraBody } : {}),
  }
}

export async function verifyApiKey(
  _apiKey: string,
  isNonInteractiveSession: boolean,
): Promise<boolean> {
  // Skip API verification if running in print mode (isNonInteractiveSession)
  if (isNonInteractiveSession) {
    return true
  }

  // V116.1 P1-5 (2026-07-24):登录态验证改打 GET /api/oauth/profile —— 零计费。
  // 旧实现发送 max_tokens=1 的真实 chat 请求:每次验证都走网关 Hold/计费链,
  // 服务端权益链故障期(本次事故)验证请求自身 500,登录被误判失败。
  // 语义:仅 401 / 403(authentication_error) 判令牌失效(清 cache,与旧逻辑
  // 一致);404(老网关无路由)/其他 403/5xx/网络错误一律放行 —— 网关暂不可用
  // ≠ 登录失败,首个真实请求会经归一器呈现真实故障文案(§四:登录验证零计费)。
  try {
    const accessToken = getAcosmiOAuthTokens()?.accessToken
    if (!accessToken) {
      // 纯 API-key / 无 OAuth 令牌场景:profile 探针不适用。放行,首个真实
      // 请求暴露 auth 问题(与旧 NetworkError-skip 同语义,不做计费性探测)。
      logForDebugging(
        '[verifyApiKey] no OAuth access token; skipping zero-billing profile probe',
      )
      return true
    }
    const probe = await probeOauthProfile(accessToken)
    if (probe.kind === 'auth-expired') {
      await handleAuthExpiredInQuery()
      return false
    }
    if (probe.kind === 'unavailable') {
      logForDebugging(
        `[verifyApiKey] profile probe unavailable (status=${probe.status ?? 'network'}); gateway degraded — NOT a login failure`,
        { level: 'warn' },
      )
    }
    return true
  } catch (err) {
    logError(err)
    throw err
  }
}

export async function queryModelWithoutStreaming({
  messages,
  systemPrompt,
  thinkingConfig,
  tools,
  signal,
  options,
}: {
  messages: Message[]
  systemPrompt: SystemPrompt
  thinkingConfig: ThinkingConfig
  tools: Tools
  signal: AbortSignal
  options: Options
}): Promise<AssistantMessage> {
  // Store the assistant message but continue consuming the generator to ensure
  // logAPISuccessAndDuration gets called (which happens after all yields)
  let assistantMessage: AssistantMessage | undefined
  for await (const message of withStreamingVCR(messages, async function* () {
    yield* queryModel(
      messages,
      systemPrompt,
      thinkingConfig,
      tools,
      signal,
      options,
    )
  })) {
    if (message.type === 'assistant') {
      assistantMessage = message
    }
  }
  if (!assistantMessage) {
    // If the signal was aborted, throw APIUserAbortError instead of a generic error
    // This allows callers to handle abort scenarios gracefully
    if (signal.aborted) {
      throw new APIUserAbortError()
    }
    throw new Error('No assistant message found')
  }
  return assistantMessage
}

export async function* queryModelWithStreaming({
  messages,
  systemPrompt,
  thinkingConfig,
  tools,
  signal,
  options,
}: {
  messages: Message[]
  systemPrompt: SystemPrompt
  thinkingConfig: ThinkingConfig
  tools: Tools
  signal: AbortSignal
  options: Options
}): AsyncGenerator<
  StreamEvent | AssistantMessage | SystemAPIErrorMessage,
  void
> {
  return yield* withStreamingVCR(messages, async function* () {
    yield* queryModel(
      messages,
      systemPrompt,
      thinkingConfig,
      tools,
      signal,
      options,
    )
  })
}

/**
 * Determines if an LSP tool should be deferred (tool appears with defer_loading: true)
 * because LSP initialization is not yet complete.
 */
function shouldDeferLspTool(tool: Tool): boolean {
  if (!('isLsp' in tool) || !tool.isLsp) {
    return false
  }
  const status = getInitializationStatus()
  // Defer when pending or not started
  return status.status === 'pending' || status.status === 'not-started'
}

/**
 * Per-attempt timeout for non-streaming fallback requests, in milliseconds.
 * Reads API_TIMEOUT_MS when set so slow backends and the streaming path
 * share the same ceiling.
 *
 * Remote sessions default to 120s to stay under CCR's container idle-kill
 * (~5min) so a hung fallback to a wedged backend surfaces a clean
 * APIConnectionTimeoutError instead of stalling past SIGKILL.
 *
 * Otherwise defaults to 300s — long enough for slow backends without
 * approaching the API's 10-minute non-streaming boundary.
 */
function getNonstreamingFallbackTimeoutMs(): number {
  const override = parseInt(process.env.API_TIMEOUT_MS || '', 10)
  if (override) return override
  return isEnvTruthy(process.env.CRABCODE_REMOTE) ? 120_000 : 300_000
}

/**
 * Helper generator for non-streaming API requests.
 * Encapsulates the common pattern of creating a withRetry generator,
 * iterating to yield system messages, and returning the final BetaMessage.
 */
export async function* executeNonStreamingRequest(
  clientOptions: {
    model: string
    fetchOverride?: Options['fetchOverride']
    source: string
  },
  retryOptions: {
    model: string
    fallbackModel?: string
    thinkingConfig: ThinkingConfig
    fastMode?: boolean
    signal: AbortSignal
    initialConsecutive529Errors?: number
    querySource?: QuerySource
  },
  paramsFromContext: (context: RetryContext) => BetaMessageStreamParams,
  onAttempt: (attempt: number, start: number, maxOutputTokens: number) => void,
  captureRequest: (params: BetaMessageStreamParams) => void,
  /**
   * Request ID of the failed streaming attempt this fallback is recovering
   * from. Emitted in tengu_nonstreaming_fallback_error for funnel correlation.
   */
  originatingRequestId?: string | null,
): AsyncGenerator<SystemAPIErrorMessage, BetaMessage> {
  const initialNonGatewayModel = [clientOptions.model, retryOptions.model].find(
    model => isNonGatewayModelReference(model),
  )
  if (initialNonGatewayModel) {
    throw new DanglingModelReferenceError(initialNonGatewayModel)
  }

  const fallbackTimeoutMs = getNonstreamingFallbackTimeoutMs()
  const generator = withRetry(
    () => Promise.resolve(null as unknown as Acosmi),
    async (_acosmi, attempt, context) => {
      const start = Date.now()
      const retryParams = paramsFromContext(context)
      if (isNonGatewayModelReference(retryParams.model)) {
        throw new DanglingModelReferenceError(retryParams.model)
      }
      captureRequest(retryParams)
      onAttempt(attempt, start, retryParams.max_tokens)

      const adjustedParams = adjustParamsForNonStreaming(
        retryParams,
        MAX_NON_STREAMING_TOKENS,
      )

      try {
        // SDK 直调非流式 fallback. AcosmiResponse 字段与 BetaMessage 一致 — 直接断言.
        const resp = await chatComplete(
          normalizeModelStringForAPI(adjustedParams.model),
          buildAcosmiChatRequestFromParams(adjustedParams),
        )
        return resp as unknown as BetaMessage
      } catch (err) {
        // User aborts are not errors — re-throw immediately without logging
        if (err instanceof APIUserAbortError) throw err

        // M1+M2 K6 (2026-05-01) + 根因修 2026-05-05: 仅真 auth-expired 才清 cache.
        // 与流式路径 line 2167-2170 对齐: 401 一律算 auth-expired (网关约定);
        // 403 仅当 type === 'authentication_error' 才算 auth-expired, 其他 403
        // (permission_error / not_found_error / 空 type) 是有效 token 下的 RBAC/
        // 资源/作用域问题, 清 cache 反而会让 picker 永空 + 模型回退失败死循环。
        if (err instanceof HTTPError) {
          const isAuthExpired =
            err.statusCode === 401 ||
            (err.statusCode === 403 && err.type === 'authentication_error')
          if (isAuthExpired) {
            await handleAuthExpiredInQuery()
          }
          if (err.statusCode === 401 || err.statusCode === 403) {
            throw err
          }
        }

        // Instrumentation: record when the non-streaming request errors (including
        // timeouts). Lets us distinguish "fallback hung past container kill"
        // (no event) from "fallback hit the bounded timeout" (this event).
        logForDiagnosticsNoPII('error', 'cli_nonstreaming_fallback_error')
        logEvent('tengu_nonstreaming_fallback_error', {
          model:
            clientOptions.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          error:
            err instanceof Error
              ? (err.name as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
              : ('unknown' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS),
          attempt,
          timeout_ms: fallbackTimeoutMs,
          request_id: (originatingRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
        throw err
      }
    },
    {
      model: retryOptions.model,
      fallbackModel: retryOptions.fallbackModel,
      thinkingConfig: retryOptions.thinkingConfig,
      ...(isFastModeEnabled() && { fastMode: retryOptions.fastMode }),
      signal: retryOptions.signal,
      initialConsecutive529Errors: retryOptions.initialConsecutive529Errors,
      querySource: retryOptions.querySource,
    },
  )

  let e
  do {
    e = await generator.next()
    if (!e.done && e.value.type === 'system') {
      yield e.value
    }
  } while (!e.done)

  return e.value as BetaMessage
}

/**
 * Extracts the request ID from the most recent assistant message in the
 * conversation. Used to link consecutive API requests in analytics so we can
 * join them for cache-hit-rate analysis and incremental token tracking.
 *
 * Deriving this from the message array (rather than global state) ensures each
 * query chain (main thread, subagent, teammate) tracks its own request chain
 * independently, and rollback/undo naturally updates the value.
 */
function getPreviousRequestIdFromMessages(
  messages: Message[],
): string | undefined {
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i]!
    if (msg.type === 'assistant' && msg.requestId) {
      return msg.requestId
    }
  }
  return undefined
}

async function* queryModel(
  messages: Message[],
  systemPrompt: SystemPrompt,
  thinkingConfig: ThinkingConfig,
  tools: Tools,
  signal: AbortSignal,
  options: Options,
): AsyncGenerator<
  StreamEvent | AssistantMessage | SystemAPIErrorMessage,
  void
> {
  options = {
    ...options,
    model: assertRuntimeModel(options.model, {
      source: 'queryModel',
      querySource: options.querySource,
    }),
  }
  if (options.accountBridgeRuntimeAccess) {
    const boundRouteId = parseAccountBridgeReference(options.model)
    if (boundRouteId !== options.accountBridgeRuntimeAccess.route.routeId) {
      throw new Error(
        'queryModel: account bridge runtime access does not match the selected account route',
      )
    }
    cacheAccountBridgeRouteCapability(options.accountBridgeRuntimeAccess.route)
  }

  // `local:<id>` references are served by the TUI-managed loopback
  // inference server. They are routed to localModelChatStreamAdapter in the
  // streaming withRetry callback below, so the literal `local:` string never
  // reaches chatStreamAdapter / the Acosmi SDK.

  // Check cheap conditions first — the off-switch await blocks on GrowthBook
  // init (~10ms). For non-max-effort tiers this skips the await
  // entirely. Subscribers don't hit this path at all.
  if (
    !isAcosmiSubscriber() &&
    isMaxEffortModel(options.model) &&
    (
      await getDynamicConfig_BLOCKS_ON_INIT<{ activated: boolean }>(
        'tengu-off-switch',
        {
          activated: false,
        },
      )
    ).activated
  ) {
    logEvent('tengu_off_switch_query', {})
    yield getAssistantMessageFromError(
      new Error(CUSTOM_OFF_SWITCH_MESSAGE),
      options.model,
    )
    return
  }

  // W-TUI-LOCKED-GATEWAY-MODELS D6① (2026-07-05): locked-model pre-flight.
  // A gateway model the account has NOT unlocked (membership expired / tier
  // too low) would be rejected server-side anyway — today as a misleading
  // generic 500 — so never send the doomed request. Sits in queryModel, the
  // single funnel for TUI, non-interactive, and subagent forms
  // flow through. isLockedGatewayModel is exact-slug-match by design (a
  // pre-flight must never false-positive a legitimate request); `local:<id>`
  // refs are served by the loopback adapter, not the gateway — skip.
  //
  // 2026-07-27: 该预检的正当性来自「能可靠预测服务端会拒」。生产实证推翻了这条
  // 预测对达标会员的成立性(locked 由「无权益桶」产生,而准入走 credit 链根本不
  // 看桶;被标锁的模型实际调用成功),于是预检把展示缺陷升格成了不可绕过的封锁。
  // 收窄**不在此处**:`getLockedGatewayModelIds` 是本谓词与 TUI 选择闸的单一取
  // 数点,达标会员在那里即得空集,四形态一并收窄;在这里再判一次只会制造第二处
  // 需要同步的判据。未达 Plus 地板的账号仍按原样拦截(免费档 allowedModels 白
  // 名单确会在 hold 期拒)。
  if (
    !isNonGatewayModelReference(options.model) &&
    isLockedGatewayModel(options.model)
  ) {
    logEvent('tengu_locked_model_preflight_block', {})
    yield getAssistantMessageFromError(
      new Error(getGatewayEntitlementErrorMessage(options.model)),
      options.model,
    )
    return
  }

  // V116.1 P0-3 (2026-07-24): 手机绑定预检。profile 的 requires_phone_binding
  // === true 表示账号未绑定手机号(注册赠送权益领取前置),网关侧发送必被权益
  // 链拒绝 —— 与其让用户看到「无权益」误导文案,预检直接给出固定绑定引导。
  // 仅 `=== true` 拦截:老网关不下发该字段(undefined)不得拦;`local:<id>`
  // 引用走本地回环、与网关权益无关,跳过(同上方 locked 预检的守卫)。
  // 解除路径:重新登录强刷 profile(计划 §三 P0-3.2),或 token refresh 周期
  // 内 fetchProfileInfo 传播 false。
  if (
    !isNonGatewayModelReference(options.model) &&
    getGlobalConfig().oauthAccount?.requiresPhoneBinding === true
  ) {
    logEvent('tengu_phone_binding_preflight_block', {})
    yield getAssistantMessageFromError(
      new Error(PHONE_BINDING_REQUIRED_MESSAGE),
      options.model,
    )
    return
  }

  // Derive previous request ID from the last assistant message in this query chain.
  // This is scoped per message array (main thread, subagent, teammate each have their own),
  // so concurrent agents don't clobber each other's request chain tracking.
  // Also naturally handles rollback/undo since removed messages won't be in the array.
  const previousRequestId = getPreviousRequestIdFromMessages(messages)

  const resolvedModel = options.model

  queryCheckpoint('query_tool_schema_build_start')
  const isAgenticQuery =
    options.querySource.startsWith('repl_main_thread') ||
    options.querySource.startsWith('agent:') ||
    options.querySource === 'sdk' ||
    options.querySource === 'hook_agent' ||
    options.querySource === 'verification_agent'
  const betas = getMergedBetas(options.model, { isAgenticQuery })

  // Always send the advisor beta header when advisor is enabled, so
  // non-agentic queries (compact, side_question, extract_memories, etc.)
  // can parse advisor server_tool_use blocks already in the conversation history.
  if (isAdvisorEnabled()) {
    betas.push(ADVISOR_BETA_HEADER)
  }

  let advisorModel: string | undefined
  if (isAgenticQuery && isAdvisorEnabled()) {
    let advisorOption = options.advisorModel

    const advisorExperiment = getExperimentAdvisorModels()
    if (advisorExperiment !== undefined) {
      if (
        normalizeModelStringForAPI(advisorExperiment.baseModel) ===
        normalizeModelStringForAPI(options.model)
      ) {
        // Override the advisor model if the base model matches. We
        // should only have experiment models if the user cannot
        // configure it themselves.
        advisorOption = advisorExperiment.advisorModel
      }
    }

    if (advisorOption) {
      const normalizedAdvisorModel = normalizeModelStringForAPI(
        parseUserSpecifiedModel(advisorOption),
      )
      if (!modelSupportsAdvisor(options.model)) {
        logForDebugging(
          `[AdvisorTool] Skipping advisor - base model ${options.model} does not support advisor`,
        )
      } else if (!isValidAdvisorModel(normalizedAdvisorModel)) {
        logForDebugging(
          `[AdvisorTool] Skipping advisor - ${normalizedAdvisorModel} is not a valid advisor model`,
        )
      } else {
        advisorModel = normalizedAdvisorModel
        logForDebugging(
          `[AdvisorTool] Server-side tool enabled with ${advisorModel} as the advisor model`,
        )
      }
    }
  }

  // Check if tool search is enabled (checks mode, model support, and threshold for auto mode)
  // This is async because it may need to calculate MCP tool description sizes for TstAuto mode
  let useToolSearch = await isToolSearchEnabled(
    options.model,
    tools,
    options.getToolPermissionContext,
    options.agents,
    'query',
  )

  // Precompute once — isDeferredTool does 2 GrowthBook lookups per call
  const deferredToolNames = new Set<string>()
  if (useToolSearch) {
    for (const t of tools) {
      if (isDeferredTool(t)) deferredToolNames.add(t.name)
    }
  }

  // Even if tool search mode is enabled, skip if there are no deferred tools
  // AND no MCP servers are still connecting. When servers are pending, keep
  // ToolSearch available so the model can discover tools after they connect.
  if (
    useToolSearch &&
    deferredToolNames.size === 0 &&
    !options.hasPendingMcpServers
  ) {
    logForDebugging(
      'Tool search disabled: no deferred tools available to search',
    )
    useToolSearch = false
  }

  // Filter out ToolSearchTool if tool search is not enabled for this model
  // ToolSearchTool returns tool_reference blocks which unsupported models can't handle
  let filteredTools: Tools

  if (useToolSearch) {
    // Dynamic tool loading: Only include deferred tools that have been discovered
    // via tool_reference blocks in the message history. This eliminates the need
    // to predeclare all deferred tools upfront and removes limits on tool quantity.
    const discoveredToolNames = extractDiscoveredToolNames(messages)

    filteredTools = tools.filter(tool => {
      // Always include non-deferred tools
      if (!deferredToolNames.has(tool.name)) return true
      // Always include ToolSearchTool (so it can discover more tools)
      if (toolMatchesName(tool, TOOL_SEARCH_TOOL_NAME)) return true
      // Only include deferred tools that have been discovered
      return discoveredToolNames.has(tool.name)
    })
  } else {
    filteredTools = tools.filter(
      t => !toolMatchesName(t, TOOL_SEARCH_TOOL_NAME),
    )
  }

  // Add tool search beta header if enabled - required for defer_loading to be accepted
  // Header differs by provider: 1P/Foundry use advanced-tool-use, Vertex/Bedrock use tool-search-tool
  // For Bedrock, this header must go in extraBodyParams, not the betas array
  const toolSearchHeader = useToolSearch ? getToolSearchBetaHeader() : null
  if (toolSearchHeader && getAPIProvider() !== 'bedrock') {
    if (!betas.includes(toolSearchHeader)) {
      betas.push(toolSearchHeader)
    }
  }

  // Determine if cached microcompact is enabled for this model.
  // Computed once here (in async context) and captured by paramsFromContext.
  // The beta header is also captured here to avoid a top-level import of the
  // ant-only CACHE_EDITING_BETA_HEADER constant.
  let cachedMCEnabled = false
  let cacheEditingBetaHeader = ''
  if (feature('CACHED_MICROCOMPACT')) {
    const {
      isCachedMicrocompactEnabled,
      isModelSupportedForCacheEditing,
      getCachedMCConfig,
    } = await import('../compact/cachedMicrocompact.js')
    const betas = await import('src/constants/betas.js')
    cacheEditingBetaHeader = betas.CACHE_EDITING_BETA_HEADER
    const featureEnabled = isCachedMicrocompactEnabled()
    const modelSupported = isModelSupportedForCacheEditing(options.model)
    cachedMCEnabled = featureEnabled && modelSupported
    const config = getCachedMCConfig()
    logForDebugging(
      `Cached MC gate: enabled=${featureEnabled} modelSupported=${modelSupported} model=${options.model} supportedModels=${jsonStringify(config.supportedModels)}`,
    )
  }

  const useGlobalCacheFeature = shouldUseGlobalCacheScope()
  const willDefer = (t: Tool) =>
    useToolSearch && (deferredToolNames.has(t.name) || shouldDeferLspTool(t))
  // MCP tools are per-user → dynamic tool section → can't globally cache.
  // Only gate when an MCP tool will actually render (not defer_loading).
  const needsToolBasedCacheMarker =
    useGlobalCacheFeature &&
    filteredTools.some(t => t.isMcp === true && !willDefer(t))

  // Ensure prompt_caching_scope beta header is present when global cache is enabled.
  if (
    useGlobalCacheFeature &&
    !betas.includes(PROMPT_CACHING_SCOPE_BETA_HEADER)
  ) {
    betas.push(PROMPT_CACHING_SCOPE_BETA_HEADER)
  }

  // Determine global cache strategy for logging
  const globalCacheStrategy: GlobalCacheStrategy = useGlobalCacheFeature
    ? needsToolBasedCacheMarker
      ? 'none'
      : 'system_prompt'
    : 'none'

  // Build tool schemas, adding defer_loading for MCP tools when tool search is enabled
  // Note: We pass the full `tools` list (not filteredTools) to toolToAPISchema so that
  // ToolSearchTool's prompt can list ALL available MCP tools. The filtering only affects
  // which tools are actually sent to the API, not what the model sees in tool descriptions.
  const toolSchemas = await Promise.all(
    filteredTools.map(tool =>
      toolToAPISchema(tool, {
        getToolPermissionContext: options.getToolPermissionContext,
        tools,
        agents: options.agents,
        allowedAgentTypes: options.allowedAgentTypes,
        model: options.model,
        deferLoading: willDefer(tool),
      }),
    ),
  )

  if (useToolSearch) {
    const includedDeferredTools = count(filteredTools, t =>
      deferredToolNames.has(t.name),
    )
    logForDebugging(
      `Dynamic tool loading: ${includedDeferredTools}/${deferredToolNames.size} deferred tools included`,
    )
  }

  queryCheckpoint('query_tool_schema_build_end')

  // Normalize messages before building system prompt (needed for fingerprinting)
  // Instrumentation: Track message count before normalization
  logEvent('tengu_api_before_normalize', {
    preNormalizedMessageCount: messages.length,
  })

  queryCheckpoint('query_message_normalization_start')
  let messagesForAPI = normalizeMessagesForAPI(messages, filteredTools)
  queryCheckpoint('query_message_normalization_end')

  // Model-specific post-processing: strip tool-search-specific fields if the
  // selected model doesn't support tool search.
  //
  // Why is this needed in addition to normalizeMessagesForAPI?
  // - normalizeMessagesForAPI uses isToolSearchEnabledNoModelCheck() because it's
  //   called from ~20 places (analytics, feedback, sharing, etc.), many of which
  //   don't have model context. Adding model to its signature would be a large refactor.
  // - This post-processing uses the model-aware isToolSearchEnabled() check
  // - This handles mid-conversation model switching (e.g., default-tier → fast-mode) where
  //   stale tool-search fields from the previous model would cause 400 errors
  //
  // Note: For assistant messages, normalizeMessagesForAPI already normalized the
  // tool inputs, so stripCallerFieldFromAssistantMessage only needs to remove the
  // 'caller' field (not re-normalize inputs).
  if (!useToolSearch) {
    messagesForAPI = messagesForAPI.map(msg => {
      switch (msg.type) {
        case 'user':
          // Strip tool_reference blocks from tool_result content
          return stripToolReferenceBlocksFromUserMessage(msg)
        case 'assistant':
          // Strip 'caller' field from tool_use blocks
          return stripCallerFieldFromAssistantMessage(msg)
        default:
          return msg
      }
    })
  }

  // Repair tool_use/tool_result pairing mismatches that can occur when resuming
  // resumed TUI sessions. Inserts synthetic error tool_results for orphaned
  // tool_uses and strips orphaned tool_results referencing non-existent tool_uses.
  messagesForAPI = ensureToolResultPairing(messagesForAPI)

  // Strip advisor blocks — the API rejects them without the beta header.
  if (!betas.includes(ADVISOR_BETA_HEADER)) {
    messagesForAPI = stripAdvisorBlocks(messagesForAPI)
  }

  // Strip excess media items before making the API call.
  // The API rejects requests with >100 media items but returns a confusing error.
  // Rather than erroring (which is hard to recover from in Cowork/CCD), we
  // silently drop the oldest media items to stay within the limit.
  messagesForAPI = stripExcessMediaItems(
    messagesForAPI,
    API_MAX_MEDIA_PER_REQUEST,
  )

  // Capability-aware multimodal media policy (W-MULTIMODAL-INPUT). For the
  // ACTUAL per-request model (main loop AND subagents flow through here), the
  // single capability-aware layer that decides what to do with image blocks
  // the model cannot consume. Runs ONCE per turn (before withRetry, so retries
  // reuse the degraded messages) and is NOT on sideQuery's path, so the vision
  // sidecar's own image send is never gated. Document (PDF) blocks are
  // intentionally NOT gated (gateway parses them for all models); PDF
  // page-images are image blocks and so flow through this same policy.
  // 2026-07-24 F3a: `no_eligible_model` is a model-level condition (the
  // sidecar's destination resolution depends only on main model + catalog +
  // consent, never on the image), so one turn-scoped flag is exact. When any
  // sidecar call reports it, every placeholder this turn carries the reason
  // and the "switch to a vision-capable main model" hint instead of the bare
  // omission text.
  let sidecarHadNoEligibleModel = false
  const mediaCapabilityResult = await applyMediaCapabilityPolicy(
    messagesForAPI,
    options.model,
    {
      // Chat vision sidecar — default ON (PR-CC5); CRABCODE_CHAT_VISION_SIDECAR
      // is a kill switch. When killed / no eligible vision model exists,
      // runChatMediaSidecar returns null and the policy falls back to the
      // honest placeholder.
      degradeImage: image =>
        runChatMediaSidecarDetailed(image, {
          signal,
          mainModel: options.model,
          onOutcome: outcome => {
            if (outcome.kind === 'no_eligible_model') {
              sidecarHadNoEligibleModel = true
            }
          },
        }),
      // PR-3b: already-described images (chat history re-sends every turn)
      // resolve synchronously without occupying a degrade-concurrency slot.
      peekCached: peekChatMediaSidecarCacheDetailed,
      buildDegradePlaceholder: (_image, ctx) =>
        sidecarHadNoEligibleModel
          ? buildNoEligibleVisionPlaceholder(ctx.model)
          : null,
    },
  )
  messagesForAPI = mediaCapabilityResult.messages
  if (mediaCapabilityResult.degradedImages > 0) {
    // V2: distinguish a WORKING vision fallback (sidecarDescribed) from a
    // BROKEN one (placeholderFallback — sidecar off / no eligible model /
    // failed). Before, both collapsed into `degradedImages`, so a user whose
    // vision fallback was silently broken looked identical to one it worked
    // for. `placeholderFallback > 0` on a text-only turn is the actionable
    // "the vision fallback is not producing descriptions" signal.
    logForDebugging(
      `[queryModel] media policy: degraded ${mediaCapabilityResult.degradedImages} image block(s) for text-only model ${options.model} ` +
        `(sidecar-described=${mediaCapabilityResult.sidecarDescribed}, placeholder-fallback=${mediaCapabilityResult.placeholderFallback})`,
    )
    logEvent('tengu_media_capability_degraded', {
      degradedImages: mediaCapabilityResult.degradedImages,
      imageCount: mediaCapabilityResult.imageCount,
      sidecarDescribed: mediaCapabilityResult.sidecarDescribed,
      placeholderFallback: mediaCapabilityResult.placeholderFallback,
      freshDescriptions: mediaCapabilityResult.freshDescriptions,
      memoryCacheHits: mediaCapabilityResult.memoryCacheHits,
      diskCacheHits: mediaCapabilityResult.diskCacheHits,
      placeholderFallbacks: mediaCapabilityResult.placeholderFallbacks,
    })
  } else if (
    mediaCapabilityResult.modality === 'unknown' &&
    mediaCapabilityResult.imageCount > 0
  ) {
    logEvent('tengu_media_capability_unknown_modality', {
      imageCount: mediaCapabilityResult.imageCount,
    })
  }

  // PR-A (V3 billing transparency, 2026-07-01): the UI-facing counterpart to
  // the telemetry above. A single turn can yield several `AssistantMessage`s
  // (one per streamed content block, or one on the non-streaming fallback
  // path) — this is a turn-level fact, not a per-block one, so it's attached
  // to exactly the FIRST message constructed below (whichever branch runs),
  // never more than once per turn.
  const mediaSidecarUsed =
    mediaCapabilityResult.degradedImages > 0
      ? {
          sidecarDescribed: mediaCapabilityResult.sidecarDescribed,
          placeholderFallback: mediaCapabilityResult.placeholderFallback,
          freshDescriptions: mediaCapabilityResult.freshDescriptions,
          memoryCacheHits: mediaCapabilityResult.memoryCacheHits,
          diskCacheHits: mediaCapabilityResult.diskCacheHits,
          placeholderFallbacks: mediaCapabilityResult.placeholderFallbacks,
          // PR-R2a: per-kind reasons for the fallbacks (internal envelope +
          // TUI render; the worker's protocol stamp deliberately omits it).
          placeholderFallbackKinds:
            mediaCapabilityResult.placeholderFallbackKinds,
        }
      : undefined
  let mediaSidecarUsedAttached = false
  const attachMediaSidecarUsage = <T extends AssistantMessage>(
    message: T,
  ): T => {
    if (!mediaSidecarUsed || mediaSidecarUsedAttached) return message
    mediaSidecarUsedAttached = true
    return { ...message, mediaSidecarUsed }
  }

  // Instrumentation: Track message count after normalization
  logEvent('tengu_api_after_normalize', {
    postNormalizedMessageCount: messagesForAPI.length,
  })

  // Compute fingerprint from first user message for attribution.
  // Must run BEFORE injecting synthetic messages (e.g. deferred tool names)
  // so the fingerprint reflects the actual user input.
  const fingerprint = computeFingerprintFromMessages(messagesForAPI)

  // When the delta attachment is enabled, deferred tools are announced
  // via persisted deferred_tools_delta attachments instead of this
  // ephemeral prepend (which busts cache whenever the pool changes).
  if (useToolSearch && !isDeferredToolsDeltaEnabled()) {
    const deferredToolList = tools
      .filter(t => deferredToolNames.has(t.name))
      .map(formatDeferredToolLine)
      .sort()
      .join('\n')
    if (deferredToolList) {
      messagesForAPI = [
        createUserMessage({
          content: `<available-deferred-tools>\n${deferredToolList}\n</available-deferred-tools>`,
          isMeta: true,
        }),
        ...messagesForAPI,
      ]
    }
  }

  // filter(Boolean) works by converting each element to a boolean - empty strings become false and are filtered out.
  systemPrompt = asSystemPrompt(
    [
      getAttributionHeader(fingerprint),
      getCLISyspromptPrefix({
        isNonInteractive: options.isNonInteractiveSession,
        hasAppendSystemPrompt: options.hasAppendSystemPrompt,
      }),
      ...systemPrompt,
      ...(advisorModel ? [ADVISOR_TOOL_INSTRUCTIONS] : []),
    ].filter(Boolean),
  )

  // Prepend system prompt block for easy API identification
  logAPIPrefix(systemPrompt)

  const enablePromptCaching =
    options.enablePromptCaching ?? getPromptCachingEnabled(options.model)
  const system = buildSystemPromptBlocks(systemPrompt, enablePromptCaching, {
    skipGlobalCacheForSystemPrompt: needsToolBasedCacheMarker,
    querySource: options.querySource,
  })
  const useBetas = betas.length > 0

  // Build minimal context for detailed tracing (when beta tracing is enabled)
  // Note: The actual new_context message extraction is done in sessionTracing.ts using
  // hash-based tracking per querySource (agent) from the messagesForAPI array
  const extraToolSchemas = [...(options.extraToolSchemas ?? [])]
  if (advisorModel) {
    // Server tools must be in the tools array by API contract. Appended after
    // toolSchemas (which carries the cache_control marker) so toggling /advisor
    // only churns the small suffix, not the cached prefix.
    extraToolSchemas.push({
      type: 'advisor_20260301',
      name: 'advisor',
      model: advisorModel,
    } as unknown as BetaToolUnion)
  }
  const allTools = [...toolSchemas, ...extraToolSchemas]

  const isFastMode =
    isFastModeEnabled() &&
    isFastModeAvailable() &&
    !isFastModeCooldown() &&
    isFastModeSupportedByModel(options.model) &&
    !!options.fastMode

  // Sticky-on latches for dynamic beta headers. Each header, once first
  // sent, keeps being sent for the rest of the session so mid-session
  // toggles don't change the server-side cache key and bust ~50-70K tokens.
  // Latches are cleared on /clear and /compact via clearBetaHeaderLatches().
  // Per-call gates (isAgenticQuery, querySource===repl_main_thread) stay
  // per-call so non-agentic queries keep their own stable header set.

  let afkHeaderLatched = getAfkModeHeaderLatched() === true
  if (feature('TRANSCRIPT_CLASSIFIER')) {
    if (
      !afkHeaderLatched &&
      isAgenticQuery &&
      shouldIncludeFirstPartyOnlyBetas() &&
      (autoModeStateModule?.isAutoModeActive() ?? false)
    ) {
      afkHeaderLatched = true
      setAfkModeHeaderLatched(true)
    }
  }

  let fastModeHeaderLatched = getFastModeHeaderLatched() === true
  if (!fastModeHeaderLatched && isFastMode) {
    fastModeHeaderLatched = true
    setFastModeHeaderLatched(true)
  }

  let cacheEditingHeaderLatched = getCacheEditingHeaderLatched() === true
  if (feature('CACHED_MICROCOMPACT')) {
    if (
      !cacheEditingHeaderLatched &&
      cachedMCEnabled &&
      isOfficialProvider() &&
      options.querySource === 'repl_main_thread'
    ) {
      cacheEditingHeaderLatched = true
      setCacheEditingHeaderLatched(true)
    }
  }

  // Only latch from agentic queries so a classifier call doesn't flip the
  // main thread's context_management mid-turn.
  let thinkingClearLatched = getThinkingClearLatched() === true
  if (!thinkingClearLatched && isAgenticQuery) {
    const lastCompletion = getLastApiCompletionTimestamp()
    if (
      lastCompletion !== null &&
      Date.now() - lastCompletion > CACHE_TTL_1HOUR_MS
    ) {
      thinkingClearLatched = true
      setThinkingClearLatched(true)
    }
  }

  const effort = resolveAppliedEffort(options.model, options.effortValue)

  if (feature('PROMPT_CACHE_BREAK_DETECTION')) {
    // Exclude defer_loading tools from the hash -- the API strips them from the
    // prompt, so they never affect the actual cache key. Including them creates
    // false-positive "tool schemas changed" breaks when tools are discovered or
    // MCP servers reconnect.
    const toolsForCacheDetection = allTools.filter(
      t => !('defer_loading' in t && t.defer_loading),
    )
    // Capture everything that could affect the server-side cache key.
    // Pass latched header values (not live state) so break detection
    // reflects what we actually send, not what the user toggled.
    recordPromptState({
      system,
      toolSchemas: toolsForCacheDetection,
      querySource: options.querySource,
      model: options.model,
      agentId: options.agentId,
      fastMode: fastModeHeaderLatched,
      globalCacheStrategy,
      betas,
      autoModeActive: afkHeaderLatched,
      isUsingOverage: currentLimits.status === 'rejected',
      cachedMCEnabled: cacheEditingHeaderLatched,
      effortValue: effort,
      extraBodyParams: getExtraBodyParams(),
    })
  }

  const newContext: LLMRequestNewContext | undefined = isBetaTracingEnabled()
    ? {
        systemPrompt: systemPrompt.join('\n\n'),
        querySource: options.querySource,
        tools: jsonStringify(allTools),
      }
    : undefined

  // Capture the span so we can pass it to endLLMRequestSpan later
  // This ensures responses are matched to the correct request when multiple requests run in parallel
  const llmSpan = startLLMRequestSpan(
    options.model,
    newContext,
    messagesForAPI,
    isFastMode,
  )

  const startIncludingRetries = Date.now()
  let start = Date.now()
  let attemptNumber = 0
  const attemptStartTimes: number[] = []
  let stream: Stream<NormalizedAcosmiChatStreamEvent> | undefined = undefined
  let streamRequestId: string | null | undefined = undefined
  let clientRequestId: string | undefined = undefined
  // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins -- Response is available in Node 18+ and is used by the SDK
  let streamResponse: Response | undefined = undefined

  // S-1 (2026-06-09 prelaunch audit): caller abort → SSE transport wiring.
  // The stream adapter creates its own AbortController; the caller's `signal`
  // was previously never connected to it, so a user abort (ESC) could not
  // actively tear down an in-flight SSE fetch — it only took effect at the
  // post-loop cleanup point. detachCallerAbort holds the listener removal
  // for the wiring installed right after the stream is obtained below; it is
  // invoked from releaseStreamResources (every exit path) to prevent listener
  // leaks on the long-lived caller AbortSignal.
  let detachCallerAbort: (() => void) | undefined = undefined

  // Release all stream resources to prevent native memory leaks.
  // The Response object holds native TLS/socket buffers that live outside the
  // V8 heap (observed on the Node.js/npm path; see GH #32920), so we must
  // explicitly cancel and release it regardless of how the generator exits.
  function releaseStreamResources(): void {
    if (detachCallerAbort) {
      detachCallerAbort()
      detachCallerAbort = undefined
    }
    cleanupStream(stream)
    stream = undefined
    if (streamResponse) {
      streamResponse.body?.cancel().catch(() => {})
      streamResponse = undefined
    }
  }

  // Consume pending cache edits ONCE before paramsFromContext is defined.
  // paramsFromContext is called multiple times (logging, retries), so consuming
  // inside it would cause the first call to steal edits from subsequent calls.
  const consumedCacheEdits = cachedMCEnabled ? consumePendingCacheEdits() : null
  const consumedPinnedEdits = cachedMCEnabled ? getPinnedCacheEdits() : []

  // Capture the betas sent in the last API request, including the ones that
  // were dynamically added, so we can log and send it to telemetry.
  let lastRequestBetas: string[] | undefined

  const paramsFromContext = (retryContext: RetryContext) => {
    const betasParams = [...betas]

    const extraBodyParams = getExtraBodyParams()

    const outputConfig: BetaOutputConfig = {
      ...((extraBodyParams.output_config as BetaOutputConfig) ?? {}),
    }

    configureEffortParams(
      effort,
      outputConfig,
      extraBodyParams,
      betasParams,
      options.model,
    )

    configureTaskBudgetParams(
      options.taskBudget,
      outputConfig as BetaOutputConfig & { task_budget?: TaskBudgetParam },
      betasParams,
    )

    // Merge outputFormat into extraBodyParams.output_config alongside effort
    // Requires structured-outputs beta header per SDK (see parse() in messages.mjs)
    if (options.outputFormat && !('format' in outputConfig)) {
      outputConfig.format = options.outputFormat as BetaJSONOutputFormat
      // Add beta header if not already present and provider supports it
      if (
        modelSupportsStructuredOutputs(options.model) &&
        !betasParams.includes(STRUCTURED_OUTPUTS_BETA_HEADER)
      ) {
        betasParams.push(STRUCTURED_OUTPUTS_BETA_HEADER)
      }
    }

    // Retry context gets preference because it tries to course correct if we exceed the context window limit
    const maxOutputTokens =
      retryContext?.maxTokensOverride ||
      options.maxOutputTokensOverride ||
      getMaxOutputTokensForModel(options.model)

    const hasThinking =
      thinkingConfig.type !== 'disabled' &&
      !isEnvTruthy(process.env.CRABCODE_DISABLE_THINKING)
    let thinking: BetaMessageStreamParams['thinking'] | undefined = undefined

    // IMPORTANT: Do not change the adaptive-vs-budget thinking selection below
    // without notifying the model launch DRI and research. This is a sensitive
    // setting that can greatly affect model quality and bashing.
    if (hasThinking && modelSupportsThinking(options.model)) {
      if (
        !isEnvTruthy(process.env.CRABCODE_DISABLE_ADAPTIVE_THINKING) &&
        modelSupportsAdaptiveThinking(options.model)
      ) {
        // For models that support adaptive thinking, always use adaptive
        // thinking without a budget.
        thinking = {
          type: 'adaptive',
        } as unknown as BetaMessageStreamParams['thinking']
      } else {
        // For models that do not support adaptive thinking, use the default
        // thinking budget unless explicitly specified.
        let thinkingBudget = getMaxThinkingTokensForModel(options.model)
        if (
          thinkingConfig.type === 'enabled' &&
          thinkingConfig.budgetTokens !== undefined
        ) {
          thinkingBudget = thinkingConfig.budgetTokens
        }
        thinkingBudget = Math.min(maxOutputTokens - 1, thinkingBudget)
        thinking = {
          budget_tokens: thinkingBudget,
          type: 'enabled',
        } satisfies BetaMessageStreamParams['thinking']
      }
    }

    // Get API context management strategies if enabled
    const contextManagement = getAPIContextManagement({
      hasThinking,
      isRedactThinkingActive: betasParams.includes(REDACT_THINKING_BETA_HEADER),
      clearAllThinking: thinkingClearLatched,
    })

    const enablePromptCaching =
      options.enablePromptCaching ?? getPromptCachingEnabled(retryContext.model)

    // Fast mode: header is latched session-stable (cache-safe), but
    // `speed='fast'` stays dynamic so cooldown still suppresses the actual
    // fast-mode request without changing the cache key.
    let speed: string | undefined
    const isFastModeForRetry =
      isFastModeEnabled() &&
      isFastModeAvailable() &&
      !isFastModeCooldown() &&
      isFastModeSupportedByModel(options.model) &&
      !!retryContext.fastMode
    if (isFastModeForRetry) {
      speed = 'fast'
    }
    if (fastModeHeaderLatched && !betasParams.includes(FAST_MODE_BETA_HEADER)) {
      betasParams.push(FAST_MODE_BETA_HEADER)
    }

    // AFK mode beta: latched once auto mode is first activated. Still gated
    // by isAgenticQuery per-call so classifiers/compaction don't get it.
    if (feature('TRANSCRIPT_CLASSIFIER')) {
      if (
        afkHeaderLatched &&
        shouldIncludeFirstPartyOnlyBetas() &&
        isAgenticQuery &&
        !betasParams.includes(AFK_MODE_BETA_HEADER)
      ) {
        betasParams.push(AFK_MODE_BETA_HEADER)
      }
    }

    // Cache editing beta: header is latched session-stable; useCachedMC
    // (controls cache_edits body behavior) stays live so edits stop when
    // the feature disables but the header doesn't flip.
    const useCachedMC =
      cachedMCEnabled &&
      isOfficialProvider() &&
      options.querySource === 'repl_main_thread'
    if (
      cacheEditingHeaderLatched &&
      isOfficialProvider() &&
      options.querySource === 'repl_main_thread' &&
      !betasParams.includes(cacheEditingBetaHeader)
    ) {
      betasParams.push(cacheEditingBetaHeader)
      logForDebugging(
        'Cache editing beta header enabled for cached microcompact',
      )
    }

    // Only send temperature when thinking is disabled — the API requires
    // temperature: 1 when thinking is enabled, which is already the default.
    const temperature = !hasThinking
      ? (options.temperatureOverride ?? 1)
      : undefined

    lastRequestBetas = betasParams

    return {
      model: normalizeModelStringForAPI(options.model),
      messages: addCacheBreakpoints(
        messagesForAPI,
        enablePromptCaching,
        options.querySource,
        useCachedMC,
        consumedCacheEdits as import('./types.js').CachedMCEditsBlock | null,
        consumedPinnedEdits as import('./types.js').CachedMCPinnedEdits[],
        options.skipCacheWrite,
      ),
      system,
      tools: allTools,
      tool_choice: options.toolChoice,
      ...(useBetas && { betas: betasParams }),
      metadata: getAPIMetadata(),
      max_tokens: maxOutputTokens,
      thinking,
      ...(temperature !== undefined && { temperature }),
      ...(contextManagement &&
        useBetas &&
        betasParams.includes(CONTEXT_MANAGEMENT_BETA_HEADER) && {
          context_management: contextManagement,
        }),
      ...extraBodyParams,
      ...(Object.keys(outputConfig).length > 0 && {
        output_config: outputConfig,
      }),
      ...(speed !== undefined && { speed }),
    }
  }

  // Compute log scalars synchronously so the fire-and-forget .then() closure
  // captures only primitives instead of paramsFromContext's full closure scope
  // (messagesForAPI, system, allTools, betas — the entire request-building
  // context), which would otherwise be pinned until the promise resolves.
  {
    const queryParams = paramsFromContext({
      model: options.model,
      thinkingConfig,
    })
    const logMessagesLength = queryParams.messages.length
    const logBetas = useBetas ? (queryParams.betas ?? []) : []
    const logThinkingType = queryParams.thinking?.type ?? 'disabled'
    const logEffortValue = queryParams.output_config?.effort
    void options.getToolPermissionContext().then(permissionContext => {
      logAPIQuery({
        model: options.model,
        messagesLength: logMessagesLength,
        temperature: options.temperatureOverride ?? 1,
        betas: logBetas,
        permissionMode: permissionContext.mode,
        querySource: options.querySource,
        queryTracking: options.queryTracking,
        thinkingType: logThinkingType as 'enabled' | 'disabled' | 'adaptive' | undefined,
        effortValue: logEffortValue as import('../../utils/effort.js').EffortLevel | null | undefined,
        fastMode: isFastMode,
        previousRequestId,
      })
    })
  }

  const newMessages: AssistantMessage[] = []
  let ttftMs = 0
  let partialMessage: BetaMessage | undefined = undefined
  const contentBlocks: (BetaContentBlock | ConnectorTextBlock)[] = []
  let usage: NonNullableUsage = EMPTY_USAGE
  let costUSD = 0
  let stopReason: BetaStopReason | null = null
  let didFallBackToNonStreaming = false
  let fallbackMessage: AssistantMessage | undefined
  let maxOutputTokens = 0
  let responseHeaders: globalThis.Headers | undefined = undefined
  let research: unknown = undefined
  let isFastModeRequest = isFastMode // Keep separate state as it may change if falling back
  let isAdvisorInProgress = false

  try {
    queryCheckpoint('query_client_creation_start')
    const generator = withRetry(
      // Phase 3: No SDK client needed — IPC handles all API calls.
      // withRetry requires a client factory; pass a no-op that returns null.
      () => Promise.resolve(null as unknown as Acosmi),
      async (_acosmi, attempt, context) => {
        attemptNumber = attempt
        isFastModeRequest = context.fastMode ?? false
        start = Date.now()
        attemptStartTimes.push(start)
        // PR-A (2026-07-01, caught in destructive review): reset per attempt.
        // `mediaSidecarUsed` itself doesn't change across retries (same
        // already-degraded messagesForAPI is resent), but which message is
        // "first" does — a discarded/failed earlier attempt must not steal
        // the flag from the attempt that actually succeeds and reaches the
        // user.
        mediaSidecarUsedAttached = false
        // F3-4 (W-GOAL-HANG): structured attempt bracket — start. Terminal
        // events: cli_streaming_attempt_completed (success path, below) /
        // cli_streaming_attempt_failed (terminal error paths). Watchdog
        // aborts additionally keep their dedicated cli_streaming_idle_*
        // events. NoPII: model slug + numbers only.
        logForDiagnosticsNoPII('info', 'cli_streaming_attempt_started', {
          attempt,
          model: options.model,
        })
        // Client has been created by withRetry's getClient() call. This fires
        // once per attempt; on retries the client is usually cached (withRetry
        // only calls getClient() again after auth errors), so the delta from
        // client_creation_start is meaningful on attempt 1.
        queryCheckpoint('query_client_creation_end')

        const params = paramsFromContext(context)
        captureAPIRequest(params, options.querySource) // Capture for bug reports
        maxOutputTokens = params.max_tokens

        // Fire immediately before the fetch is dispatched. .withResponse() below
        // awaits until response headers arrive, so this MUST be before the await
        // or the "Network TTFB" phase measurement is wrong.
        queryCheckpoint('query_api_request_sent')
        if (!options.agentId) {
          headlessProfilerCheckpoint('api_request_sent')
        }

        // SDK 直调流式 — chatStreamAdapter 内部把 SSE.data 对齐 BetaRaw
        // 形态，并保留 SDK 2.12 声明的 sources 事件判别符。
        const sdkReq = buildAcosmiChatRequestFromParams(params)
        queryCheckpoint('query_response_headers_received')
        streamRequestId = null
        streamResponse = undefined
        clientRequestId = undefined
        logForDebugging(
          `[queryModel] streaming chat OUT options.model=${options.model} params.model=${params.model} fallback=${options.fallbackModel ?? '(none)'}`,
        )
        // `local:<id>` models are hosted by the TUI-managed loopback
        // inference server, never the Acosmi gateway. Route through the local
        // adapter; it returns the same ChatStreamAdapter shape, so the
        // streaming consumption / idle watchdog / cleanupStream path below is
        // shared verbatim. `params` is the BetaMessageStreamParams already
        // built above; the adapter pins the bare local id internally.
        //
        // BLOCKER-4: the inference-time entitlement gate. Config-time and
        // server-start gates are not enough — a stale `local:<id>` selection
        // (account logged out / downgraded / switched after the server was
        // already running) must not keep streaming. `local:` runs on the
        // user's own hardware so this is a feature-tier gate, not a billing
        // gate, but it is still a PR-7/PR-9 acceptance item.
        if (isLocalModelReference(params.model)) {
          if (!canUseLocalModels(getMembershipGateInput())) {
            throw new Error(
              'local models require a Plus (or higher) paid membership',
            )
          }
          return localModelChatStreamAdapter(
            params.model,
            params,
          ) as unknown as Stream<NormalizedAcosmiChatStreamEvent>
        }
        // BLOCKER-1: custom (bring-your-own) models have their own runtime
        // path — they must reach the user-declared `baseUrl`, never the
        // Acosmi gateway. BLOCKER-4: the same inference-time entitlement gate
        // applies, so a stale custom selection cannot keep streaming after a
        // downgrade.
        const customRuntime = resolveCustomModelRuntime(params.model)
        if (customRuntime) {
          if (!canUseCustomModels(getMembershipGateInput())) {
            throw new Error(
              'custom models require a Plus (or higher) paid membership',
            )
          }
          return customModelChatStreamAdapter(
            customRuntime,
            params,
          ) as unknown as Stream<NormalizedAcosmiChatStreamEvent>
        }
        const accountRouteId = parseAccountBridgeReference(params.model)
        if (accountRouteId) {
          // BLOCKER-4 parity for `account:` routes (双层会员门 2026-07-24):
          // the control plane refuses to sign a grant below the Plus floor,
          // but a grant already inside its five-minute TTL would otherwise
          // keep streaming after a logout or downgrade. This is the single
          // inference chokepoint — transcript-derived auxiliary work (title,
          // compaction, away recap, tool labels) reaches it through
          // `resolveSessionAuxiliaryRoute`, so it needs no separate gate.
          if (!canUseAccountBridge(getMembershipGateInput())) {
            throw new Error(
              'account models require a Plus (or higher) paid membership',
            )
          }
          const runtimeAccess = options.accountBridgeRuntimeAccess
          if (!runtimeAccess || runtimeAccess.route.routeId !== accountRouteId) {
            throw new DanglingModelReferenceError(params.model)
          }
          return accountBridgeChatStreamAdapter(
            runtimeAccess,
            params,
            options.crabcodeThinkingMode ?? 'auto',
          ) as unknown as Stream<NormalizedAcosmiChatStreamEvent>
        }
        // PR-2 (2026-07-01 audit finding 5): a `custom:` prefix already
        // declares "not a gateway model". When the registry entry no longer
        // resolves (deleted / disabled / downgraded), falling through to the
        // gateway would leak the conversation and bury the real cause under a
        // gateway model_not_found/402 — fail loud with recovery guidance
        // instead. (`local:` cannot reach here: its branch above always
        // returns the local adapter.)
        if (isNonGatewayModelReference(params.model)) {
          throw new DanglingModelReferenceError(params.model)
        }
        return chatStreamAdapter(
          params.model,
          sdkReq,
        ) as unknown as Stream<NormalizedAcosmiChatStreamEvent>
      },
      {
        model: options.model,
        fallbackModel: parseAccountBridgeReference(options.model)
          ? undefined
          : options.fallbackModel,
        thinkingConfig,
        ...(isFastModeEnabled() ? { fastMode: isFastMode } : false),
        signal,
        querySource: options.querySource,
      },
    )

    let e
    do {
      e = await generator.next()

      // yield API error messages (the stream has a 'controller' property, error messages don't)
      if (!('controller' in e.value)) {
        yield e.value
      }
    } while (!e.done)
    stream = e.value as Stream<NormalizedAcosmiChatStreamEvent>

    // S-1: wire the caller's AbortSignal to the stream adapter's controller so
    // a user abort propagates to the SSE transport layer immediately (aborting
    // the underlying fetch) instead of waiting for the stream loop to exit on
    // its own. All three adapters (acosmi / local / custom) expose the same
    // `.controller`, so wiring at this consumer point covers every route.
    // cleanupStream reads the closure variable `stream`, which
    // releaseStreamResources sets to undefined on release — a late abort
    // firing after release is therefore a safe no-op.
    if (signal.aborted) {
      cleanupStream(stream)
    } else {
      const onCallerAbort = (): void => {
        cleanupStream(stream)
      }
      signal.addEventListener('abort', onCallerAbort, { once: true })
      detachCallerAbort = () =>
        signal.removeEventListener('abort', onCallerAbort)
    }

    // reset state
    newMessages.length = 0
    ttftMs = 0
    partialMessage = undefined
    contentBlocks.length = 0
    usage = EMPTY_USAGE
    stopReason = null
    isAdvisorInProgress = false

    // Streaming idle timeout watchdog: abort the stream if no chunks arrive
    // for STREAM_IDLE_TIMEOUT_MS. Unlike the stall detection below (which only
    // fires when the *next* chunk arrives), this uses setTimeout to actively
    // kill hung streams. Without this, a silently dropped connection can hang
    // the session indefinitely since the SDK's request timeout only covers the
    // initial fetch(), not the streaming body.
    // S-1 (2026-06-09 prelaunch audit): default ON. The timer is reset on
    // every received chunk, so only a truly idle stream (zero events for
    // STREAM_IDLE_TIMEOUT_MS, default 90s — slow-but-alive streams keep
    // resetting it) trips the watchdog, and the trip lands on the retryable
    // error path (non-streaming fallback / withRetry), never a crash.
    // Escape hatches: CRABCODE_DISABLE_STREAM_WATCHDOG=1 turns it off, and an
    // explicit CRABCODE_ENABLE_STREAM_WATCHDOG=0/false (the legacy opt-in
    // variable) is also honored as off.
    const streamWatchdogEnabled =
      !isEnvTruthy(process.env.CRABCODE_DISABLE_STREAM_WATCHDOG) &&
      !isEnvDefinedFalsy(process.env.CRABCODE_ENABLE_STREAM_WATCHDOG)
    const STREAM_IDLE_TIMEOUT_MS =
      parseInt(process.env.CRABCODE_STREAM_IDLE_TIMEOUT_MS || '', 10) || 90_000
    const STREAM_IDLE_WARNING_MS = STREAM_IDLE_TIMEOUT_MS / 2
    let streamIdleAborted = false
    // performance.now() snapshot when watchdog fires, for measuring abort propagation delay
    let streamWatchdogFiredAt: number | null = null
    let streamIdleWarningTimer: ReturnType<typeof setTimeout> | null = null
    let streamIdleTimer: ReturnType<typeof setTimeout> | null = null
    function clearStreamIdleTimers(): void {
      if (streamIdleWarningTimer !== null) {
        clearTimeout(streamIdleWarningTimer)
        streamIdleWarningTimer = null
      }
      if (streamIdleTimer !== null) {
        clearTimeout(streamIdleTimer)
        streamIdleTimer = null
      }
    }
    function resetStreamIdleTimer(): void {
      clearStreamIdleTimers()
      if (!streamWatchdogEnabled) {
        return
      }
      streamIdleWarningTimer = setTimeout(
        warnMs => {
          logForDebugging(
            `Streaming idle warning: no chunks received for ${warnMs / 1000}s`,
            { level: 'warn' },
          )
          logForDiagnosticsNoPII('warn', 'cli_streaming_idle_warning')
        },
        STREAM_IDLE_WARNING_MS,
        STREAM_IDLE_WARNING_MS,
      )
      streamIdleTimer = setTimeout(() => {
        streamIdleAborted = true
        streamWatchdogFiredAt = performance.now()
        logForDebugging(
          `Streaming idle timeout: no chunks received for ${STREAM_IDLE_TIMEOUT_MS / 1000}s, aborting stream`,
          { level: 'error' },
        )
        logForDiagnosticsNoPII('error', 'cli_streaming_idle_timeout')
        logEvent('tengu_streaming_idle_timeout', {
          model:
            options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          request_id: (streamRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          timeout_ms: STREAM_IDLE_TIMEOUT_MS,
        })
        releaseStreamResources()
      }, STREAM_IDLE_TIMEOUT_MS)
    }
    resetStreamIdleTimer()

    startSessionActivity('api_call')
    try {
      // stream in and accumulate state
      let isFirstChunk = true
      let lastEventTime: number | null = null // Set after first chunk to avoid measuring TTFB as a stall
      const STALL_THRESHOLD_MS = 30_000 // 30 seconds
      let totalStallTime = 0
      let stallCount = 0

      for await (const part of stream) {
        resetStreamIdleTimer()
        const now = Date.now()

        // Detect and log streaming stalls (only after first event to avoid counting TTFB)
        if (lastEventTime !== null) {
          const timeSinceLastEvent = now - lastEventTime
          if (timeSinceLastEvent > STALL_THRESHOLD_MS) {
            stallCount++
            totalStallTime += timeSinceLastEvent
            logForDebugging(
              `Streaming stall detected: ${(timeSinceLastEvent / 1000).toFixed(1)}s gap between events (stall #${stallCount})`,
              { level: 'warn' },
            )
            logEvent('tengu_streaming_stall', {
              stall_duration_ms: timeSinceLastEvent,
              stall_count: stallCount,
              total_stall_time_ms: totalStallTime,
              event_type:
                part.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              model:
                options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              request_id: (streamRequestId ??
                'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
            })
          }
        }
        lastEventTime = now

        if (isFirstChunk) {
          logForDebugging('Stream started - received first chunk')
          queryCheckpoint('query_first_chunk_received')
          if (!options.agentId) {
            headlessProfilerCheckpoint('first_chunk')
          }
          endQueryProfile()
          isFirstChunk = false
        }

        switch (part.type) {
          case 'message_start': {
            partialMessage = part.message
            ttftMs = Date.now() - start
            usage = updateUsage(usage, part.message?.usage)
            // Capture research from message_start if available (internal only).
            // Always overwrite with the latest value.
            if (
              process.env.USER_TYPE === 'ant' &&
              'research' in (part.message as unknown as Record<string, unknown>)
            ) {
              research = (part.message as unknown as Record<string, unknown>)
                .research
            }
            break
          }
          case 'content_block_start':
            switch (part.content_block.type) {
              case 'tool_use':
                contentBlocks[part.index] = {
                  ...part.content_block,
                  input: '',
                }
                break
              case 'server_tool_use':
                contentBlocks[part.index] = {
                  ...part.content_block,
                  input: '' as unknown as { [key: string]: unknown },
                }
                if ((part.content_block.name as string) === 'advisor') {
                  isAdvisorInProgress = true
                  logForDebugging(`[AdvisorTool] Advisor tool called`)
                  logEvent('tengu_advisor_tool_call', {
                    model:
                      options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                    advisor_model: (advisorModel ??
                      'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                  })
                }
                break
              case 'text':
                contentBlocks[part.index] = {
                  ...part.content_block,
                  // awkwardly, the sdk sometimes returns text as part of a
                  // content_block_start message, then returns the same text
                  // again in a content_block_delta message. we ignore it here
                  // since there doesn't seem to be a way to detect when a
                  // content_block_delta message duplicates the text.
                  text: '',
                }
                break
              case 'thinking':
                contentBlocks[part.index] = {
                  ...part.content_block,
                  // also awkward
                  thinking: '',
                  // initialize signature to ensure field exists even if signature_delta never arrives
                  signature: '',
                }
                break
              default:
                // even more awkwardly, the sdk mutates the contents of text blocks
                // as it works. we want the blocks to be immutable, so that we can
                // accumulate state ourselves.
                contentBlocks[part.index] = { ...part.content_block }
                if (
                  (part.content_block.type as string) === 'advisor_tool_result'
                ) {
                  isAdvisorInProgress = false
                  logForDebugging(`[AdvisorTool] Advisor tool result received`)
                }
                break
            }
            break
          case 'content_block_delta': {
            const contentBlock = contentBlocks[part.index]
            const delta = part.delta as typeof part.delta | ConnectorTextDelta
            if (!contentBlock) {
              logEvent('tengu_streaming_error', {
                error_type:
                  'content_block_not_found_delta' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                part_type:
                  part.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                part_index: part.index,
              })
              throw new RangeError('Content block not found')
            }
            if (
              feature('CONNECTOR_TEXT') &&
              delta.type === 'connector_text_delta'
            ) {
              if (contentBlock.type !== 'connector_text') {
                logEvent('tengu_streaming_error', {
                  error_type:
                    'content_block_type_mismatch_connector_text' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                  expected_type:
                    'connector_text' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                  actual_type:
                    contentBlock.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                })
                throw new Error('Content block is not a connector_text block')
              }
              contentBlock.connector_text = (contentBlock.connector_text ?? '') + (delta as unknown as { connector_text: string }).connector_text
            } else {
              switch (delta.type) {
                case 'citations_delta':
                  // citations_delta: no-op (not yet consumed by UI)
                  break
                case 'input_json_delta':
                  if (
                    contentBlock.type !== 'tool_use' &&
                    contentBlock.type !== 'server_tool_use'
                  ) {
                    logEvent('tengu_streaming_error', {
                      error_type:
                        'content_block_type_mismatch_input_json' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      expected_type:
                        'tool_use' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      actual_type:
                        contentBlock.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                    })
                    throw new Error('Content block is not a input_json block')
                  }
                  if (typeof contentBlock.input !== 'string') {
                    logEvent('tengu_streaming_error', {
                      error_type:
                        'content_block_input_not_string' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      input_type:
                        typeof contentBlock.input as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                    })
                    throw new Error('Content block input is not a string')
                  }
                  // Go relay 端 partial_json 带 omitempty，空字符串序列化时字段被丢
                  // → TS 这里收到 undefined，直接 += 会拼成 "undefined" 损坏 tool input。
                  contentBlock.input += delta.partial_json ?? ''
                  break
                case 'text_delta':
                  if (contentBlock.type !== 'text') {
                    logEvent('tengu_streaming_error', {
                      error_type:
                        'content_block_type_mismatch_text' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      expected_type:
                        'text' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      actual_type:
                        contentBlock.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                    })
                    throw new Error('Content block is not a text block')
                  }
                  contentBlock.text += delta.text ?? ''
                  break
                case 'signature_delta':
                  if (
                    feature('CONNECTOR_TEXT') &&
                    contentBlock.type === 'connector_text'
                  ) {
                    contentBlock.signature = (delta as unknown as { signature?: string }).signature
                    break
                  }
                  if (contentBlock.type !== 'thinking') {
                    logEvent('tengu_streaming_error', {
                      error_type:
                        'content_block_type_mismatch_thinking_signature' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      expected_type:
                        'thinking' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      actual_type:
                        contentBlock.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                    })
                    throw new Error('Content block is not a thinking block')
                  }
                  contentBlock.signature = delta.signature as string
                  break
                case 'thinking_delta':
                  if (contentBlock.type !== 'thinking') {
                    logEvent('tengu_streaming_error', {
                      error_type:
                        'content_block_type_mismatch_thinking_delta' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      expected_type:
                        'thinking' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                      actual_type:
                        contentBlock.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                    })
                    throw new Error('Content block is not a thinking block')
                  }
                  contentBlock.thinking += delta.thinking ?? ''
                  break
              }
            }
            // Capture research from content_block_delta if available (internal only).
            // Always overwrite with the latest value.
            if (process.env.USER_TYPE === 'ant' && 'research' in part) {
              research = (part as { research: unknown }).research
            }
            break
          }
          case 'content_block_stop': {
            const contentBlock = contentBlocks[part.index]
            if (!contentBlock) {
              logEvent('tengu_streaming_error', {
                error_type:
                  'content_block_not_found_stop' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                part_type:
                  part.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                part_index: part.index,
              })
              throw new RangeError('Content block not found')
            }
            if (!partialMessage) {
              logEvent('tengu_streaming_error', {
                error_type:
                  'partial_message_not_found' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
                part_type:
                  part.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              })
              throw new Error('Message not found')
            }
            const m: AssistantMessage = {
              message: {
                ...partialMessage,
                content: normalizeContentFromAPI(
                  [contentBlock] as BetaContentBlock[],
                  tools,
                  options.agentId,
                ),
              } as AssistantMessage['message'],
              requestId: streamRequestId ?? undefined,
              type: 'assistant',
              uuid: randomUUID(),
              timestamp: new Date().toISOString(),
              ...(process.env.USER_TYPE === 'ant' &&
                research !== undefined && { research }),
              ...(advisorModel && { advisorModel }),
              ...(mediaSidecarUsed &&
                !mediaSidecarUsedAttached && { mediaSidecarUsed }),
            }
            if (mediaSidecarUsed && !mediaSidecarUsedAttached) {
              mediaSidecarUsedAttached = true
            }
            newMessages.push(m)
            yield m
            break
          }
          case 'message_delta': {
            usage = updateUsage(usage, part.usage)
            // Capture research from message_delta if available (internal only).
            // Always overwrite with the latest value. Also write back to
            // already-yielded messages since message_delta arrives after
            // content_block_stop.
            if (
              process.env.USER_TYPE === 'ant' &&
              'research' in (part as unknown as Record<string, unknown>)
            ) {
              research = (part as unknown as Record<string, unknown>).research
              for (const msg of newMessages) {
                (msg as AssistantMessage & { research?: unknown }).research = research
              }
            }

            // Write final usage and stop_reason back to the last yielded
            // message. Messages are created at content_block_stop from
            // partialMessage, which was set at message_start before any tokens
            // were generated (output_tokens: 0, stop_reason: null).
            // message_delta arrives after content_block_stop with the real
            // values.
            //
            // IMPORTANT: Use direct property mutation, not object replacement.
            // The transcript write queue holds a reference to message.message
            // and serializes it lazily (100ms flush interval). Object
            // replacement ({ ...lastMsg.message, usage }) would disconnect
            // the queued reference; direct mutation ensures the transcript
            // captures the final values.
            stopReason = part.delta.stop_reason ?? null

            const lastMsg = newMessages.at(-1)
            if (lastMsg) {
              // eslint-disable-next-line @typescript-eslint/no-explicit-any
              lastMsg.message.usage = usage as any
              lastMsg.message.stop_reason = stopReason
            }

            // Update cost
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            const costUSDForPart = calculateUSDCost(resolvedModel, usage as any)
            costUSD += addToTotalSessionCost(
              costUSDForPart,
              // eslint-disable-next-line @typescript-eslint/no-explicit-any
              usage as any,
              options.model,
            )

            const refusalMessage = getErrorMessageIfRefusal(
              part.delta.stop_reason ?? null,
              options.model,
            )
            if (refusalMessage) {
              yield refusalMessage
            }

            if (stopReason === 'max_tokens') {
              // 2026-07-17 audit RC-1: attribute the cut honestly. When the
              // terminal usage is far below the requested budget the limit was
              // imposed upstream, not by CRABCODE_MAX_OUTPUT_TOKENS — the old
              // copy blamed the client's own number (64000) while the provider
              // had cut at ~2001.
              const maxTokensCopy = formatMaxOutputTokensError(
                maxOutputTokens,
                usage.output_tokens,
              )
              logEvent('tengu_max_tokens_reached', {
                max_tokens: maxOutputTokens,
                output_tokens: usage.output_tokens ?? 0,
                upstream_truncated: maxTokensCopy.upstreamTruncated ? 1 : 0,
              })
              if (maxTokensCopy.upstreamTruncated) {
                logEvent('tengu_upstream_output_cap_mismatch', {
                  max_tokens: maxOutputTokens,
                  output_tokens: usage.output_tokens ?? 0,
                })
              }
              yield attachMediaSidecarUsage(
                createAssistantAPIErrorMessage({
                  content: maxTokensCopy.content,
                  error: 'max_output_tokens' as import('../../entrypoints/sdk/coreTypes.generated.js').SDKAssistantMessageError,
                }),
              )
            }

            if ((stopReason as string) === 'model_context_window_exceeded') {
              logEvent('tengu_context_window_exceeded', {
                max_tokens: maxOutputTokens,
                output_tokens: usage.output_tokens,
              })
              // Reuse the max_output_tokens recovery path — from the model's
              // perspective, both mean "response was cut off, continue from
              // where you left off."
              yield attachMediaSidecarUsage(
                createAssistantAPIErrorMessage({
                  content: `${API_ERROR_MESSAGE_PREFIX}: The model has reached its context window limit.`,
                  error: 'max_output_tokens' as import('../../entrypoints/sdk/coreTypes.generated.js').SDKAssistantMessageError,
                }),
              )
            }
            break
          }
          case 'error': {
            // V116.1 P1-4 (2026-07-24): 网关/上游流内 error 帧(anthropic 协议
            // `{type:'error', error:{type,message}, errorCode?}`;顶层 errorCode
            // 为 nexus V116.1 新增,上游原生帧无此字段)。此前 switch 无此 case,
            // 帧被静默忽略,用户要等 30s 空闲看门狗才看到超时文案,且空流可能
            // 触发非流式回退重放。现即时经归一器呈现并终止本次流(不回退重放:
            // 流已部分消费/计费,重发有双计费风险)。
            const sseNormalized = normalizeSseErrorEvent(part)
            logForDebugging(
              `[queryModel] in-stream error frame: code=${sseNormalized.errorCode ?? '-'} type=${sseNormalized.errorType ?? '-'} msg=${sseNormalized.message.slice(0, 300)}`,
              { level: 'error' },
            )
            const sseDisplay = getGatewayErrorDisplay(
              sseNormalized,
              options.model,
            )
            yield attachMediaSidecarUsage(
              createAssistantAPIErrorMessage(
                sseDisplay ?? {
                  content: `${API_ERROR_MESSAGE_PREFIX}: ${sseNormalized.errorType ? `[${sseNormalized.errorType}] ` : ''}${sseNormalized.message}`,
                  error: 'server_error',
                },
              ),
            )
            clearStreamIdleTimers()
            releaseStreamResources()
            return
          }
          case 'message_stop':
            break
        }

        yield {
          type: 'stream_event',
          event: part,
          ...(part.type === 'message_start' ? { ttftMs } : undefined),
        }
      }
      // Clear the idle timeout watchdog now that the stream loop has exited
      clearStreamIdleTimers()

      // S-1: when the caller abort wired above tears down the transport, the
      // adapter swallows the AbortError (`if (!controller.signal.aborted)
      // throw err`) and the for-await exits cleanly mid-message. Without this
      // guard the truncated stream would fall through to the empty-stream /
      // non-streaming fallback below and issue a brand-new API call the user
      // just cancelled. Surface it as APIUserAbortError instead — the catch
      // block below recognizes it (signal.aborted === true) and rethrows.
      if (signal.aborted) {
        throw new APIUserAbortError()
      }

      // If the stream was aborted by our idle timeout watchdog, fall back to
      // non-streaming retry rather than treating it as a completed stream.
      if (streamIdleAborted) {
        // Instrumentation: proves the for-await exited after the watchdog fired
        // (vs. hung forever). exit_delay_ms measures abort propagation latency:
        // 0-10ms = abort worked; >>1000ms = something else woke the loop.
        const exitDelayMs =
          streamWatchdogFiredAt !== null
            ? Math.round(performance.now() - streamWatchdogFiredAt)
            : -1
        logForDiagnosticsNoPII(
          'info',
          'cli_stream_loop_exited_after_watchdog_clean',
        )
        logEvent('tengu_stream_loop_exited_after_watchdog', {
          request_id: (streamRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          exit_delay_ms: exitDelayMs,
          exit_path:
            'clean' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          model:
            options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
        // Prevent double-emit: this throw lands in the catch block below,
        // whose exit_path='error' probe guards on streamWatchdogFiredAt.
        streamWatchdogFiredAt = null
        throw new Error('Stream idle timeout - no chunks received')
      }

      // Detect when the stream completed without producing any assistant messages.
      // This covers two proxy failure modes:
      // 1. No events at all (!partialMessage): proxy returned 200 with non-SSE body
      // 2. Partial events (partialMessage set but no content blocks completed AND
      //    no stop_reason received): proxy returned message_start but stream ended
      //    before content_block_stop and before message_delta with stop_reason
      // BetaMessageStream had the first check in _endRequest() but the raw Stream
      // does not - without it the generator silently returns no assistant messages,
      // causing "Execution error" in -p mode.
      // Note: We must check stopReason to avoid false positives. For example, with
      // structured output (--json-schema), the model calls a StructuredOutput tool
      // on turn 1, then on turn 2 responds with end_turn and no content blocks.
      // That's a legitimate empty response, not an incomplete stream.
      if (!partialMessage || (newMessages.length === 0 && !stopReason)) {
        logForDebugging(
          !partialMessage
            ? 'Stream completed without receiving message_start event - triggering non-streaming fallback'
            : 'Stream completed with message_start but no content blocks completed - triggering non-streaming fallback',
          { level: 'error' },
        )
        logEvent('tengu_stream_no_events', {
          model:
            options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          request_id: (streamRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
        throw new Error('Stream ended without receiving any events')
      }

      // Log summary if any stalls occurred during streaming
      if (stallCount > 0) {
        logForDebugging(
          `Streaming completed with ${stallCount} stall(s), total stall time: ${(totalStallTime / 1000).toFixed(1)}s`,
          { level: 'warn' },
        )
        logEvent('tengu_streaming_stall_summary', {
          stall_count: stallCount,
          total_stall_time_ms: totalStallTime,
          model:
            options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          request_id: (streamRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
      }

      // Check if the cache actually broke based on response tokens
      if (feature('PROMPT_CACHE_BREAK_DETECTION')) {
        void checkResponseForCacheBreak(
          options.querySource,
          usage.cache_read_input_tokens,
          usage.cache_creation_input_tokens,
          messages,
          options.agentId,
          streamRequestId,
        )
      }

      // Process fallback percentage header and quota status if available
      // streamResponse is set when the stream is created in the withRetry callback above
      // TypeScript's control flow analysis can't track that streamResponse is set in the callback
      // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
      const resp = streamResponse as unknown as Response | undefined
      if (resp) {
        extractQuotaStatusFromHeaders(resp.headers)
        // Store headers for gateway detection
        responseHeaders = resp.headers
      }
    } catch (streamingError) {
      // Clear the idle timeout watchdog on error path too
      clearStreamIdleTimers()

      // Instrumentation: if the watchdog had already fired and the for-await
      // threw (rather than exiting cleanly), record that the loop DID exit and
      // how long after the watchdog. Distinguishes true hangs from error exits.
      if (streamIdleAborted && streamWatchdogFiredAt !== null) {
        const exitDelayMs = Math.round(
          performance.now() - streamWatchdogFiredAt,
        )
        logForDiagnosticsNoPII(
          'info',
          'cli_stream_loop_exited_after_watchdog_error',
        )
        logEvent('tengu_stream_loop_exited_after_watchdog', {
          request_id: (streamRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          exit_delay_ms: exitDelayMs,
          exit_path:
            'error' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          error_name:
            streamingError instanceof Error
              ? (streamingError.name as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
              : ('unknown' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS),
          model:
            options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
      }

      if (streamingError instanceof APIUserAbortError) {
        // Check if the abort signal was triggered by the user (ESC key)
        // If the signal is aborted, it's a user-initiated abort
        // If not, it's likely a timeout from the SDK
        if (signal.aborted) {
          // This is a real user abort (ESC key was pressed)
          logForDebugging(
            `Streaming aborted by user: ${errorMessage(streamingError)}`,
          )
          if (isAdvisorInProgress) {
            logEvent('tengu_advisor_tool_interrupted', {
              model:
                options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              advisor_model: (advisorModel ??
                'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
            })
          }
          throw streamingError
        } else {
          // The SDK threw APIUserAbortError but our signal wasn't aborted
          // This means it's a timeout from the SDK's internal timeout
          logForDebugging(
            `Streaming timeout (SDK abort): ${streamingError.message}`,
            { level: 'error' },
          )
          // Throw a more specific error for timeout
          throw new APIConnectionTimeoutError({ message: 'Request timed out' })
        }
      }

      if (streamingError instanceof AcosmiStreamDecodeError) {
        logForDiagnosticsNoPII('error', 'cli_stream_event_invalid_json')
        logForDebugging(
          `Acosmi stream decode failed without payload capture: event_name_encoded_len=${streamingError.eventNameEncodedLength}`,
          { level: 'error' },
        )
        releaseStreamResources()
        yield attachMediaSidecarUsage(
          createAssistantAPIErrorMessage({
            content:
              '模型流返回了无法解析的事件，本轮已安全停止；运行环境仍可继续使用。\nThe model stream returned an invalid event. This turn stopped safely and the runtime remains available.',
            error: 'server_error',
          }),
        )
        return
      }

      // PR-2 (2026-07-01 audit finding 2): custom/local models have no
      // "gateway non-streaming" downgrade tier. Falling back would ship the
      // full prompt to the Acosmi gateway (privacy leak + inference-time
      // entitlement bypass) and bury the real streaming error (e.g. a custom
      // endpoint 404) under a gateway model_not_found/402. Both tracks are
      // covered: prefixed refs (`custom:`/`local:`) and legacy bare custom
      // modelIds (Track 2, `resolveCustomModelRuntime` hit).
      if (isNonGatewayModelReference(options.model)) {
        logForDebugging(
          `Error streaming (non-gateway model — gateway non-streaming fallback suppressed): ${errorMessage(streamingError)}`,
          { level: 'error' },
        )
        throw streamingError
      }

      // When the flag is enabled, skip the non-streaming fallback and let the
      // error propagate to withRetry. The mid-stream fallback causes double tool
      // execution when streaming tool execution is active: the partial stream
      // starts a tool, then the non-streaming retry produces the same tool_use
      // and runs it again. See inc-4258.
      const disableFallback =
        isEnvTruthy(process.env.CRABCODE_DISABLE_NONSTREAMING_FALLBACK) ||
        getFeatureValue_CACHED_MAY_BE_STALE(
          'tengu_disable_streaming_to_non_streaming_fallback',
          false,
        )

      if (disableFallback) {
        logForDebugging(
          `Error streaming (non-streaming fallback disabled): ${errorMessage(streamingError)}`,
          { level: 'error' },
        )
        logEvent('tengu_streaming_fallback_to_non_streaming', {
          model:
            options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          error:
            streamingError instanceof Error
              ? (streamingError.name as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
              : (String(
                  streamingError,
                ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS),
          attemptNumber,
          maxOutputTokens,
          thinkingType:
            thinkingConfig.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          fallback_disabled: true,
          request_id: (streamRequestId ??
            'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
          fallback_cause: (streamIdleAborted
            ? 'watchdog'
            : 'other') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
        throw streamingError
      }

      logForDebugging(
        `Error streaming, falling back to non-streaming mode: ${errorMessage(streamingError)}`,
        { level: 'error' },
      )
      didFallBackToNonStreaming = true
      mediaSidecarUsedAttached = false
      if (options.onStreamingFallback) {
        options.onStreamingFallback()
      }

      logEvent('tengu_streaming_fallback_to_non_streaming', {
        model:
          options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        error:
          streamingError instanceof Error
            ? (streamingError.name as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
            : (String(
                streamingError,
              ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS),
        attemptNumber,
        maxOutputTokens,
        thinkingType:
          thinkingConfig.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        fallback_disabled: false,
        request_id: (streamRequestId ??
          'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        fallback_cause: (streamIdleAborted
          ? 'watchdog'
          : 'other') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })

      // Fall back to non-streaming mode with retries.
      // If the streaming failure was itself a 529, count it toward the
      // consecutive-529 budget so total 529s-before-model-fallback is the
      // same whether the overload was hit in streaming or non-streaming mode.
      // This is a speculative fix for https://github.com/acosmi/crabcode/issues/1513
      // Instrumentation: proves executeNonStreamingRequest was entered (vs. the
      // fallback event firing but the call itself hanging at dispatch).
      logForDiagnosticsNoPII('info', 'cli_nonstreaming_fallback_started')
      logEvent('tengu_nonstreaming_fallback_started', {
        request_id: (streamRequestId ??
          'unknown') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        model:
          options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        fallback_cause: (streamIdleAborted
          ? 'watchdog'
          : 'other') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })
      const result = yield* executeNonStreamingRequest(
        { model: options.model, source: options.querySource },
        {
          model: options.model,
          fallbackModel: options.fallbackModel,
          thinkingConfig,
          ...(isFastModeEnabled() && { fastMode: isFastMode }),
          signal,
          querySource: options.querySource,
        },
        paramsFromContext,
        (attempt, _startTime, tokens) => {
          attemptNumber = attempt
          maxOutputTokens = tokens
        },
        params => captureAPIRequest(params, options.querySource),
        streamRequestId,
      )

      // Empty response detection: if the non-streaming fallback returned no
      // content blocks, the upstream returned 200 OK but produced no output.
      // Throw instead of silently yielding an empty message (which shows as
      // blank to the user).
      const normalizedContent = normalizeContentFromAPI(
        result.content,
        tools,
        options.agentId,
      )
      if (
        !normalizedContent ||
        (Array.isArray(normalizedContent) && normalizedContent.length === 0)
      ) {
        throw new Error(
          `模型未返回任何内容 (model: ${options.model}, stop_reason: ${result.stop_reason ?? 'unknown'})`,
        )
      }

      const m: AssistantMessage = {
        message: {
          ...result,
          content: normalizedContent,
        } as AssistantMessage['message'],
        requestId: streamRequestId ?? undefined,
        type: 'assistant',
        uuid: randomUUID(),
        timestamp: new Date().toISOString(),
        ...(process.env.USER_TYPE === 'ant' &&
          research !== undefined && {
            research,
          }),
        ...(advisorModel && {
          advisorModel,
        }),
        ...(mediaSidecarUsed &&
          !mediaSidecarUsedAttached && { mediaSidecarUsed }),
      }
      if (mediaSidecarUsed && !mediaSidecarUsedAttached) {
        mediaSidecarUsedAttached = true
      }
      newMessages.push(m)
      fallbackMessage = m
      yield m
    } finally {
      clearStreamIdleTimers()
    }
  } catch (errorFromRetry) {
    // FallbackTriggeredError must propagate to query.ts, which performs the
    // actual model switch. Swallowing it here would turn the fallback into a
    // no-op — the user would just see "Model fallback triggered: X -> Y" as
    // an error message with no actual retry on the fallback model.
    if (errorFromRetry instanceof FallbackTriggeredError) {
      if (mediaSidecarUsed && !mediaSidecarUsedAttached) {
        errorFromRetry.mediaSidecarUsed = mediaSidecarUsed
      }
      throw errorFromRetry
    }

    // SDK NetworkError (timeout / EOF / DNS) — withRetry 已默认重试可重试错误,
    // 此处仅在 retry 链路最终失败时显示 friendly message.
    const sdkNetworkError =
      errorFromRetry instanceof CannotRetryError &&
      errorFromRetry.originalError instanceof Error &&
      errorFromRetry.originalError.name === 'NetworkError'
    if (sdkNetworkError) {
      logForDebugging(
        `SDK network unavailable during streaming: ${errorFromRetry.originalError.message}`,
        { level: 'warn' },
      )
      // 错误消息附带底层传输错误摘要 —— 2026-06-12 Win 真机报障只看到
      // 「网络连接中断」六个字，没有任何形态信息（timeout? reset? socket
      // closed?），无法远程定位。NetworkError.message = "op url: cause"
      // （SDK 端 URL 已脱敏），截断防刷屏。
      const networkCause = String(
        (errorFromRetry.originalError as Error).message ?? '',
      ).slice(0, 200)
      yield attachMediaSidecarUsage(
        createAssistantAPIErrorMessage({
          content:
            '网络连接中断，请稍后重试。\n' +
            'Network connection lost, please retry shortly.' +
            (networkCause ? `\n(${networkCause})` : ''),
        }),
      )
      releaseStreamResources()
      return
    }

    // SDK HTTPError 4xx 路径分流 (根因修 2026-05-05):
    //
    // 旧逻辑: 401/403 一律视为 token revoke, 触发 handleAuthExpiredInQuery
    //   (清 keychain + token cache + capabilities 磁盘 cache) + 引导重登。
    //
    // 实测发现: 网关 403 有三种语义 (HTTPError.type 区分,
    //   见 sdk-ts/index.mjs:177 `this.type = opts.type ?? ""`):
    //     - authentication_error: token 真失效 (清 cache 是对的)
    //     - permission_error: RBAC 拒绝 (例: "该模型未在你的套餐白名单中") — token 仍有效
    //     - not_found_error: 资源不存在 — token 仍有效
    //   后两种被旧逻辑误判 → 清掉有效 token + 模型 cache → 用户被迫重登 →
    //   重登也修不了 (根因是权限/资源, 不是 auth) → 死循环。
    //
    // 401 一律走 auth-expired (网关 401 = token 失效约定, 见 bridge-kick.ts:166)。
    // 403 仅当 type === 'authentication_error' 才走 auth-expired。
    // 其他 403 yield 网关原 message (含 type + body), 不动 token。
    const httpAuthErr =
      errorFromRetry instanceof HTTPError
        ? errorFromRetry
        : errorFromRetry instanceof CannotRetryError &&
          errorFromRetry.originalError instanceof HTTPError
        ? errorFromRetry.originalError
        : null
    if (httpAuthErr !== null) {
      logForDebugging(
        `[queryModel] streaming HTTPError caught status=${httpAuthErr.statusCode} type=${httpAuthErr.type ?? '(none)'} code=${(httpAuthErr as unknown as { code?: string }).code ?? '(none)'} message=${(httpAuthErr.message ?? '').slice(0, 300)} body=${((httpAuthErr as unknown as { body?: string }).body ?? '').slice(0, 500)}`,
      )
      const isAuthExpired =
        httpAuthErr.statusCode === 401 ||
        (httpAuthErr.statusCode === 403 &&
          httpAuthErr.type === 'authentication_error')

      if (isAuthExpired) {
        logForDebugging(
          `SDK auth expired during streaming: status=${httpAuthErr.statusCode} type=${httpAuthErr.type}`,
          { level: 'warn' },
        )
        await handleAuthExpiredInQuery()
        yield attachMediaSidecarUsage(
          createAssistantAPIErrorMessage({
            content:
              '认证已过期，请重新登录。\n' +
              'Authentication expired, please re-login.',
          }),
        )
        releaseStreamResources()
        return
      }

      if (httpAuthErr.statusCode === 429) {
        const raw429 = `${httpAuthErr.message || ''} ${httpAuthErr.body || ''}`
        if (raw429.includes('WINDOW_LIMIT_EXCEEDED')) {
          // V116.1 P1-3:文案原样提升为 errors.ts 共享常量(归一器五路同源)。
          yield attachMediaSidecarUsage(
            createAssistantAPIErrorMessage({
              content: WINDOW_LIMIT_EXCEEDED_MESSAGE,
            }),
          )
          releaseStreamResources()
          return
        }
      }

      if (httpAuthErr.statusCode === 403) {
        // permission_error / scope_missing / not_found_error / 空 type 的 403:
        // 不动 token, 透传网关原 message 让用户看到真实原因。
        logForDebugging(
          `SDK 403 (non-auth): type=${httpAuthErr.type} body=${(httpAuthErr.body || '').slice(0, 200)}`,
          { level: 'warn' },
        )
        // W-TUI-LOCKED-GATEWAY-MODELS D6② (2026-07-05): the nexus-v4 fix maps
        // LIMIT_POLICY_DENIED hold rejections to a 403 permission_error whose
        // body keeps the code (cross-repo contract). This yield short-circuits
        // before getAssistantMessageFromError, so match the code here too and
        // surface the unified membership guidance instead of the raw body.
        const raw403Message = httpAuthErr.message || ''
        yield attachMediaSidecarUsage(
          createAssistantAPIErrorMessage({
            content: raw403Message.includes('LIMIT_POLICY_DENIED')
              ? getGatewayEntitlementErrorMessage(options.model)
              : raw403Message ||
                '该请求被服务端拒绝 (403), 请检查模型/权限/套餐配置。',
          }),
        )
        releaseStreamResources()
        return
      }

      // V116.1 P1-3 (2026-07-24):402 / 5xx 归一呈现。此前这两类 fall through
      // 到 getAssistantMessageFromError 的文本嗅探 —— 500「校验流量包权益失败」
      // 被误映射为「需要会员权益」,免费用户看到无权益误导文案(事故客户端根因)。
      // 归一器按 status/errorCode 分类:402→额度用尽;500 带机器码→按码呈现;
      // 500 无码→通用上游故障。404 此处不返回(display=null),继续走下方
      // 非流式回退判定。
      {
        const gatewayDisplay = getGatewayErrorDisplay(
          normalizeGatewayError(httpAuthErr),
          options.model,
        )
        if (gatewayDisplay) {
          yield attachMediaSidecarUsage(
            createAssistantAPIErrorMessage(gatewayDisplay),
          )
          releaseStreamResources()
          return
        }
      }
    }

    // Check if this is a 404 error during stream creation that should trigger
    // non-streaming fallback. This handles gateways that return 404 for streaming
    // endpoints but work fine with non-streaming. Before v2.1.8, BetaMessageStream
    // threw 404s during iteration (caught by inner catch with fallback), but now
    // with raw streams, 404s are thrown during creation (caught here).
    // V116.1 P1-3 (2026-07-24):原判定基于本地 APIError(零构造点)恒 false ——
    // 该「404 流式端点回退非流式」设计行为自 Phase 3 起静默失效。按真实形态
    // (httpAuthErr 已从 CannotRetryError 解包出 SDK HTTPError)复活。
    const is404StreamCreationError =
      !didFallBackToNonStreaming &&
      httpAuthErr !== null &&
      httpAuthErr.statusCode === 404

    if (
      is404StreamCreationError &&
      !isNonGatewayModelReference(options.model)
    ) {
      // 404 is thrown at .withResponse() before streamRequestId is assigned,
      // and CannotRetryError means every retry failed. SDK HTTPError 不携带
      // request-id(旧 APIError.requestID 字段),固定 'unknown'。
      const failedRequestId = 'unknown'
      logForDebugging(
        'Streaming endpoint returned 404, falling back to non-streaming mode',
        { level: 'warn' },
      )
      didFallBackToNonStreaming = true
      mediaSidecarUsedAttached = false
      if (options.onStreamingFallback) {
        options.onStreamingFallback()
      }

      logEvent('tengu_streaming_fallback_to_non_streaming', {
        model:
          options.model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        error:
          '404_stream_creation' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        attemptNumber,
        maxOutputTokens,
        thinkingType:
          thinkingConfig.type as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        request_id:
          failedRequestId as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        fallback_cause:
          '404_stream_creation' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })

      try {
        // Fall back to non-streaming mode
        const result = yield* executeNonStreamingRequest(
          { model: options.model, source: options.querySource },
          {
            model: options.model,
            fallbackModel: options.fallbackModel,
            thinkingConfig,
            ...(isFastModeEnabled() && { fastMode: isFastMode }),
            signal,
          },
          paramsFromContext,
          (attempt, _startTime, tokens) => {
            attemptNumber = attempt
            maxOutputTokens = tokens
          },
          params => captureAPIRequest(params, options.querySource),
          failedRequestId,
        )

        const m: AssistantMessage = {
          message: {
            ...result,
            content: normalizeContentFromAPI(
              result.content,
              tools,
              options.agentId,
            ),
          } as AssistantMessage['message'],
          requestId: streamRequestId ?? undefined,
          type: 'assistant',
          uuid: randomUUID(),
          timestamp: new Date().toISOString(),
          ...(process.env.USER_TYPE === 'ant' &&
            research !== undefined && { research }),
          ...(advisorModel && { advisorModel }),
          ...(mediaSidecarUsed &&
            !mediaSidecarUsedAttached && { mediaSidecarUsed }),
        }
        if (mediaSidecarUsed && !mediaSidecarUsedAttached) {
          mediaSidecarUsedAttached = true
        }
        newMessages.push(m)
        fallbackMessage = m
        yield m

        // Continue to success logging below
      } catch (fallbackError) {
        // Propagate model-fallback signal to query.ts (see comment above).
        if (fallbackError instanceof FallbackTriggeredError) {
          if (mediaSidecarUsed && !mediaSidecarUsedAttached) {
            fallbackError.mediaSidecarUsed = mediaSidecarUsed
          }
          throw fallbackError
        }

        // Fallback also failed, handle as normal error
        logForDebugging(
          `Non-streaming fallback also failed: ${errorMessage(fallbackError)}`,
          { level: 'error' },
        )

        let error = fallbackError
        let errorModel = options.model
        if (fallbackError instanceof CannotRetryError) {
          error = fallbackError.originalError
          errorModel = fallbackError.retryContext.model
        }

        // V116.1 P1-3:extractQuotaStatusFromError 已改鸭子签名(其内部按旧
        // .status 早退,行为不变);SDK HTTPError 不携带 request-id 字段,
        // 旧 APIError.requestID 提取分支为死代码,requestId 仅取流内值。
        extractQuotaStatusFromError(error)

        const requestId = streamRequestId || undefined

        // F3-4: structured attempt bracket — failure terminal (fallback path
        // also failed). NoPII: error class name only, never the message.
        logForDiagnosticsNoPII('error', 'cli_streaming_attempt_failed', {
          attempt: attemptNumber,
          model: options.model,
          duration_ms: Date.now() - start,
          outcome:
            error instanceof APIUserAbortError ? 'user_abort' : 'error',
          error_name: error instanceof Error ? error.name : 'unknown',
        })

        logAPIError({
          error,
          model: errorModel,
          messageCount: messagesForAPI.length,
          messageTokens: tokenCountFromLastAPIResponse(messagesForAPI),
          durationMs: Date.now() - start,
          durationMsIncludingRetries: Date.now() - startIncludingRetries,
          attempt: attemptNumber,
          requestId,
          clientRequestId,
          didFallBackToNonStreaming,
          queryTracking: options.queryTracking,
          querySource: options.querySource,
          llmSpan,
          fastMode: isFastModeRequest,
          previousRequestId,
        })

        if (error instanceof APIUserAbortError) {
          releaseStreamResources()
          return
        }

        yield attachMediaSidecarUsage(
          getAssistantMessageFromError(error, errorModel, {
            messages,
            messagesForAPI,
          }),
        )
        releaseStreamResources()
        return
      }
    } else {
      // Original error handling for non-404 errors
      logForDebugging(`Error in API request: ${errorMessage(errorFromRetry)}`, {
        level: 'error',
      })

      let error = errorFromRetry
      let errorModel = options.model
      if (errorFromRetry instanceof CannotRetryError) {
        error = errorFromRetry.originalError
        errorModel = errorFromRetry.retryContext.model
      }

      // V116.1 P1-3:同 fallback 分支 —— extractQuotaStatusFromError 鸭子签名
      // (内部旧 .status 早退,行为不变);requestId 仅取流内值(HTTPError 无
      // request-id 字段,旧提取分支为死代码)。
      extractQuotaStatusFromError(error)

      const requestId = streamRequestId || undefined

      // F3-4: structured attempt bracket — failure terminal (retry chain
      // exhausted / non-retryable). NoPII: error class name only.
      logForDiagnosticsNoPII('error', 'cli_streaming_attempt_failed', {
        attempt: attemptNumber,
        model: options.model,
        duration_ms: Date.now() - start,
        outcome: error instanceof APIUserAbortError ? 'user_abort' : 'error',
        error_name: error instanceof Error ? error.name : 'unknown',
      })

      logAPIError({
        error,
        model: errorModel,
        messageCount: messagesForAPI.length,
        messageTokens: tokenCountFromLastAPIResponse(messagesForAPI),
        durationMs: Date.now() - start,
        durationMsIncludingRetries: Date.now() - startIncludingRetries,
        attempt: attemptNumber,
        requestId,
        clientRequestId,
        didFallBackToNonStreaming,
        queryTracking: options.queryTracking,
        querySource: options.querySource,
        llmSpan,
        fastMode: isFastModeRequest,
        previousRequestId,
      })

      // Don't yield an assistant error message for user aborts
      // The interruption message is handled in query.ts
      if (error instanceof APIUserAbortError) {
        releaseStreamResources()
        return
      }

      yield attachMediaSidecarUsage(
        getAssistantMessageFromError(error, errorModel, {
          messages,
          messagesForAPI,
        }),
      )
      releaseStreamResources()
      return
    }
  } finally {
    stopSessionActivity('api_call')
    // Must be in the finally block: if the generator is terminated early
    // via .return() (e.g. consumer breaks out of for-await-of, or query.ts
    // encounters an abort), code after the try/finally never executes.
    // Without this, the Response object's native TLS/socket buffers leak
    // until the generator itself is GC'd (see GH #32920).
    releaseStreamResources()

    // Non-streaming fallback cost: the streaming path tracks cost in the
    // message_delta handler before any yield. Fallback pushes to newMessages
    // then yields, so tracking must be here to survive .return() at the yield.
    if (fallbackMessage) {
      const fallbackUsage = fallbackMessage.message.usage
      usage = updateUsage(EMPTY_USAGE, fallbackUsage)
      stopReason = fallbackMessage.message.stop_reason
      const fallbackCost = calculateUSDCost(resolvedModel, fallbackUsage)
      costUSD += addToTotalSessionCost(
        fallbackCost,
        fallbackUsage,
        options.model,
      )
    }
  }

  // Mark all registered tools as sent to API so they become eligible for deletion
  if (feature('CACHED_MICROCOMPACT') && cachedMCEnabled) {
    markToolsSentToAPIState()
  }

  // Track the last requestId for the main conversation chain so shutdown
  // can send a cache eviction hint to inference. Exclude backgrounded
  // sessions (Ctrl+B) which share the repl_main_thread querySource but
  // run inside an agent context — they are independent conversation chains
  // whose cache should not be evicted when the foreground session clears.
  if (
    streamRequestId &&
    !getAgentContext() &&
    (options.querySource.startsWith('repl_main_thread') ||
      options.querySource === 'sdk')
  ) {
    setLastMainRequestId(streamRequestId)
  }

  // F3-4: structured attempt bracket — success terminal. Reached only when
  // the streaming loop (or its non-streaming fallback) produced a message.
  logForDiagnosticsNoPII('info', 'cli_streaming_attempt_completed', {
    attempt: attemptNumber,
    model: options.model,
    duration_ms: Date.now() - start,
    outcome: didFallBackToNonStreaming ? 'success_nonstreaming_fallback' : 'success',
  })

  // Precompute scalars so the fire-and-forget .then() closure doesn't pin the
  // full messagesForAPI array (the entire conversation up to the context window
  // limit) until getToolPermissionContext() resolves.
  const logMessageCount = messagesForAPI.length
  const logMessageTokens = tokenCountFromLastAPIResponse(messagesForAPI)
  void options.getToolPermissionContext().then(permissionContext => {
    logAPISuccessAndDuration({
      model:
        newMessages[0]?.message.model ?? partialMessage?.model ?? options.model,
      preNormalizedModel: options.model,
      usage,
      start,
      startIncludingRetries,
      attempt: attemptNumber,
      messageCount: logMessageCount,
      messageTokens: logMessageTokens,
      requestId: streamRequestId ?? null,
      stopReason,
      ttftMs,
      didFallBackToNonStreaming,
      querySource: options.querySource,
      headers: responseHeaders,
      costUSD,
      queryTracking: options.queryTracking,
      permissionMode: permissionContext.mode,
      // Pass newMessages for beta tracing - extraction happens in logging.ts
      // only when beta tracing is enabled
      newMessages,
      llmSpan,
      globalCacheStrategy,
      requestSetupMs: start - startIncludingRetries,
      attemptStartTimes,
      fastMode: isFastModeRequest,
      previousRequestId,
      betas: lastRequestBetas,
    })
  })

  // Defensive: also release on normal completion (no-op if finally already ran).
  releaseStreamResources()
}

/**
 * Cleans up stream resources to prevent memory leaks.
 * @internal Exported for testing
 */
export function cleanupStream(
  stream: Stream<NormalizedAcosmiChatStreamEvent> | undefined,
): void {
  if (!stream) {
    return
  }
  try {
    // Abort the stream via its controller if not already aborted
    if (!stream.controller.signal.aborted) {
      stream.controller.abort()
    }
  } catch {
    // Ignore - stream may already be closed
  }
}

export async function queryFastMode({
  systemPrompt = asSystemPrompt([]),
  userPrompt,
  outputFormat,
  signal,
  options,
}: {
  systemPrompt: SystemPrompt
  userPrompt: string
  outputFormat?: BetaJSONOutputFormat
  signal: AbortSignal
  options: FastModeOptions
}): Promise<AssistantMessage> {
  const { sessionModel, ...queryOptions } = options
  const route = resolveSessionAuxiliaryRoute({
    sessionModel,
    preferredAuxiliaryModel:
      sessionModel && isNonGatewayModelReference(sessionModel)
        ? sessionModel
        : getSmallFastModel(),
    accountBridgeRuntimeAccess: options.accountBridgeRuntimeAccess,
  })
  if (route.kind === 'skip') {
    throw new Error(
      `queryFastMode auxiliary route unavailable: ${route.reason}`,
    )
  }
  const result = await withVCR(
    [
      createUserMessage({
        content: systemPrompt.map(text => ({ type: 'text', text })),
      }),
      createUserMessage({
        content: userPrompt,
      }),
    ],
    async () => {
      const messages = [
        createUserMessage({
          content: userPrompt,
        }),
      ]

      const result = await queryModelWithoutStreaming({
        messages,
        systemPrompt,
        thinkingConfig: { type: 'disabled' },
        tools: [],
        signal,
        options: {
          ...queryOptions,
          model: route.model,
          accountBridgeRuntimeAccess:
            route.accountBridgeRuntimeAccess ?? undefined,
          enablePromptCaching: queryOptions.enablePromptCaching ?? false,
          outputFormat,
          async getToolPermissionContext() {
            return getEmptyToolPermissionContext()
          },
        },
      })
      return [result]
    },
  )
  // We don't use streaming for the fast-mode helper so this is safe
  return result[0]! as AssistantMessage
}

/**
 * Query a specific model through the CrabCode infrastructure.
 * This goes through the full query pipeline including proper authentication,
 * betas, and headers - unlike direct API calls.
 */
export async function queryWithModel({
  systemPrompt = asSystemPrompt([]),
  userPrompt,
  outputFormat,
  signal,
  options,
}: {
  systemPrompt: SystemPrompt
  userPrompt: string
  outputFormat?: BetaJSONOutputFormat
  signal: AbortSignal
  options: QueryWithModelOptions
}): Promise<AssistantMessage> {
  const result = await withVCR(
    [
      createUserMessage({
        content: systemPrompt.map(text => ({ type: 'text', text })),
      }),
      createUserMessage({
        content: userPrompt,
      }),
    ],
    async () => {
      const messages = [
        createUserMessage({
          content: userPrompt,
        }),
      ]

      const result = await queryModelWithoutStreaming({
        messages,
        systemPrompt,
        thinkingConfig: { type: 'disabled' },
        tools: [],
        signal,
        options: {
          ...options,
          enablePromptCaching: options.enablePromptCaching ?? false,
          outputFormat,
          async getToolPermissionContext() {
            return getEmptyToolPermissionContext()
          },
        },
      })
      return [result]
    },
  )
  return result[0]! as AssistantMessage
}
