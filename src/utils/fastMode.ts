import axios from 'axios'
import { getOauthConfig } from 'src/constants/oauth.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from 'src/services/analytics/growthbook.js'
import {
  getIsNonInteractiveSession,
  getKairosActive,
  preferThirdPartyAuthentication,
} from '../bootstrap/state.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../services/analytics/index.js'
import {
  getAcosmiApiKey,
  getAcosmiOAuthTokens,
  handleOAuth401Error,
  hasProfileScope,
} from './auth.js'
import { isInBundledMode } from './bundledMode.js'
import { getGlobalConfig, saveGlobalConfig } from './config.js'
import { logForDebugging } from './debug.js'
import { isEnvTruthy } from './envUtils.js'
import {
  getDefaultMainLoopModelSetting,
  isMaxEffort1MMergeEnabled,
  type ModelSetting,
  parseUserSpecifiedModel,
} from './model/model.js'
import { getAPIProvider, isOfficialProvider } from './model/providers.js'
import { isEssentialTrafficOnly } from './privacyLevel.js'
import {
  getInitialSettings,
  getSettingsForSource,
  updateSettingsForSource,
} from './settings/settings.js'
import { getCachedCapabilityWithDefaultFallback, getCachedModelDisplayName } from './model/modelCapabilities.js'
import { findModelByCapability } from './model/findModelByCapability.js'
import { createSignal } from './signal.js'

export function isFastModeEnabled(): boolean {
  return !isEnvTruthy(process.env.CRABCODE_DISABLE_FAST_MODE)
}

export function isFastModeAvailable(): boolean {
  if (!isFastModeEnabled()) {
    return false
  }
  return getFastModeUnavailableReason() === null
}

type AuthType = 'oauth' | 'api-key'

function getDisabledReasonMessage(
  disabledReason: FastModeDisabledReason,
  authType: AuthType,
): string {
  switch (disabledReason) {
    case 'free':
      return authType === 'oauth'
        ? 'Fast mode requires a paid subscription'
        : 'Fast mode unavailable during evaluation. Please purchase credits.'
    case 'preference':
      return 'Fast mode has been disabled by your organization'
    case 'extra_usage_disabled':
      // Only OAuth users can have extra_usage_disabled; console users don't have this concept
      return 'Fast mode requires extra usage billing · /extra-usage to enable'
    case 'network_error':
      return 'Fast mode unavailable due to network connectivity issues'
    case 'unknown':
      return 'Fast mode is currently unavailable'
  }
}

// getFastModeUnavailableReason 被 React 渲染链路高频调用 (每次组件 re-render
// 都查一次). 旧版每次进入失败分支都 logForDebugging, 在 orgStatus.disabled
// 状态下能 /tmp/tui.log 每秒数十条 'Fast mode unavailable: ...' 刷屏.
// dedup: 仅在 reason 字面发生变化时打一条日志, reason 稳定后静默.
let lastLoggedFastModeReason: string | null | undefined = undefined
function logFastModeUnavailableOnce(reason: string): void {
  if (lastLoggedFastModeReason === reason) {
    return
  }
  lastLoggedFastModeReason = reason
  logForDebugging(`Fast mode unavailable: ${reason}`)
}

export function getFastModeUnavailableReason(): string | null {
  if (!isFastModeEnabled()) {
    return 'Fast mode is not available'
  }

  const statigReason = getFeatureValue_CACHED_MAY_BE_STALE(
    'tengu_penguins_off',
    null,
  )
  // Statsig reason has priority over other reasons.
  if (statigReason !== null) {
    logFastModeUnavailableOnce(statigReason)
    return statigReason
  }

  // Previously, fast mode required the native binary (bun build). This is no
  // longer necessary, but we keep this option behind a flag just in case.
  if (
    !isInBundledMode() &&
    getFeatureValue_CACHED_MAY_BE_STALE('tengu_marble_sandcastle', false)
  ) {
    return 'Fast mode requires the native binary · Install from: https://acosmi.com/product/crabcode'
  }

  // Not available in the SDK unless explicitly opted in via --settings.
  // Assistant daemon mode is exempt — it's first-party orchestration, and
  // kairosActive is set before this check runs (main.tsx:~1626 vs ~3249).
  if (
    getIsNonInteractiveSession() &&
    preferThirdPartyAuthentication() &&
    !getKairosActive()
  ) {
    const flagFastMode = getSettingsForSource('flagSettings')?.fastMode
    if (!flagFastMode) {
      const reason = 'Fast mode is not available in the Agent SDK'
      logFastModeUnavailableOnce(reason)
      return reason
    }
  }

  // Only available for 1P (not Bedrock/Vertex/Foundry)
  if (!isOfficialProvider()) {
    const reason = 'Fast mode is not available on Bedrock, Vertex, or Foundry'
    logFastModeUnavailableOnce(reason)
    return reason
  }

  if (orgStatus.status === 'disabled') {
    if (
      orgStatus.reason === 'network_error' ||
      orgStatus.reason === 'unknown'
    ) {
      // The org check can fail behind corporate proxies that block the
      // endpoint. We add CRABCODE_SKIP_FAST_MODE_NETWORK_ERRORS=1 to
      // bypass this check in the CC binary. This is OK since we have
      // another check in the API to error out when disabled by org.
      if (isEnvTruthy(process.env.CRABCODE_SKIP_FAST_MODE_NETWORK_ERRORS)) {
        return null
      }
    }
    const authType: AuthType =
      getAcosmiOAuthTokens() !== null ? 'oauth' : 'api-key'
    const reason = getDisabledReasonMessage(orgStatus.reason, authType)
    logFastModeUnavailableOnce(reason)
    return reason
  }

  lastLoggedFastModeReason = null
  return null
}

export function getFastModeModelDisplay(): string {
  return getCachedModelDisplayName(getFastModeModel())
}

export function getFastModeModel(): string {
  const maxEffortId = findModelByCapability('supports_max_effort')?.id
  if (maxEffortId) {
    return maxEffortId + (isMaxEffort1MMergeEnabled() ? '[1m]' : '')
  }
  return parseUserSpecifiedModel(getDefaultMainLoopModelSetting())
}

export function getInitialFastModeSetting(model: ModelSetting): boolean {
  if (!isFastModeEnabled()) {
    return false
  }
  if (!isFastModeAvailable()) {
    return false
  }
  if (!isFastModeSupportedByModel(model)) {
    return false
  }
  const settings = getInitialSettings()
  // If per-session opt-in is required, fast mode starts off each session
  if (settings.fastModePerSessionOptIn) {
    return false
  }
  return settings.fastMode === true
}

export function isFastModeSupportedByModel(
  modelSetting: ModelSetting,
): boolean {
  if (!isFastModeEnabled()) {
    return false
  }
  const model = modelSetting ?? getDefaultMainLoopModelSetting()
  const parsedModel = parseUserSpecifiedModel(model)
  // SDK capabilities (with default-model fallback) is the authoritative source.
  const result = getCachedCapabilityWithDefaultFallback(parsedModel, 'supports_fast_mode')
  if (result !== undefined) return result

  // Acosmi provider: SDK caps authoritative; conservative false on miss.
  if (getAPIProvider() === 'acosmi') return false
  return false
}

// --- Fast mode runtime state ---
// Separate from user preference (settings.fastMode). This tracks the actual
// operational state: whether we're actively sending fast speed or in cooldown
// after a rate limit.

export type FastModeRuntimeState =
  | { status: 'active' }
  | { status: 'cooldown'; resetAt: number; reason: CooldownReason }

let runtimeState: FastModeRuntimeState = { status: 'active' }
let hasLoggedCooldownExpiry = false

// --- Cooldown event listeners ---
export type CooldownReason = 'rate_limit' | 'overloaded'

const cooldownTriggered =
  createSignal<[resetAt: number, reason: CooldownReason]>()
const cooldownExpired = createSignal()
export const onCooldownTriggered = cooldownTriggered.subscribe
export const onCooldownExpired = cooldownExpired.subscribe

export function getFastModeRuntimeState(): FastModeRuntimeState {
  if (
    runtimeState.status === 'cooldown' &&
    Date.now() >= runtimeState.resetAt
  ) {
    if (isFastModeEnabled() && !hasLoggedCooldownExpiry) {
      logForDebugging('Fast mode cooldown expired, re-enabling fast mode')
      hasLoggedCooldownExpiry = true
      cooldownExpired.emit()
    }
    runtimeState = { status: 'active' }
  }
  return runtimeState
}

export function triggerFastModeCooldown(
  resetTimestamp: number,
  reason: CooldownReason,
): void {
  if (!isFastModeEnabled()) {
    return
  }
  runtimeState = { status: 'cooldown', resetAt: resetTimestamp, reason }
  hasLoggedCooldownExpiry = false
  const cooldownDurationMs = resetTimestamp - Date.now()
  logForDebugging(
    `Fast mode cooldown triggered (${reason}), duration ${Math.round(cooldownDurationMs / 1000)}s`,
  )
  logEvent('tengu_fast_mode_fallback_triggered', {
    cooldown_duration_ms: cooldownDurationMs,
    cooldown_reason:
      reason as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })
  cooldownTriggered.emit(resetTimestamp, reason)
}

export function clearFastModeCooldown(): void {
  runtimeState = { status: 'active' }
}

/**
 * Called when the API rejects a fast mode request (e.g., 400 "Fast mode is
 * not enabled for your organization"). Permanently disables fast mode using
 * the same flow as when the prefetch discovers the org has it disabled.
 */
export function handleFastModeRejectedByAPI(): void {
  if (orgStatus.status === 'disabled') {
    return
  }
  orgStatus = { status: 'disabled', reason: 'preference' }
  updateSettingsForSource('userSettings', { fastMode: undefined })
  saveGlobalConfig(current => ({
    ...current,
    penguinModeOrgEnabled: false,
  }))
  orgFastModeChange.emit(false)
}


export function isFastModeCooldown(): boolean {
  return getFastModeRuntimeState().status === 'cooldown'
}

export function getFastModeState(
  model: ModelSetting,
  fastModeUserEnabled: boolean | undefined,
): 'off' | 'cooldown' | 'on' {
  const enabled =
    isFastModeEnabled() &&
    isFastModeAvailable() &&
    !!fastModeUserEnabled &&
    isFastModeSupportedByModel(model)
  if (enabled && isFastModeCooldown()) {
    return 'cooldown'
  }
  if (enabled) {
    return 'on'
  }
  return 'off'
}

// Disabled reason returned by the API. The API is the canonical source for why
// fast mode is disabled (free account, admin preference, extra usage not enabled).
export type FastModeDisabledReason =
  | 'free'
  | 'preference'
  | 'extra_usage_disabled'
  | 'network_error'
  | 'unknown'

// In-memory cache of the fast mode status from the API.
// Distinct from the user's fastMode app state — this represents
// whether the org *allows* fast mode and why it may be disabled.
// Modeled as a discriminated union so the invalid state
// (disabled without a reason) is unrepresentable.
type FastModeOrgStatus =
  | { status: 'pending' }
  | { status: 'enabled' }
  | { status: 'disabled'; reason: FastModeDisabledReason }

let orgStatus: FastModeOrgStatus = { status: 'pending' }

// Listeners notified when org-level fast mode status changes
const orgFastModeChange = createSignal<[orgEnabled: boolean]>()
export const onOrgFastModeChanged = orgFastModeChange.subscribe

type FastModeResponse = {
  enabled: boolean
  disabled_reason: FastModeDisabledReason | null
}

async function fetchFastModeStatus(
  auth: { accessToken: string } | { apiKey: string },
): Promise<FastModeResponse> {
  const endpoint = `${getOauthConfig().BASE_API_URL}/api/crab_code_penguin_mode`
  const headers: Record<string, string> =
    'accessToken' in auth
      ? {
          Authorization: `Bearer ${auth.accessToken}`,
        }
      : { 'x-api-key': auth.apiKey }

  // timeout matches bootstrap.ts (its sibling startup prefetch hitting the
  // same gateway). Without it, axios.defaults.timeout is 0 (unbounded) and a
  // slow/hung proxy makes this best-effort probe block indefinitely, turning
  // the cold-start "~0s fast-fail" into a multi-tens-of-seconds hang. 5s caps
  // the wait; the doFetch caller already handles the throw (cached / network_error).
  const response = await axios.get<FastModeResponse>(endpoint, {
    headers,
    timeout: 5000,
  })
  return response.data
}

const PREFETCH_MIN_INTERVAL_MS = 30_000
let lastPrefetchAt = 0
let inflightPrefetch: Promise<void> | null = null

/**
 * Resolve orgStatus from the persisted cache without making any API calls.
 * Used when startup prefetches are throttled to avoid hitting the network
 * while still making fast mode availability checks work.
 */
export function resolveFastModeStatusFromCache(): void {
  if (!isFastModeEnabled()) {
    return
  }
  if (orgStatus.status !== 'pending') {
    return
  }
  const isAnt = process.env.USER_TYPE === 'ant'
  const cachedEnabled = getGlobalConfig().penguinModeOrgEnabled === true
  orgStatus =
    isAnt || cachedEnabled
      ? { status: 'enabled' }
      : { status: 'disabled', reason: 'unknown' }
}

export async function prefetchFastModeStatus(): Promise<void> {
  // Skip network requests if nonessential traffic is disabled
  if (isEssentialTrafficOnly()) {
    return
  }

  if (!isFastModeEnabled()) {
    return
  }

  if (inflightPrefetch) {
    logForDebugging(
      'Fast mode prefetch in progress, returning in-flight promise',
    )
    return inflightPrefetch
  }

  // Service key OAuth sessions lack user:profile scope → endpoint 403s.
  // Resolve orgStatus from cache and bail before burning the throttle window.
  // API key auth is unaffected.
  const apiKey = getAcosmiApiKey()
  const hasUsableOAuth =
    getAcosmiOAuthTokens()?.accessToken && hasProfileScope()
  if (!hasUsableOAuth && !apiKey) {
    const isAnt = process.env.USER_TYPE === 'ant'
    const cachedEnabled = getGlobalConfig().penguinModeOrgEnabled === true
    orgStatus =
      isAnt || cachedEnabled
        ? { status: 'enabled' }
        : { status: 'disabled', reason: 'preference' }
    return
  }

  const now = Date.now()
  if (now - lastPrefetchAt < PREFETCH_MIN_INTERVAL_MS) {
    logForDebugging('Skipping fast mode prefetch, fetched recently')
    return
  }
  lastPrefetchAt = now

  const fetchWithCurrentAuth = async (): Promise<FastModeResponse> => {
    const currentTokens = getAcosmiOAuthTokens()
    const auth =
      currentTokens?.accessToken && hasProfileScope()
        ? { accessToken: currentTokens.accessToken }
        : apiKey
          ? { apiKey }
          : null
    if (!auth) {
      throw new Error('No auth available')
    }
    return fetchFastModeStatus(auth)
  }

  async function doFetch(): Promise<void> {
    try {
      let status: FastModeResponse
      try {
        status = await fetchWithCurrentAuth()
      } catch (err) {
        const isAuthError =
          axios.isAxiosError(err) &&
          (err.response?.status === 401 ||
            (err.response?.status === 403 &&
              typeof err.response?.data === 'string' &&
              err.response.data.includes('OAuth token has been revoked')))
        if (isAuthError) {
          const failedAccessToken = getAcosmiOAuthTokens()?.accessToken
          if (failedAccessToken) {
            await handleOAuth401Error(failedAccessToken)
            status = await fetchWithCurrentAuth()
          } else {
            throw err
          }
        } else {
          throw err
        }
      }

      const previousEnabled =
        orgStatus.status !== 'pending'
          ? orgStatus.status === 'enabled'
          : getGlobalConfig().penguinModeOrgEnabled
      orgStatus = status.enabled
        ? { status: 'enabled' }
        : {
            status: 'disabled',
            reason: status.disabled_reason ?? 'preference',
          }
      if (previousEnabled !== status.enabled) {
        // When org disables fast mode, permanently turn off the user's fast mode setting
        if (!status.enabled) {
          updateSettingsForSource('userSettings', { fastMode: undefined })
        }
        saveGlobalConfig(current => ({
          ...current,
          penguinModeOrgEnabled: status.enabled,
        }))
        orgFastModeChange.emit(status.enabled)
      }
      logForDebugging(
        `Org fast mode: ${status.enabled ? 'enabled' : `disabled (${status.disabled_reason ?? 'preference'})`}`,
      )
    } catch (err) {
      // On failure: ants default to enabled (don't block internal users).
      // External users: fall back to the cached penguinModeOrgEnabled value;
      // if no positive cache, disable with network_error reason.
      const isAnt = process.env.USER_TYPE === 'ant'
      const cachedEnabled = getGlobalConfig().penguinModeOrgEnabled === true
      orgStatus =
        isAnt || cachedEnabled
          ? { status: 'enabled' }
          : { status: 'disabled', reason: 'network_error' }
      logForDebugging(
        `Failed to fetch org fast mode status, defaulting to ${orgStatus.status === 'enabled' ? 'enabled (cached)' : 'disabled (network_error)'}: ${err}`,
        { level: 'error' },
      )
      logEvent('tengu_org_penguin_mode_fetch_failed', {})
    } finally {
      inflightPrefetch = null
    }
  }

  inflightPrefetch = doFetch()
  return inflightPrefetch
}
