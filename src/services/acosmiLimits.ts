// Acosmi quota limits — backed by @acosmi/sdk-ts Client.getBalance()
//
// The Acosmi system uses a token quota model (EntitlementBalance) instead of
// Acosmi's window-based rate limits (5h/7d). Balance data comes from
// SDK Client.getBalance() (GET /entitlements/balance).


import isEqual from 'lodash-es/isEqual.js'
import { getIsNonInteractiveSession } from '../bootstrap/state.js'
import { isAcosmiSubscriber } from '../utils/auth.js'
import { logError } from '../utils/log.js'
import { isEssentialTrafficOnly } from '../utils/privacyLevel.js'
import type { AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS } from './analytics/index.js'
import { logEvent } from './analytics/index.js'

// Re-export message functions from centralized location
export {
  getRateLimitErrorMessage,
  getRateLimitWarning,
  // 套餐临界阈值（used% ≥ 90% = 剩余 ≤ 10%）定义在
  // rateLimitMessages.ts，并经此 façade 对外。
  QUOTA_CRITICAL_USED_PCT,
} from './rateLimitMessages.js'

// Warning threshold: warn when remaining < 20% of quota（used% > 80%）。
// 注：更紧迫的「临界」档 = used% ≥ QUOTA_CRITICAL_USED_PCT（90%，剩余 ≤ 10%），
// 在 getQuotaWarningText 内对告警措辞做升级（不新增 status）。
const WARNING_THRESHOLD = 0.2

export type AcosmiLimits = {
  status: 'allowed' | 'allowed_warning' | 'rejected'
  /** Remaining tokens in current entitlements */
  tokenRemaining: number
  /** Total token quota across active entitlements */
  tokenQuota: number
  /** Remaining API calls in current entitlements */
  callRemaining: number
  /** Total call quota across active entitlements */
  callQuota: number
}

// Exported for testing only
export let currentLimits: AcosmiLimits = {
  status: 'allowed',
  tokenRemaining: 0,
  tokenQuota: 0,
  callRemaining: 0,
  callQuota: 0,
}

/**
 * Raw utilization from entitlement balance, exposed to statusline scripts
 * via getRawUtilization(). Returns token utilization as a 0-1 fraction.
 */
type RawUtilization = {
  token?: { utilization: number }
}
let rawUtilization: RawUtilization = {}

export function getRawUtilization(): RawUtilization {
  return rawUtilization
}

type StatusChangeListener = (limits: AcosmiLimits) => void
export const statusListeners: Set<StatusChangeListener> = new Set()

export function emitStatusChange(limits: AcosmiLimits) {
  currentLimits = limits
  statusListeners.forEach(listener => listener(limits))

  logEvent('tengu_acosmi_limits_status_changed', {
    status:
      limits.status as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    tokenRemaining: limits.tokenRemaining,
    tokenQuota: limits.tokenQuota,
  })
}

/**
 * Compute AcosmiLimits from EntitlementBalance.
 */
function computeLimitsFromBalance(balance: {
  totalTokenRemaining: number
  totalTokenQuota: number
  totalCallRemaining: number
  totalCallQuota: number
}): AcosmiLimits {
  const {
    totalTokenRemaining,
    totalTokenQuota,
    totalCallRemaining,
    totalCallQuota,
  } = balance

  let status: AcosmiLimits['status'] = 'allowed'

  if (totalTokenRemaining <= 0 || totalCallRemaining <= 0) {
    status = 'rejected'
  } else if (
    totalTokenQuota > 0 &&
    totalTokenRemaining / totalTokenQuota < WARNING_THRESHOLD
  ) {
    status = 'allowed_warning'
  }

  return {
    status,
    tokenRemaining: totalTokenRemaining,
    tokenQuota: totalTokenQuota,
    callRemaining: totalCallRemaining,
    callQuota: totalCallQuota,
  }
}

/**
 * Refresh balance from SDK and update limits.
 */
async function refreshBalanceFromIPC(): Promise<void> {
  const { getAcosmiClient } = await import('./acosmi/index.js')
  const { logForDebugging } = await import('../utils/debug.js')
  const client = await getAcosmiClient()
  let balance: Awaited<ReturnType<typeof client.getBalance>> | null = null
  try {
    balance = await client.getBalance()
  } catch (e) {
    logForDebugging(
      `[acosmiLimits] getBalance FAIL ${(e as Error)?.message}`,
    )
  }
  logForDebugging(
    `[acosmiLimits] getBalance returned ${balance == null ? 'null' : JSON.stringify(balance)}`,
  )
  if (!balance) {
    return // SDK unavailable / unauthorized, keep existing state
  }

  const newLimits = computeLimitsFromBalance(balance)

  // Update raw utilization for statusline scripts
  if (balance.totalTokenQuota > 0) {
    rawUtilization = {
      token: {
        utilization: balance.totalTokenUsed / balance.totalTokenQuota,
      },
    }
  } else {
    rawUtilization = {}
  }

  if (!isEqual(currentLimits, newLimits)) {
    emitStatusChange(newLimits)
  }
}

/**
 * Pre-check quota status on app startup via Acosmi SDK.
 */
export async function checkQuotaStatus(): Promise<void> {
  if (isEssentialTrafficOnly()) {
    return
  }

  if (!isAcosmiSubscriber()) {
    return
  }

  if (getIsNonInteractiveSession()) {
    return
  }

  try {
    await refreshBalanceFromIPC()
  } catch (e) {
    logError(e as Error)
  }
}

/**
 * Called after each API response to refresh quota state.
 * In the Acosmi system, the actual balance comes from SDK Client.getBalance(),
 * not HTTP headers.
 */
export function extractQuotaStatusFromHeaders(
  _headers: globalThis.Headers,
): void {
  if (!isAcosmiSubscriber()) {
    if (currentLimits.status !== 'allowed') {
      emitStatusChange({
        status: 'allowed',
        tokenRemaining: 0,
        tokenQuota: 0,
        callRemaining: 0,
        callQuota: 0,
      })
    }
    return
  }

  // Trigger async balance refresh from IPC (fire-and-forget)
  refreshBalanceFromIPC().catch(logError)
}

/**
 * Called on 429 errors to mark quota as rejected and refresh balance.
 *
 * V116.1 P1-3 (2026-07-24) 事实记录:参数原标注本地 APIError(零构造点),
 * 运行时实际收到 SDK HTTPError(其状态字段是 statusCode 而非 status),故本
 * 函数自 Phase 3 起恒早退、从未生效。本次仅诚实化签名(unknown + 鸭子读取,
 * 运行时行为不变);是否按 HTTPError.statusCode 复活配额置灰属产品决策,
 * 不随本事故修复顺带激活。
 */
export function extractQuotaStatusFromError(error: unknown): void {
  if (
    !isAcosmiSubscriber() ||
    (error as { status?: number }).status !== 429
  ) {
    return
  }

  try {
    // Immediately mark as rejected
    const rejectedLimits: AcosmiLimits = {
      ...currentLimits,
      status: 'rejected',
      tokenRemaining: 0,
    }

    if (!isEqual(currentLimits, rejectedLimits)) {
      emitStatusChange(rejectedLimits)
    }

    // Also trigger async balance refresh for accurate numbers
    refreshBalanceFromIPC().catch(logError)
  } catch (e) {
    logError(e as Error)
  }
}
