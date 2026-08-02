import type { BetaMessageStreamParams } from '../../types/api-types.js'
import type {
  AccountBridgeModelRouteView,
  CrabCodeThinkingMode,
} from './types.js'
import type { ThinkingConfig } from '../../utils/thinking.js'
import type { EffortValue } from '../../utils/effort.js'
import type { Locale } from '../../i18n/config.js'
import { renderAccountBridgeReference } from '../../utils/model/accountBridgeReference.js'
export {
  resolveSupportedCrabCodeThinkingMode,
  type CrabCodeThinkingModeResolution,
} from './types.js'
import { resolveSupportedCrabCodeThinkingMode } from './types.js'

/**
 * Session-local result of resolving a user's thinking preference against one
 * exact Account Bridge route. The requested mode is retained so a later
 * preference change can be distinguished from provider capability drift.
 */
export interface AccountBridgeThinkingSelection {
  modelReference: string
  requestedMode: CrabCodeThinkingMode
  selectedMode: CrabCodeThinkingMode
}

export interface PreparedAccountBridgeThinkingMode {
  mode: CrabCodeThinkingMode
  selection: AccountBridgeThinkingSelection
}

export class UnsupportedThinkingModeError extends Error {
  readonly code = 'unsupported_thinking_mode'

  constructor(
    readonly routeId: string,
    readonly requestedMode: CrabCodeThinkingMode,
    readonly supportedModes: readonly CrabCodeThinkingMode[],
  ) {
    super(
      `Account route ${routeId} no longer supports thinking mode ${requestedMode}`,
    )
    this.name = 'UnsupportedThinkingModeError'
  }
}

export function deriveCrabCodeThinkingMode(
  thinkingConfig: ThinkingConfig | undefined,
  effortValue: EffortValue | undefined,
): CrabCodeThinkingMode {
  if (thinkingConfig === undefined) return 'auto'
  if (thinkingConfig.type === 'disabled') return 'off'
  if (thinkingConfig.type === 'adaptive' && effortValue === undefined) {
    return 'auto'
  }
  return effortValue === 'max' ? 'deep' : 'standard'
}

export function formatCrabCodeThinkingModeFallback(
  requested: CrabCodeThinkingMode,
  selected: CrabCodeThinkingMode,
  locale: Locale,
): string {
  const labels: Record<Locale, Record<CrabCodeThinkingMode, string>> = {
    'zh-CN': {
      auto: '自动',
      off: '关闭',
      standard: '标准',
      deep: '深度',
    },
    'en-US': {
      auto: 'Auto',
      off: 'Off',
      standard: 'Standard',
      deep: 'Deep',
    },
  }
  return locale === 'zh-CN'
    ? `思考档已回退：当前账户模型不支持“${labels[locale][requested]}”，已使用“${labels[locale][selected]}”。`
    : `Thinking mode fallback: the current account model does not support “${labels[locale][requested]}”; using “${labels[locale][selected]}” instead.`
}

export function assertAccountBridgeThinkingModeSupported(
  route: AccountBridgeModelRouteView,
  mode: CrabCodeThinkingMode,
): void {
  if (!route.supportedThinkingModes.includes(mode)) {
    throw new UnsupportedThinkingModeError(
      route.routeId,
      mode,
      route.supportedThinkingModes,
    )
  }
}

/**
 * Resolve thinking against the exact route already authorized for this turn,
 * preventing a capability check from racing against a different route view.
 */
export function prepareAccountBridgeThinkingModeForRoute(options: {
  route: AccountBridgeModelRouteView
  requestedMode: CrabCodeThinkingMode
  priorSelection?: AccountBridgeThinkingSelection
  locale: Locale
  onFallback?: (message: string) => void
}): PreparedAccountBridgeThinkingMode {
  const modelReference = renderAccountBridgeReference(options.route.routeId)
  const prior = options.priorSelection
  if (
    prior?.modelReference === modelReference &&
    prior.requestedMode === options.requestedMode
  ) {
    assertAccountBridgeThinkingModeSupported(
      options.route,
      prior.selectedMode,
    )
    return { mode: prior.selectedMode, selection: prior }
  }

  const resolution = resolveSupportedCrabCodeThinkingMode(
    options.requestedMode,
    options.route.supportedThinkingModes,
  )
  if (!resolution.ok) {
    throw new UnsupportedThinkingModeError(
      options.route.routeId,
      options.requestedMode,
      options.route.supportedThinkingModes,
    )
  }
  const selection: AccountBridgeThinkingSelection = {
    modelReference,
    requestedMode: options.requestedMode,
    selectedMode: resolution.mode,
  }
  if (resolution.fellBack) {
    options.onFallback?.(
      formatCrabCodeThinkingModeFallback(
        options.requestedMode,
        resolution.mode,
        options.locale,
      ),
    )
  }
  return { mode: resolution.mode, selection }
}

export function applyCrabCodeThinkingMode(
  params: BetaMessageStreamParams,
  mode: CrabCodeThinkingMode,
): BetaMessageStreamParams {
  const outputConfig = params.output_config
    ? { ...(params.output_config as Record<string, unknown>) }
    : undefined
  const next = { ...params } as BetaMessageStreamParams & Record<string, unknown>

  if (mode === 'auto') {
    delete next.thinking
    if (outputConfig) delete outputConfig.effort
  } else if (mode === 'off') {
    next.thinking = { type: 'disabled' }
    if (outputConfig) delete outputConfig.effort
  } else {
    next.thinking = { type: 'adaptive' }
    next.output_config = {
      ...outputConfig,
      effort: mode === 'deep' ? 'max' : 'high',
    } as BetaMessageStreamParams['output_config']
    return next
  }

  if (outputConfig && Object.keys(outputConfig).length > 0) {
    next.output_config = outputConfig as BetaMessageStreamParams['output_config']
  } else {
    delete next.output_config
  }
  return next
}
