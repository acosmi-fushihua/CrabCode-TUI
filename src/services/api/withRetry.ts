import type { Acosmi } from '../../types/api-types.js'

import {
  APIConnectionError,
  APIUserAbortError,
} from '../../errors/api-errors.js'
import { HTTPError } from '@acosmi/sdk-ts'
import type { QuerySource } from 'src/constants/querySource.js'
import type { AssistantMessage, SystemAPIErrorMessage } from 'src/types/message.js'
import { logForDebugging } from 'src/utils/debug.js'
import { getAPIProviderForStatsig } from 'src/utils/model/providers.js'
import { errorMessage } from '../../utils/errors.js'
import { isFastModeEnabled } from '../../utils/fastMode.js'
import { disableKeepAlive } from '../../utils/proxy.js'
import { sleep } from '../../utils/sleep.js'
import type { ThinkingConfig } from '../../utils/thinking.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../analytics/growthbook.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import { extractConnectionErrorDetails } from './errorUtils.js'

const abortError = () => new APIUserAbortError()

const DEFAULT_MAX_RETRIES = 10
export const BASE_DELAY_MS = 500

// 瞬时 NetworkError（传输层 timeout/EOF）专用重试预算。独立于 maxRetries：
// 真正断网时 ~3 次即快速失败，不耗满 10 次的总预算。
const NETWORK_ERROR_MAX_ATTEMPTS = 3

// 429 subtype classification.
// 纯函数 + 常量提取至 ./rateLimitKind.ts，便于单测无副作用 import
import type { RateLimitKind } from './rateLimitKind.js'
import { classifyRateLimitKind } from './rateLimitKind.js'
export {
  classifyRateLimitKind,
  TRANSIENT_429_MAX_ATTEMPTS,
  TRANSIENT_RETRY_AFTER_THRESHOLD_SECONDS,
} from './rateLimitKind.js'

// [W1 2026-07-11 软限额] 窗口限额 (WINDOW_LIMIT_EXCEEDED) 结构化识别 —— 跨仓机器码契约:
// Java/Go 保证 5h/7d 滚动窗口 429 的顶层 errorCode 与文案均含 WINDOW_LIMIT_EXCEEDED。
// V116.1 P1-3 (2026-07-24) 订正:运行时错误形态是 SDK HTTPError(errorCode/
// windowOverridable/windowKind/windowResetAt 为**顶层字段**,由 SDK 解析响应体);
// 旧签名标注 APIError 但该类族零构造点,实际一直靠 .message 子串兜底才活着。
// 现按真实形态三层读取:HTTPError 顶层 → 鸭子 .error 响应体(防御) → message 子串。
// overridable=true = 5h 严格模式 (用户显式关闭继续), 可经 setWindowContinuePreference(true)
// 重开后续跑; 缺失/false = 周窗/显式禁止 (硬等待)。
export function getWindowLimitDetailFromError(error: unknown): {
  isWindowLimit: boolean
  overridable: boolean
  windowKind?: string
  windowResetAt?: string
} {
  const top = error instanceof HTTPError ? error : undefined
  const body = ((error as { error?: unknown } | null | undefined)?.error ?? {}) as Record<
    string,
    unknown
  >
  const msg = typeof (error as { message?: unknown })?.message === 'string' ? (error as { message: string }).message : ''
  const isWindowLimit =
    top?.errorCode === 'WINDOW_LIMIT_EXCEEDED' ||
    body.errorCode === 'WINDOW_LIMIT_EXCEEDED' ||
    msg.includes('WINDOW_LIMIT_EXCEEDED')
  return {
    isWindowLimit,
    overridable: top?.windowOverridable === true || body.windowOverridable === true,
    windowKind:
      top?.windowKind ??
      (typeof body.windowKind === 'string' ? body.windowKind : undefined),
    windowResetAt:
      top?.windowResetAt ??
      (typeof body.windowResetAt === 'string' ? body.windowResetAt : undefined),
  }
}

// Exported so sideQuery.ts (X1) can classify 429s the same way the main
// retry loop does — one source of truth for transient/quota classification,
// not a second heuristic: do not guess gateway intent.
export function getRateLimitKindFromError(error: unknown): RateLimitKind {
  // [W1 2026-07-11 软限额] 窗口限额 429 结构化优先: 无论 Retry-After 多少一律 fail-fast (quota),
  // 绝不当 transient 自动重试 —— resetAt 前重试必再拒 (5h 严格模式须用户重开继续开关, 周窗须等恢复)。
  // 修 §4.0-3 的 <60s 被误判 transient 自动重试 2 次的缺陷 (5h 窗口临近恢复点 Retry-After 常 <60s)。
  if (getWindowLimitDetailFromError(error).isWindowLimit) return 'quota'
  const ms = getRetryAfterMs(error)
  return classifyRateLimitKind(ms === null ? null : Math.floor(ms / 1000))
}

// ============================================================================
// V116.1 P1-3 (2026-07-24) 死机器清除记录:
// 本模块曾有一整套围绕本地 APIError 分类族的重试机器 —— 529 前台/后台分流
// (is529Error/shouldRetry529/FOREGROUND_529_RETRY_SOURCES)、订阅者 transient-429
// 小预算(logRateLimitStrategy/transient429Attempts)、模型回退触发
// (isModelNotFoundError)、fast-mode 429 冷却、CRABCODE_UNATTENDED_RETRY 持久
// 重试(persistent*)、shouldRetry/x-should-retry 决策树。APIError 全仓零构造点,
// 上述全部分支自 Phase 3 SDK 迁移起 instanceof 门恒假、运行时从未执行 ——
// HTTP 错误的真实行为一直是「首错即抛 CannotRetryError」(SDK 传输层自带其
// 重试策略;TS 层不重放 HTTP POST 也是 V116.1 计划要锁死的防重放不变量)。
// 按计划连同类族删除;若未来要恢复某段能力,必须基于 SDK HTTPError 真实形态
// 重新设计并配防重放测试,不得回抄旧块。
// 例外(经审定保留):sideQuery.ts 的 X1 有界 transient-429 短重试 —— §8 仓库
// 契约 + 专用防滥用测试;已订正为 HTTPError 识别(429 = 请求被拒无副作用,
// 不违反防重放不变量)。主循环不做任何 HTTP 重试。
// ============================================================================

// Retry-After 上限:getRetryDelay 对服务器指令有意绕过 maxDelayMs,无上限时
// 一个病态的远期 Retry-After(如几个月后的 HTTP-date)会无界钉死整轮。
// 60s 之外就不是值得阻塞等待的瞬时抖动。
const RETRY_AFTER_CAP_MS = 60_000

function isStaleConnectionError(error: unknown): boolean {
  if (!(error instanceof APIConnectionError)) {
    return false
  }
  const details = extractConnectionErrorDetails(error)
  return details?.code === 'ECONNRESET' || details?.code === 'EPIPE'
}

// SDK NetworkError 是传输层错误，extends Error 而非 APIError，因此走不到
// shouldRetry()。SDK 自身的 defaultRetryable 策略把 isTimeout()/isEOF() 的
// NetworkError 视为可重试，但 SDK 仅在 doRequestWithRetry（非流式、且仅
// GET/HEAD/OPTIONS）应用 —— chat POST（流式与非流式）都拿不到 SDK 传输层
// 重试。withRetry 必须在此补上分类，否则一次瞬时抖动就丢弃整轮对话。
//
// 2026-06-12 起：**所有** NetworkError 共享 NETWORK_ERROR_MAX_ATTEMPTS 小预算，
// 不再只认 isTimeout()/isEOF()。真机取证（Win 1.3.44，Ctrl+C 中断后重发）：
// SDK classifyTransport 只给 AbortError/UND_ERR_*_TIMEOUT 打 timeout、给
// ECONNRESET/EPIPE/ECONNREFUSED/EAI_AGAIN 打 eof，其余传输错误形态（Bun/
// Windows 的 socket-closed、TLS、H2 GOAWAY、UND_ERR_SOCKET 等）两标皆无 →
// 旧逻辑零重试直接透出「网络连接中断」，一次毫秒级连接抖动就终结整轮。
// 连接期失败请求未达服务端，重试安全；用户中断（signal abort）在下方退避
// sleep 处抛 APIUserAbortError，不会被这里的放宽吞掉。
function isRetryableNetworkError(error: unknown): boolean {
  return error instanceof Error && error.name === 'NetworkError'
}

export interface RetryContext {
  maxTokensOverride?: number
  model: string
  thinkingConfig: ThinkingConfig
  fastMode?: boolean
}

interface RetryOptions {
  maxRetries?: number
  model: string
  fallbackModel?: string
  thinkingConfig: ThinkingConfig
  fastMode?: boolean
  signal?: AbortSignal
  querySource?: QuerySource
  /**
   * @deprecated V116.1 P1-3:529 连败计数器随死机器一并删除(见模块头记录),
   * 字段仅为兼容既有调用方保留,无任何行为。
   */
  initialConsecutive529Errors?: number
}

export class CannotRetryError extends Error {
  constructor(
    public readonly originalError: unknown,
    public readonly retryContext: RetryContext,
  ) {
    const message = errorMessage(originalError)
    super(message)
    this.name = 'RetryError'

    // Preserve the original stack trace if available
    if (originalError instanceof Error && originalError.stack) {
      this.stack = originalError.stack
    }
  }
}

export class FallbackTriggeredError extends Error {
  /** Turn-level media usage incurred before the model switch, if any. */
  public mediaSidecarUsed?: NonNullable<AssistantMessage['mediaSidecarUsed']>

  constructor(
    public readonly originalModel: string,
    public readonly fallbackModel: string,
  ) {
    super(`Model fallback triggered: ${originalModel} -> ${fallbackModel}`)
    this.name = 'FallbackTriggeredError'
  }
}

export async function* withRetry<T>(
  getClient: () => Promise<Acosmi>,
  operation: (
    client: Acosmi,
    attempt: number,
    context: RetryContext,
  ) => Promise<T>,
  options: RetryOptions,
): AsyncGenerator<SystemAPIErrorMessage, T> {
  const maxRetries = getMaxRetries(options)
  const retryContext: RetryContext = {
    model: options.model,
    thinkingConfig: options.thinkingConfig,
    ...(isFastModeEnabled() && { fastMode: options.fastMode }),
  }
  let client: Acosmi | null = null
  let lastError: unknown
  // 瞬时 NetworkError 重试计数
  let networkErrorAttempts = 0
  for (let attempt = 1; attempt <= maxRetries + 1; attempt++) {
    if (options.signal?.aborted) {
      throw new APIUserAbortError()
    }

    try {
      // Get a fresh client instance on first attempt or after authentication errors
      // - 401 for first-party API authentication failures
      // - 403 "OAuth token has been revoked" (another process refreshed the token)
      // - Bedrock-specific auth errors (403 or CredentialsProviderError)
      // - Vertex-specific auth errors (credential refresh failures, 401)
      // - ECONNRESET/EPIPE: stale keep-alive socket; disable pooling and reconnect
      const isStaleConnection = isStaleConnectionError(lastError)
      if (
        isStaleConnection &&
        getFeatureValue_CACHED_MAY_BE_STALE(
          'tengu_disable_keepalive_on_econnreset',
          false,
        )
      ) {
        logForDebugging(
          'Stale connection (ECONNRESET/EPIPE) — disabling keep-alive for retry',
        )
        disableKeepAlive()
      }

      if (client === null || isStaleConnection) {
        client = await getClient()
      }

      return await operation(client, attempt, retryContext)
    } catch (error) {
      lastError = error
      logForDebugging(
        `API error (attempt ${attempt}/${maxRetries + 1}): ${errorMessage(error)}`,
        { level: 'error' },
      )

      if (attempt > maxRetries) {
        throw new CannotRetryError(error, retryContext)
      }

      // 瞬时 SDK NetworkError（timeout/EOF）—— 用独立小预算重试，落入下方
      // 退避/重试块。镜像 SDK 自身的 defaultRetryable 策略；独立计数确保真正
      // 断网时 ~3 次即快速失败。
      if (isRetryableNetworkError(error)) {
        networkErrorAttempts++
        if (networkErrorAttempts > NETWORK_ERROR_MAX_ATTEMPTS) {
          throw new CannotRetryError(error, retryContext)
        }
        // fall through 到下方退避/重试块
      } else {
        // V116.1 P1-3:HTTP/其他错误在 TS 层一律不重放(自 Phase 3 起的运行时
        // 现状,亦是防重放不变量 —— 402/403/5xx 重放 = 重复计费/重复副作用风险;
        // SDK 传输层自带其重试策略)。仅瞬时 NetworkError 走上方小预算退避。
        throw new CannotRetryError(error, retryContext)
      }

      // 仅 NetworkError 到达此处:按退避重试(Retry-After 对传输层错误不存在,
      // getRetryAfter 鸭子读取返回 null,getRetryDelay 走指数退避)。
      const retryAfter = getRetryAfter(error)
      const delayMs = Math.min(
        getRetryDelay(attempt, retryAfter),
        RETRY_AFTER_CAP_MS,
      )

      logEvent('tengu_api_retry', {
        attempt,
        delayMs: delayMs,
        error: errorMessage(
          error,
        ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        status: (error as { status?: number }).status,
        provider: getAPIProviderForStatsig(),
      })

      await sleep(delayMs, options.signal, { abortError })
    }
  }

  throw new CannotRetryError(lastError, retryContext)
}

// Exported so sideQuery.ts (X1) can pass the raw header through to
// getRetryDelay for consistent backoff computation (no behavior change here).
export function getRetryAfter(error: unknown): string | null {
  return (
    ((error as { headers?: { 'retry-after'?: string } }).headers?.[
      'retry-after'
    ] ||
      // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
      ((error as { headers?: Headers }).headers as Headers)?.get?.(
        'retry-after',
      )) ??
    null
  )
}

// Parse an HTTP `Retry-After` header value to milliseconds per RFC 7231 §7.1.3,
// which permits BOTH forms:
//   - delta-seconds: "120"                          → 120_000
//   - HTTP-date:     "Wed, 21 Oct 2026 07:28:00 GMT" → ms until that instant
// The delta-seconds branch preserves the prior `parseInt` behavior exactly;
// the HTTP-date branch is only tried when that yields NaN (an HTTP-date always
// starts with a weekday name, so it never parses as an integer). A date already
// in the past clamps to 0 (retry now). Returns null if neither form parses.
// This is standards-conformant header parsing, NOT a heuristic guess of gateway
// intent — the 429 transient/quota classification downstream is
// unchanged; it just no longer mis-floors a valid HTTP-date to a 30min cooldown.
function parseRetryAfterMs(retryAfter: string): number | null {
  const seconds = parseInt(retryAfter, 10)
  if (!Number.isNaN(seconds)) {
    return seconds * 1000
  }
  const dateMs = Date.parse(retryAfter)
  if (!Number.isNaN(dateMs)) {
    return Math.max(0, dateMs - Date.now())
  }
  return null
}

export function getRetryDelay(
  attempt: number,
  retryAfterHeader?: string | null,
  maxDelayMs = 32000,
): number {
  if (retryAfterHeader) {
    const ms = parseRetryAfterMs(retryAfterHeader)
    if (ms !== null) {
      return ms
    }
  }

  const baseDelay = Math.min(
    BASE_DELAY_MS * Math.pow(2, attempt - 1),
    maxDelayMs,
  )
  const jitter = Math.random() * 0.25 * baseDelay
  return baseDelay + jitter
}

export function getDefaultMaxRetries(): number {
  // NaN guard (审计 BUG-5): a non-numeric value (e.g. `=unlimited`) would make the
  // retry loop bound `attempt <= NaN + 1` always false — zero attempts, every API
  // call throws. Same gate as BackgroundAgentScheduler.readCapacityFromEnv.
  if (process.env.CRABCODE_MAX_RETRIES) {
    const parsed = parseInt(process.env.CRABCODE_MAX_RETRIES, 10)
    if (!Number.isNaN(parsed) && parsed >= 0) {
      return parsed
    }
  }
  return DEFAULT_MAX_RETRIES
}
function getMaxRetries(options: RetryOptions): number {
  return options.maxRetries ?? getDefaultMaxRetries()
}

function getRetryAfterMs(error: unknown): number | null {
  // V116.1 P1-3:SDK HTTPError 把 Retry-After 头解析为顶层 retryAfter 秒数
  // (0 = 未提供),不携带原始 Headers —— 顶层优先,鸭子 headers 兜底(测试/
  // 旧形态)。没有这一读,HTTPError 429 的 quota(≥60s)会被恒判 transient。
  if (error instanceof HTTPError && error.retryAfter > 0) {
    return error.retryAfter * 1000
  }
  const retryAfter = getRetryAfter(error)
  if (retryAfter) {
    return parseRetryAfterMs(retryAfter)
  }
  return null
}
