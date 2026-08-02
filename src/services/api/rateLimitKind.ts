/**
 * 429 Rate-Limit 子类型判别 — 区分 transient（瞬时拥塞）与 quota（配额耗尽）。
 *
 * Classification contract:
 * - transient: Retry-After 缺失或 < 60s → 网关瞬时拥塞，允许 ≤ TRANSIENT_429_MAX_ATTEMPTS 次短退避
 * - quota:     Retry-After ≥ 60s        → 配额型限速，保持 fail-fast 不浪费 token
 *
 * 阈值 60s 来自经验：transient burst 通常网关侧几秒内恢复；
 * 配额 reset 至少分钟级以上，超过 60s 重试无意义。
 *
 * 此模块为纯函数 + 常量，不依赖 messages / auth 等模块，可被单测无副作用 import。
 */

export const TRANSIENT_RETRY_AFTER_THRESHOLD_SECONDS = 60
export const TRANSIENT_429_MAX_ATTEMPTS = 2

export type RateLimitKind = 'transient' | 'quota'

export type RateLimitDecision =
  | 'fail_fast'
  | 'retry_transient'
  | 'retry_exhausted'
  | 'retry_succeeded'

export function classifyRateLimitKind(
  retryAfterSeconds: number | null,
): RateLimitKind {
  if (
    retryAfterSeconds === null ||
    retryAfterSeconds < TRANSIENT_RETRY_AFTER_THRESHOLD_SECONDS
  ) {
    return 'transient'
  }
  return 'quota'
}
