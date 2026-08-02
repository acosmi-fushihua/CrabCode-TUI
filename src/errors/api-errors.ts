/**
 * api-errors.ts — 本地传输层哨兵错误类。
 *
 * V116.1 P1-3 (2026-07-24) 瘦身:原 Phase 3 从 @acosmi-ai/sdk 复刻的 APIError
 * 分类族(APIError 基类 + RateLimitError/AuthenticationError/NotFoundError +
 * .generate 工厂)全仓零构造点、instanceof 分支全部死代码,已整族删除 ——
 * HTTP 层错误的真实形态是 @acosmi/sdk-ts 的 `HTTPError`,分类一律走
 * `services/api/gatewayErrorNormalizer.ts` 归一结果。
 *
 * 仅保留三个**本地构造**的哨兵类(改为直接基于 Error,不再挂在 APIError 下):
 *   - APIConnectionError        传输层失败(无 HTTP 响应),withRetry 按它分类重试
 *   - APIConnectionTimeoutError 请求超时(APIConnectionError 子类,分类关系是
 *                               活语义:instanceof APIConnectionError 必须命中超时)
 *   - APIUserAbortError         用户主动中止(AbortSignal),消费方按 instanceof
 *                               与 name === 'APIUserAbortError' 双形态识别
 *
 * `status`/`headers`/`error` 保留为恒 undefined 的字段:既有消费点(logging/
 * withRetry)以 `.status` 宽访问,保持类型面不变、行为不变。
 */

/** Thrown when a network/transport error occurs (no HTTP response). */
export class APIConnectionError extends Error {
  readonly status: undefined = undefined
  readonly headers: undefined = undefined
  readonly error: undefined = undefined
  override cause: Error | undefined

  constructor({ message, cause }: { message?: string; cause?: Error } = {}) {
    super(message ?? 'Connection error.')
    this.name = 'APIConnectionError'
    this.cause = cause
  }
}

/** Thrown when a request times out. */
export class APIConnectionTimeoutError extends APIConnectionError {
  constructor({ message }: { message?: string } = {}) {
    super({ message: message ?? 'Request timed out.' })
    this.name = 'APIConnectionTimeoutError'
  }
}

/** Thrown when the user aborts a request via AbortSignal. */
export class APIUserAbortError extends Error {
  readonly status: undefined = undefined
  readonly headers: undefined = undefined
  readonly error: undefined = undefined

  constructor({ message }: { message?: string } = {}) {
    super(message ?? 'Request was aborted.')
    this.name = 'APIUserAbortError'
  }
}
