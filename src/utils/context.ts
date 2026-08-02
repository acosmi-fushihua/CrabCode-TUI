// ANT-ONLY import markers must not be reordered
import { CONTEXT_1M_BETA_HEADER } from '../constants/betas.js'
import { isEnvTruthy } from './envUtils.js'
import { resolveAntModel } from './model/antModels.js'
import { getCachedCapabilityWithDefaultFallback, getModelCapability } from './model/modelCapabilities.js'
import { getAPIProvider } from './model/providers.js'
import { resolveCustomModelDef } from './model/customModelResolver.js'

// Model context window size (200k tokens for all models right now)
export const MODEL_CONTEXT_WINDOW_DEFAULT = 200_000

// Maximum output tokens for compact operations
export const COMPACT_MAX_OUTPUT_TOKENS = 20_000

// Default max output tokens
const MAX_OUTPUT_TOKENS_DEFAULT = 32_000
const MAX_OUTPUT_TOKENS_UPPER_LIMIT = 64_000

// Capped default for slot-reservation optimization. BQ p99 output = 4,911
// tokens, so 32k/64k defaults over-reserve 8-16× slot capacity. With the cap
// enabled, <1% of requests hit the limit; those get one clean retry at 64k
// (see query.ts max_output_tokens_escalate). Cap is applied in
// crabcode.ts:getMaxOutputTokensForModel to avoid the growthbook→betas→context
// import cycle.
export const CAPPED_DEFAULT_MAX_TOKENS = 8_000
export const ESCALATED_MAX_TOKENS = 64_000

/**
 * Check if 1M context is disabled via environment variable.
 * Used by C4E admins to disable 1M context for HIPAA compliance.
 */
export function is1mContextDisabled(): boolean {
  return isEnvTruthy(process.env.CRABCODE_DISABLE_1M_CONTEXT)
}

export function has1mContext(model: string): boolean {
  if (is1mContextDisabled()) {
    return false
  }
  return /\[1m\]/i.test(model)
}

export function modelSupports1M(model: string): boolean {
  if (is1mContextDisabled()) {
    return false
  }
  // SDK capabilities (with default-model fallback) is the authoritative source.
  const result = getCachedCapabilityWithDefaultFallback(model, 'supports_1m_context')
  if (result !== undefined) return result
  // Acosmi: SDK caps are authoritative; conservative false on miss.
  if (getAPIProvider() === 'acosmi') return false
  return false
}

export function getContextWindowForModel(
  model: string,
  betas?: string[],
): number {
  // Allow override via environment variable (ant-only)
  // This takes precedence over all other context window resolution, including 1M detection,
  // so users can cap the effective context window for local decisions (auto-compact, etc.)
  // while still using a 1M-capable endpoint.
  if (
    process.env.USER_TYPE === 'ant' &&
    process.env.CRABCODE_MAX_CONTEXT_TOKENS
  ) {
    const override = parseInt(process.env.CRABCODE_MAX_CONTEXT_TOKENS, 10)
    if (!isNaN(override) && override > 0) {
      return override
    }
  }

  // [1m] suffix — explicit client-side opt-in, respected over all detection
  if (has1mContext(model)) {
    return 1_000_000
  }

  // Custom model: use the context window declared on the settings entry.
  // PR-4 (2026-07-01 audit finding 4): this used to be gated on
  // `getAPIProvider() === 'custom'` — a pure-env check that is never true for
  // TUI registry users — so every registry custom model silently ran on
  // the 200k default (auto-compact timing, attachment budgets, StatusLine %
  // all wrong for smaller-context endpoints). The resolver itself is the
  // gate now: it only matches configured entries (`custom:<id>` refs AND
  // legacy bare ids), returning undefined for gateway slugs.
  {
    const customDef = resolveCustomModelDef(model)
    if (customDef?.contextWindow) {
      return customDef.contextWindow
    }
  }

  const cap = getModelCapability(model)
  if (cap?.max_input_tokens && cap.max_input_tokens >= 100_000) {
    if (
      cap.max_input_tokens > MODEL_CONTEXT_WINDOW_DEFAULT &&
      is1mContextDisabled()
    ) {
      return MODEL_CONTEXT_WINDOW_DEFAULT
    }
    return cap.max_input_tokens
  }

  if (betas?.includes(CONTEXT_1M_BETA_HEADER) && modelSupports1M(model)) {
    return 1_000_000
  }
  if (process.env.USER_TYPE === 'ant') {
    const antModel = resolveAntModel(model)
    if (antModel?.contextWindow) {
      return antModel.contextWindow
    }
  }
  return MODEL_CONTEXT_WINDOW_DEFAULT
}

/**
 * Calculate context window usage percentage from token usage data.
 * Returns used and remaining percentages, or null values if no usage data.
 */
export function calculateContextPercentages(
  currentUsage: {
    input_tokens: number
    cache_creation_input_tokens: number
    cache_read_input_tokens: number
  } | null,
  contextWindowSize: number,
): { used: number | null; remaining: number | null } {
  if (!currentUsage) {
    return { used: null, remaining: null }
  }

  const totalInputTokens =
    currentUsage.input_tokens +
    currentUsage.cache_creation_input_tokens +
    currentUsage.cache_read_input_tokens

  const usedPercentage = Math.round(
    (totalInputTokens / contextWindowSize) * 100,
  )
  const clampedUsed = Math.min(100, Math.max(0, usedPercentage))

  return {
    used: clampedUsed,
    remaining: 100 - clampedUsed,
  }
}

/**
 * Returns the model's default and upper limit for max output tokens.
 */
export function getModelMaxOutputTokens(model: string): {
  default: number
  upperLimit: number
} {
  let defaultTokens: number
  let upperLimit: number

  // PR-4 (2026-07-01 audit finding 4): a configured custom model declares its
  // own output ceiling — the SDK/ant tables below only know gateway models,
  // so this field was collected by the form but never consumed (sending the
  // 32k/64k default to an upstream with a lower cap 4xx'd the request).
  {
    const customDef = resolveCustomModelDef(model)
    if (customDef?.maxOutputTokens && customDef.maxOutputTokens >= 1) {
      const declared = customDef.maxOutputTokens
      return {
        default: Math.min(declared, MAX_OUTPUT_TOKENS_DEFAULT),
        upperLimit: declared,
      }
    }
  }

  if (process.env.USER_TYPE === 'ant') {
    const antModel = resolveAntModel(model.toLowerCase())
    if (antModel) {
      defaultTokens = antModel.defaultMaxTokens ?? MAX_OUTPUT_TOKENS_DEFAULT
      upperLimit = antModel.upperMaxTokensLimit ?? MAX_OUTPUT_TOKENS_UPPER_LIMIT
      return { default: defaultTokens, upperLimit }
    }
  }

  // SDK capabilities — use max_output_tokens when available
  const cap = getModelCapability(model)
  if (cap?.capabilities?.max_output_tokens && cap.capabilities.max_output_tokens >= 4_096) {
    const sdkMax = cap.capabilities.max_output_tokens
    // Default to half of max for most models, except very high limits
    const sdkDefault = sdkMax >= 128_000 ? 64_000 : Math.min(32_000, sdkMax)
    return { default: sdkDefault, upperLimit: sdkMax }
  }

  // SDK caps are authoritative — no hardcoded model-specific fallbacks
  defaultTokens = MAX_OUTPUT_TOKENS_DEFAULT
  upperLimit = MAX_OUTPUT_TOKENS_UPPER_LIMIT

  // Apply SDK cache override for max_tokens if available
  if (cap?.max_tokens && cap.max_tokens >= 4_096) {
    upperLimit = cap.max_tokens
    defaultTokens = Math.min(defaultTokens, upperLimit)
  }

  return { default: defaultTokens, upperLimit }
}

/**
 * Returns the max thinking budget tokens for a given model. The max
 * thinking tokens should be strictly less than the max output tokens.
 *
 * Deprecated since newer models use adaptive thinking rather than a
 * strict thinking token budget.
 */
export function getMaxThinkingTokensForModel(model: string): number {
  return getModelMaxOutputTokens(model).upperLimit - 1
}
