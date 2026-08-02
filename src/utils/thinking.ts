// ANT-ONLY import markers must not be reordered
// resolveAntModel is ANT-ONLY; stub it for external builds.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const resolveAntModel = (_model: string): any => null
import type { Theme } from './theme.js'
import { feature } from './featurePolyfill.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../services/analytics/growthbook.js'
import { getCachedCapabilityWithDefaultFallback } from './model/modelCapabilities.js'
import { get3PModelCapabilityOverride } from './model/modelSupportOverrides.js'
import { resolveCustomModelDef } from './model/customModelResolver.js'
import { getAPIProvider } from './model/providers.js'
import { getSettingsWithErrors } from './settings/settings.js'
import { getAccountBridgeRouteCapability } from '../services/accountBridge/capabilityCache.js'

export type ThinkingConfig =
  | { type: 'adaptive' }
  | { type: 'enabled'; budgetTokens: number }
  | { type: 'disabled' }

/**
 * Build-time gate (feature) + runtime gate (GrowthBook). The build flag
 * controls code inclusion in external builds; the GB flag controls rollout.
 */
export function isUltrathinkEnabled(): boolean {
  if (!feature('ULTRATHINK')) {
    return false
  }
  return getFeatureValue_CACHED_MAY_BE_STALE('tengu_turtle_carbon', true)
}

/**
 * Check if text contains the "ultrathink" keyword.
 */
export function hasUltrathinkKeyword(text: string): boolean {
  return /\bultrathink\b/i.test(text)
}

/**
 * Find positions of "ultrathink" keyword in text (for UI highlighting/notification)
 */
export function findThinkingTriggerPositions(text: string): Array<{
  word: string
  start: number
  end: number
}> {
  const positions: Array<{ word: string; start: number; end: number }> = []
  // Fresh /g literal each call — String.prototype.matchAll copies lastIndex
  // from the source regex, so a shared instance would leak state from
  // hasUltrathinkKeyword's .test() into this call on the next render.
  const matches = text.matchAll(/\bultrathink\b/gi)

  for (const match of matches) {
    if (match.index !== undefined) {
      positions.push({
        word: match[0],
        start: match.index,
        end: match.index + match[0].length,
      })
    }
  }

  return positions
}

const RAINBOW_COLORS: Array<keyof Theme> = [
  'rainbow_red',
  'rainbow_orange',
  'rainbow_yellow',
  'rainbow_green',
  'rainbow_blue',
  'rainbow_indigo',
  'rainbow_violet',
]

const RAINBOW_SHIMMER_COLORS: Array<keyof Theme> = [
  'rainbow_red_shimmer',
  'rainbow_orange_shimmer',
  'rainbow_yellow_shimmer',
  'rainbow_green_shimmer',
  'rainbow_blue_shimmer',
  'rainbow_indigo_shimmer',
  'rainbow_violet_shimmer',
]

export function getRainbowColor(
  charIndex: number,
  shimmer: boolean = false,
): keyof Theme {
  const colors = shimmer ? RAINBOW_SHIMMER_COLORS : RAINBOW_COLORS
  return colors[charIndex % colors.length]!
}

// Acosmi provider: 全程乐观放行,由 SDK adapter 按 caps 兜底。caps-first
// 实现会被不全的 caps 缓存架空,导致用户报"新打包没有 Max 深度思考"。
export function modelSupportsThinking(model: string): boolean {
  const accountRoute = getAccountBridgeRouteCapability(model)
  if (accountRoute !== undefined) return accountRoute?.supportsThinking === true
  // PR-4 裁决 A (2026-07-01 audit finding 4): a configured custom model only
  // supports thinking when its entry declares it — the unconditional acosmi
  // `true` below is about GATEWAY models, and a custom endpoint that doesn't
  // understand a `thinking` config may reject the request outright. Covers
  // `custom:<id>` refs and legacy bare ids (routing-consistent resolver).
  const customDef = resolveCustomModelDef(model)
  if (customDef) return customDef.supportsThinking === true

  if (getAPIProvider() === 'acosmi') return true

  const result = getCachedCapabilityWithDefaultFallback(model, 'supports_thinking')
  if (result !== undefined) return result

  const supported3P = get3PModelCapabilityOverride(model, 'thinking')
  if (supported3P !== undefined) return supported3P
  if (process.env.USER_TYPE === 'ant') {
    if (resolveAntModel(model.toLowerCase())) return true
  }
  return false
}

export function modelSupportsAdaptiveThinking(model: string): boolean {
  const accountRoute = getAccountBridgeRouteCapability(model)
  if (accountRoute !== undefined) {
    return accountRoute?.supportsAdaptiveThinking === true
  }
  // W-CUSTOM-MODEL-PLUS-GATE PR-6 (2026-07-04, 审计 F3): a configured custom
  // model never gets `thinking:{type:'adaptive'}` — that type is an
  // Acosmi-gateway extension; third-party anthropic-compatible endpoints only
  // accept enabled/disabled (+budget_tokens) and may reject the request
  // outright. Mirrors modelSupportsThinking's custom-def gate above, so a
  // thinking-capable custom model takes the enabled+budget branch instead.
  if (resolveCustomModelDef(model)) return false

  if (getAPIProvider() === 'acosmi') return true

  const result = getCachedCapabilityWithDefaultFallback(
    model,
    'supports_adaptive_thinking',
  )
  if (result !== undefined) return result

  const supported3P = get3PModelCapabilityOverride(model, 'adaptive_thinking')
  if (supported3P !== undefined) return supported3P
  return false
}

export function shouldEnableThinkingByDefault(): boolean {
  if (process.env.MAX_THINKING_TOKENS) {
    return parseInt(process.env.MAX_THINKING_TOKENS, 10) > 0
  }

  const { settings } = getSettingsWithErrors()
  if (settings.alwaysThinkingEnabled === false) {
    return false
  }

  // IMPORTANT: Do not change default thinking enabled value without notifying
  // the model launch DRI and research. This can greatly affect model quality and
  // bashing.

  // Enable thinking by default unless explicitly disabled.
  return true
}
