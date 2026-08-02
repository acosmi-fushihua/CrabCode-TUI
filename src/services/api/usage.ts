import type { WindowLimitStatus } from '@acosmi/sdk-ts'
import { getAcosmiClient } from '../acosmi/index.js'
import { getAcosmiOAuthTokens, isAcosmiSubscriber } from '../../utils/auth.js'
import { isOAuthTokenExpired } from '../oauth/client.js'

export type RateLimit = {
  utilization: number | null // a percentage from 0 to 100
  resets_at: string | null // ISO 8601 timestamp
  // [W1 2026-07-11 软限额] 该窗此刻可否被用户「继续」开关豁免 (5h 软限时 true; 周窗恒缺失)。
  // 仅在 true 时置位, 保持旧形状向后兼容。
  overridable?: boolean
}

export type ExtraUsage = {
  is_enabled: boolean
  monthly_limit: number | null
  used_credits: number | null
  utilization: number | null
}

// W1 credit-window (2026-07-10): five_hour / seven_day 由 Acosmi 平台
// `GET /api/v4/entitlements/quota-summary` 的 `windowLimits`
// (FIVE_HOUR / WEEKLY 滚动窗口, 与服务端 429 拦截闸同源) 派生 —— 旧实现打的
// `/api/oauth/usage` 是上游遗留端点, Acosmi 网关从未实现, 进度条恒不渲染。
// `extra_usage` 是上游遗留形状 (Acosmi 网关无此数据, 恒 undefined), 仅为
// /extra-usage 与 reviewRemote 的降级分支保留类型位。
export type Utilization = {
  five_hour?: RateLimit | null
  seven_day?: RateLimit | null
  extra_usage?: ExtraUsage | null
  // [W1 2026-07-11 软限额] 用户 5h「继续」开关现值 (true=软限额/默认: 到达 5h 仅提醒并继续消耗周配额;
  // false=严格模式: 到达即等待恢复)。仅后端启用窗口限额时下发; 展示端据此渲染开关 + "正在消耗周配额"提示。
  five_hour_continue_enabled?: boolean
}

export type FiveHourLimitState =
  | 'soft-exceeded'
  | 'strict-exceeded'
  | 'hard-exceeded'

/**
 * Derive the message state for a full 5-hour window without confusing the
 * user's saved preference with the server's current ability to override it.
 */
export function getFiveHourLimitState(
  utilization: Utilization | null,
): FiveHourLimitState | null {
  const limit = utilization?.five_hour
  if (limit?.utilization == null || limit.utilization < 100) return null
  if (limit.overridable === false) return 'hard-exceeded'
  if (limit.overridable !== true) return null
  if (utilization?.five_hour_continue_enabled === true) return 'soft-exceeded'
  if (utilization?.five_hour_continue_enabled === false) return 'strict-exceeded'
  return null
}

/** 最小 SDK 写接口；可注入 fake 以单测 source / 服务端回显语义。 */
export interface WindowContinuePreferenceClient {
  setWindowContinuePreference(
    enabled: boolean,
    options?: { source?: string },
  ): Promise<{ enabled: boolean }>
}

export interface UsageFetchDependencies {
  isSubscriber: () => boolean
  getTokens: () => { expiresAt: number | null; scopes?: readonly string[] } | null
  getClient: () => Promise<{
    getQuotaSummary: () => Promise<{
      windowLimits?: WindowLimitStatus[]
      windowFiveHourContinueEnabled?: boolean
    }>
  }>
}

const DEFAULT_USAGE_FETCH_DEPENDENCIES: UsageFetchDependencies = {
  isSubscriber: isAcosmiSubscriber,
  getTokens: getAcosmiOAuthTokens,
  getClient: getAcosmiClient,
}

/** TUI `/usage` 的 5 小时续用开关写入。 */
export async function setFiveHourContinuePreference(
  enabled: boolean,
  client?: WindowContinuePreferenceClient,
): Promise<boolean> {
  const resolvedClient = client ?? (await getAcosmiClient())
  const result = await resolvedClient.setWindowContinuePreference(enabled, {
    source: 'crabcode-tui',
  })
  if (typeof result?.enabled !== 'boolean') {
    throw new Error('setWindowContinuePreference response missing enabled')
  }
  return result.enabled
}

/**
 * 纯映射: quota-summary `windowLimits` → 遗留 `Utilization` 形状。
 *
 * - 百分比 = usedCredits / limitCredits * 100, clamp [0, 100];
 *   limitCredits <= 0 = 显式禁止档 (与服务端 hold 闸同语义) → 恒 100%。
 * - `resetAt` (ISO-8601, 窗口内无用量时缺失) → `resets_at`。
 * - 未启用窗口限额 (windowLimits 缺失/空) → `{}`: 面板不渲染窗口条,
 *   仅显示权益余量 —— 与开关关闭时的服务端行为对齐。
 */
export function windowLimitsToUtilization(
  windowLimits: readonly WindowLimitStatus[] | null | undefined,
  fiveHourContinueEnabled?: boolean,
): Utilization {
  const utilization: Utilization = {}
  for (const w of windowLimits ?? []) {
    const mapped = windowToRateLimit(w)
    if (mapped === null) continue
    if (w.kind === 'FIVE_HOUR') utilization.five_hour = mapped
    else if (w.kind === 'WEEKLY') utilization.seven_day = mapped
  }
  // [W1 2026-07-11 软限额] 5h「继续」开关现值 (仅 boolean 时置位, 保持旧形状向后兼容)。
  if (typeof fiveHourContinueEnabled === 'boolean') {
    utilization.five_hour_continue_enabled = fiveHourContinueEnabled
  }
  return utilization
}

function windowToRateLimit(w: WindowLimitStatus): RateLimit | null {
  if (typeof w.limitCredits !== 'number' || typeof w.usedCredits !== 'number') {
    return null
  }
  const percent =
    w.limitCredits <= 0
      ? 100
      : Math.min(100, Math.max(0, (w.usedCredits / w.limitCredits) * 100))
  const rl: RateLimit = { utilization: percent, resets_at: w.resetAt ?? null }
  // 仅 true 时置位 overridable (旧形状向后兼容; 周窗恒不置位)。
  if (w.overridable === true) rl.overridable = true
  return rl
}

export async function fetchUtilization(
  dependencies: UsageFetchDependencies = DEFAULT_USAGE_FETCH_DEPENDENCIES,
): Promise<Utilization | null> {
  // quota-summary 需要 entitlements scope；桌面 OAuth 分组 `ai` 已由网关展开
  // 包含 entitlements。旧的 hasProfileScope(account) 门槛会误拦默认
  // `ai` token，造成 /usage 总额度能显示但 5h/周窗口恒缺失。
  if (!dependencies.isSubscriber()) {
    return {}
  }

  // Skip the SDK call if the OAuth token is expired to avoid 401 errors
  const tokens = dependencies.getTokens()
  if (tokens && isOAuthTokenExpired(tokens.expiresAt)) {
    return null
  }

  const client = await dependencies.getClient()
  const summary = await client.getQuotaSummary()
  return windowLimitsToUtilization(
    summary.windowLimits,
    summary.windowFiveHourContinueEnabled,
  )
}
