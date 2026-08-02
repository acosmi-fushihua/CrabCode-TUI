/**
 * Centralized rate limit message generation for Acosmi quota model.
 * Messages are based on token/call quota remaining (EntitlementBalance),
 * not Acosmi's window-based rate limits.
 */

import type { AcosmiLimits } from './acosmiLimits.js'

// Critical quota threshold: used% ≥ 90% (remaining ≤ 10%). Keep the value here
// so TUI warnings and status output share one definition.
export const QUOTA_CRITICAL_USED_PCT = 90

/**
 * All possible rate limit error message prefixes.
 * Export this to avoid fragile string matching in TUI components.
 */
export const RATE_LIMIT_ERROR_PREFIXES = [
  "You've hit your",
  "You've used",
  'Your quota is',
] as const

/**
 * Check if a message is a rate limit error.
 */
export function isRateLimitErrorMessage(text: string): boolean {
  return RATE_LIMIT_ERROR_PREFIXES.some(prefix => text.startsWith(prefix))
}

export type RateLimitMessage = {
  message: string
  severity: 'error' | 'warning'
}

/**
 * Get the appropriate rate limit message based on limit state.
 * Returns null if no message should be shown.
 */
export function getRateLimitMessage(
  limits: AcosmiLimits,
  _model: string,
): RateLimitMessage | null {
  // ERROR STATES — quota exhausted
  if (limits.status === 'rejected') {
    return { message: getQuotaExhaustedText(limits), severity: 'error' }
  }

  // WARNING STATES — approaching quota limit
  if (limits.status === 'allowed_warning') {
    const text = getQuotaWarningText(limits)
    if (text) {
      return { message: text, severity: 'warning' }
    }
  }

  return null
}

/**
 * Get error message for API errors (used in errors.ts).
 * Returns the message string or null if no error message should be shown.
 */
export function getRateLimitErrorMessage(
  limits: AcosmiLimits,
  model: string,
): string | null {
  const message = getRateLimitMessage(limits, model)
  if (message && message.severity === 'error') {
    return message.message
  }
  return null
}

/**
 * Get warning message for UI footer.
 * Returns the warning message string or null if no warning should be shown.
 */
export function getRateLimitWarning(
  limits: AcosmiLimits,
  model: string,
): string | null {
  const message = getRateLimitMessage(limits, model)
  if (message && message.severity === 'warning') {
    return message.message
  }
  return null
}

function getQuotaExhaustedText(limits: AcosmiLimits): string {
  if (limits.tokenRemaining <= 0 && limits.callRemaining <= 0) {
    return "You've hit your usage limit — tokens and calls exhausted"
  }
  if (limits.tokenRemaining <= 0) {
    return "You've hit your usage limit — token quota exhausted"
  }
  if (limits.callRemaining <= 0) {
    return "You've hit your usage limit — call quota exhausted"
  }
  return "You've hit your usage limit"
}

function getQuotaWarningText(limits: AcosmiLimits): string | null {
  if (limits.tokenQuota <= 0) {
    return null
  }

  const pctUsed = Math.floor(
    ((limits.tokenQuota - limits.tokenRemaining) / limits.tokenQuota) * 100,
  )

  // used% 达到临界阈值（90%）时升级措辞。沿用既有 'allowed_warning' 通路，不新增 status。
  if (pctUsed >= QUOTA_CRITICAL_USED_PCT) {
    return `You've used ${pctUsed}% of your token quota — running low`
  }

  return `You've used ${pctUsed}% of your token quota`
}
