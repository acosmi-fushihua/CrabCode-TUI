import { parseAccountBridgeReference } from '../../utils/model/accountBridgeReference.js'

export const CRABCODE_THINKING_MODES = [
  'auto',
  'off',
  'standard',
  'deep',
] as const

export type CrabCodeThinkingMode = (typeof CRABCODE_THINKING_MODES)[number]

export type CrabCodeThinkingModeResolution =
  | {
      ok: true
      requested: CrabCodeThinkingMode
      mode: CrabCodeThinkingMode
      fellBack: boolean
    }
  | {
      ok: false
      requested: CrabCodeThinkingMode
      supportedModes: readonly CrabCodeThinkingMode[]
    }

/**
 * Resolve a requested mode using the stable Account Bridge fallback contract.
 * Kept beside the wire-safe types to avoid pulling the root runtime settings
 * and analytics graph into protocol-only consumers.
 */
export function resolveSupportedCrabCodeThinkingMode(
  requested: CrabCodeThinkingMode,
  supportedModes: readonly CrabCodeThinkingMode[],
): CrabCodeThinkingModeResolution {
  if (supportedModes.includes(requested)) {
    return { ok: true, requested, mode: requested, fellBack: false }
  }

  const fallbackOrder: readonly CrabCodeThinkingMode[] =
    requested === 'deep'
      ? ['standard', 'auto']
      : requested === 'standard' || requested === 'off'
        ? ['auto']
        : []
  const mode = fallbackOrder.find(candidate => supportedModes.includes(candidate))
  return mode === undefined
    ? { ok: false, requested, supportedModes }
    : { ok: true, requested, mode, fellBack: true }
}

export type AccountBridgeEligibilityState =
  | 'checking'
  | 'allowed'
  | 'blocked-cn'
  | 'unavailable'

export type AccountBridgeRuntimeState =
  | 'stopped'
  | 'starting'
  | 'ready'
  | 'stopping'
  | 'failed'
  | 'blocked'

export interface AccountBridgeEligibilityReadParams {
  forceRefresh: boolean
}

export interface AccountBridgeEligibilityView {
  state: AccountBridgeEligibilityState
  countryCode: string | null
  policyVersion: string | null
  checkedAt: string | null
  expiresAt: string | null
  reasonCode: string | null
}

export interface AccountBridgeConnectorView {
  connectorId: string
  displayName: string
  authMode: 'browser' | 'device-code'
  enabled: boolean
  disabledReasonCode: string | null
  termsStatus: 'signed-off' | 'blocked'
}

export interface AccountBridgeRuntimeView {
  state: AccountBridgeRuntimeState
  componentVersion: string | null
  protocolVersion: number | null
  lastErrorCode: string | null
}

export interface AccountBridgeAccountView {
  accountId: string
  connectorId: string
  displayLabel: string
  status:
    | 'ready'
    | 'reauthorization-required'
    | 'cooldown'
    | 'quota-exhausted'
    | 'disabled'
  connectedAt: string
  lastUsedAt: string | null
  cooldownUntil: string | null
}

export interface AccountBridgeModelCapability {
  chatRuntimeSupported: boolean | null
  supportsTools: boolean | null
  supportsThinking: boolean | null
  supportsAdaptiveThinking: boolean | null
  supportsEffort: boolean | null
  supportsMaxEffort: boolean | null
  supportsVision: boolean | null
  supportsJsonMode: boolean | null
  supportedThinkingModes: CrabCodeThinkingMode[]
  defaultThinkingMode: CrabCodeThinkingMode | null
  contextWindow: number | null
  maxOutputTokens: number | null
}

export interface AccountBridgeModelRouteView
  extends AccountBridgeModelCapability {
  routeId: string
  accountId: string
  connectorId: string
  modelId: string
  displayName: string | null
  connectorLabel: string
  accountLabel: string
}

export interface AccountBridgeUsageWindow {
  label: string
  limit: number | null
  used: number | null
  remainingPercent: number | null
  resetsAt: string | null
}

export interface AccountBridgeUsageSnapshot {
  routeId: string
  accountId: string
  state: 'available' | 'cooldown' | 'exhausted' | 'unknown'
  remainingPercent: number | null
  limitingWindowLabel: string | null
  resetsAt: string | null
  windows: AccountBridgeUsageWindow[]
  observedAt: string
}

export interface AccountBridgeModel extends AccountBridgeModelRouteView {
  usage: AccountBridgeUsageSnapshot | null
}

export interface AccountBridgeLoginStartView {
  sessionId: string
  authMode: 'browser' | 'device-code'
  authorizationUrl: string | null
  userCode: string | null
  verificationUrl: string | null
  expiresAt: string | null
}

export interface AccountBridgeLoginStartParams {
  connectorId: string
}

export interface AccountBridgeLoginPollView {
  /**
   * The sidecar instance that owned this session stopped or was replaced.
   * This is terminal: callers must start a new login instead of polling the
   * replacement process with an identifier it never minted.
   */
  state:
    | 'pending'
    | 'succeeded'
    | 'failed'
    | 'cancelled'
    | 'expired'
    | 'session-lost'
  accountId: string | null
  errorCode: string | null
}

export interface AccountBridgeLoginSessionParams {
  sessionId: string
}

export interface AccountBridgeAccountRemoveParams {
  accountId: string
}

export interface AccountBridgeUsageReadParams {
  routeIds: string[] | null
  forceRefresh: boolean
}

export interface AccountBridgeLoginCancelView {
  cancelled: boolean
}

export interface AccountBridgeAccountRemoveView {
  removed: boolean
}

export type ModelAccessEntryKind =
  | 'managed'
  | 'custom-local'
  | 'account'
  | 'catalog'

export interface ModelAccessEntryView {
  kind: ModelAccessEntryKind
  label: string
  description: string
  badge: string | null
  disabledReasonCode: string | null
  action:
    | 'select-managed-model'
    | 'open-custom-local-model'
    | 'open-account-bridge'
    | 'recheck-account-bridge'
    | 'open-model-catalog'
    | null
}

export type ModelAccessCompletion =
  | { kind: 'account'; flowId: string; routeId: string; modelReference: string }
  | {
      kind: 'custom'
      flowId: string
      customModelId: string
      modelReference: string
    }
  | { kind: 'local'; flowId: string; localModelId: string; modelReference: string }
  | { kind: 'cancelled'; flowId: string }

/**
 * Runtime-private per-turn capability. Only a trusted process-local manager
 * may construct it for the worker or direct QueryEngine transport.
 */
export interface AccountBridgeRuntimeAccess {
  endpoint: string
  route: AccountBridgeModelRouteView
  /** Runtime-private bearer credential; never part of a public view schema. */
  inferenceKey: string
}

const FORBIDDEN_WIRE_KEYS = new Set([
  'signedGrant',
  'managementKey',
  'management_key',
  'inferenceKey',
  'inference_key',
  'apiKey',
  'api_key',
  'accessToken',
  'access_token',
  'refreshToken',
  'refresh_token',
  'oauthToken',
  'oauth_token',
  'rawIdentity',
  'raw_identity',
])

function record(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError(`${label} must be an object`)
  }
  return value as Record<string, unknown>
}

function assertNoForbiddenKeys(value: unknown, label: string): void {
  if (!value || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) {
    if (FORBIDDEN_WIRE_KEYS.has(key)) {
      throw new TypeError(`${label} contains forbidden field ${key}`)
    }
    assertNoForbiddenKeys(child, `${label}.${key}`)
  }
}

function stringField(value: unknown, label: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw new TypeError(`${label} must be a non-empty string`)
  }
  return value
}

function nullableString(value: unknown, label: string): string | null {
  if (value === undefined || value === null) return null
  if (typeof value !== 'string') throw new TypeError(`${label} must be string|null`)
  return value
}

function booleanField(value: unknown, label: string): boolean {
  if (typeof value !== 'boolean') throw new TypeError(`${label} must be boolean`)
  return value
}

function nullableBoolean(value: unknown, label: string): boolean | null {
  if (value === undefined || value === null) return null
  if (typeof value !== 'boolean') throw new TypeError(`${label} must be boolean|null`)
  return value
}

function nullableNumber(value: unknown, label: string): number | null {
  if (value === undefined || value === null) return null
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new TypeError(`${label} must be a finite number|null`)
  }
  return value
}

function nullableNonNegativeNumber(value: unknown, label: string): number | null {
  const parsed = nullableNumber(value, label)
  if (parsed !== null && parsed < 0) {
    throw new TypeError(`${label} must be non-negative|null`)
  }
  return parsed
}

function nullablePositiveInteger(value: unknown, label: string): number | null {
  const parsed = nullableNumber(value, label)
  if (parsed !== null && (!Number.isSafeInteger(parsed) || parsed <= 0)) {
    throw new TypeError(`${label} must be a positive safe integer|null`)
  }
  return parsed
}

function enumField<T extends string>(
  value: unknown,
  allowed: readonly T[],
  label: string,
): T {
  if (typeof value !== 'string' || !allowed.includes(value as T)) {
    throw new TypeError(`${label} has an unsupported value`)
  }
  return value as T
}

function routeIdField(value: unknown, label: string): string {
  const routeId = stringField(value, label)
  if (!parseAccountBridgeReference(`account:${routeId}`)) {
    throw new TypeError(`${label} is not a valid opaque route id`)
  }
  return routeId
}

export function isCrabCodeThinkingMode(
  value: unknown,
): value is CrabCodeThinkingMode {
  return (
    typeof value === 'string' &&
    CRABCODE_THINKING_MODES.includes(value as CrabCodeThinkingMode)
  )
}

export function parseCrabCodeThinkingMode(
  value: unknown,
  label = 'crabcodeThinkingMode',
): CrabCodeThinkingMode {
  return enumField(value, CRABCODE_THINKING_MODES, label)
}

export function parseAccountBridgeEligibilityView(
  value: unknown,
): AccountBridgeEligibilityView {
  assertNoForbiddenKeys(value, 'eligibility')
  const input = record(value, 'eligibility')
  return {
    state: enumField(
      input.state,
      ['checking', 'allowed', 'blocked-cn', 'unavailable'],
      'eligibility.state',
    ),
    countryCode: nullableString(input.countryCode, 'eligibility.countryCode'),
    policyVersion: nullableString(input.policyVersion, 'eligibility.policyVersion'),
    checkedAt: nullableString(input.checkedAt, 'eligibility.checkedAt'),
    expiresAt: nullableString(input.expiresAt, 'eligibility.expiresAt'),
    reasonCode: nullableString(input.reasonCode, 'eligibility.reasonCode'),
  }
}

export function parseAccountBridgeEligibilityReadParams(
  value: unknown,
): AccountBridgeEligibilityReadParams {
  const input = record(value, 'eligibilityReadParams')
  return {
    forceRefresh: booleanField(
      input.forceRefresh,
      'eligibilityReadParams.forceRefresh',
    ),
  }
}

export function parseAccountBridgeLoginStartParams(
  value: unknown,
): AccountBridgeLoginStartParams {
  const input = record(value, 'loginStartParams')
  return {
    connectorId: stringField(input.connectorId, 'loginStartParams.connectorId'),
  }
}

export function parseAccountBridgeLoginSessionParams(
  value: unknown,
): AccountBridgeLoginSessionParams {
  const input = record(value, 'loginSessionParams')
  return {
    sessionId: stringField(input.sessionId, 'loginSessionParams.sessionId'),
  }
}

export function parseAccountBridgeAccountRemoveParams(
  value: unknown,
): AccountBridgeAccountRemoveParams {
  const input = record(value, 'accountRemoveParams')
  return {
    accountId: stringField(input.accountId, 'accountRemoveParams.accountId'),
  }
}

export function parseAccountBridgeUsageReadParams(
  value: unknown,
): AccountBridgeUsageReadParams {
  const input = record(value, 'usageReadParams')
  if (input.routeIds !== null && !Array.isArray(input.routeIds)) {
    throw new TypeError('usageReadParams.routeIds must be string[]|null')
  }
  return {
    routeIds:
      input.routeIds === null
        ? null
        : input.routeIds.map((routeId, index) =>
            routeIdField(routeId, `usageReadParams.routeIds[${index}]`),
          ),
    forceRefresh: booleanField(
      input.forceRefresh,
      'usageReadParams.forceRefresh',
    ),
  }
}

export function parseAccountBridgeConnectorView(
  value: unknown,
): AccountBridgeConnectorView {
  assertNoForbiddenKeys(value, 'connector')
  const input = record(value, 'connector')
  return {
    connectorId: stringField(input.connectorId, 'connector.connectorId'),
    displayName: stringField(input.displayName, 'connector.displayName'),
    authMode: enumField(input.authMode, ['browser', 'device-code'], 'connector.authMode'),
    enabled: booleanField(input.enabled, 'connector.enabled'),
    disabledReasonCode: nullableString(
      input.disabledReasonCode,
      'connector.disabledReasonCode',
    ),
    termsStatus: enumField(
      input.termsStatus,
      ['signed-off', 'blocked'],
      'connector.termsStatus',
    ),
  }
}

export function parseAccountBridgeRuntimeView(
  value: unknown,
): AccountBridgeRuntimeView {
  assertNoForbiddenKeys(value, 'runtime')
  const input = record(value, 'runtime')
  return {
    state: enumField(
      input.state,
      ['stopped', 'starting', 'ready', 'stopping', 'failed', 'blocked'],
      'runtime.state',
    ),
    componentVersion: nullableString(input.componentVersion, 'runtime.componentVersion'),
    protocolVersion: nullablePositiveInteger(
      input.protocolVersion,
      'runtime.protocolVersion',
    ),
    lastErrorCode: nullableString(input.lastErrorCode, 'runtime.lastErrorCode'),
  }
}

export function parseAccountBridgeLoginStartView(
  value: unknown,
): AccountBridgeLoginStartView {
  assertNoForbiddenKeys(value, 'loginStart')
  const input = record(value, 'loginStart')
  return {
    sessionId: stringField(input.sessionId, 'loginStart.sessionId'),
    authMode: enumField(input.authMode, ['browser', 'device-code'], 'loginStart.authMode'),
    authorizationUrl: nullableString(
      input.authorizationUrl,
      'loginStart.authorizationUrl',
    ),
    userCode: nullableString(input.userCode, 'loginStart.userCode'),
    verificationUrl: nullableString(
      input.verificationUrl,
      'loginStart.verificationUrl',
    ),
    expiresAt: nullableString(input.expiresAt, 'loginStart.expiresAt'),
  }
}

export function parseAccountBridgeLoginPollView(
  value: unknown,
): AccountBridgeLoginPollView {
  assertNoForbiddenKeys(value, 'loginPoll')
  const input = record(value, 'loginPoll')
  return {
    state: enumField(
      input.state,
      ['pending', 'succeeded', 'failed', 'cancelled', 'expired', 'session-lost'],
      'loginPoll.state',
    ),
    accountId: nullableString(input.accountId, 'loginPoll.accountId'),
    errorCode: nullableString(input.errorCode, 'loginPoll.errorCode'),
  }
}

export function parseAccountBridgeLoginCancelView(
  value: unknown,
): AccountBridgeLoginCancelView {
  const input = record(value, 'loginCancel')
  return { cancelled: booleanField(input.cancelled, 'loginCancel.cancelled') }
}

export function parseAccountBridgeAccountRemoveView(
  value: unknown,
): AccountBridgeAccountRemoveView {
  const input = record(value, 'accountRemove')
  return { removed: booleanField(input.removed, 'accountRemove.removed') }
}

export function parseAccountBridgeAccountView(
  value: unknown,
): AccountBridgeAccountView {
  assertNoForbiddenKeys(value, 'account')
  const input = record(value, 'account')
  return {
    accountId: stringField(input.accountId, 'account.accountId'),
    connectorId: stringField(input.connectorId, 'account.connectorId'),
    displayLabel: stringField(input.displayLabel, 'account.displayLabel'),
    status: enumField(
      input.status,
      ['ready', 'reauthorization-required', 'cooldown', 'quota-exhausted', 'disabled'],
      'account.status',
    ),
    connectedAt: stringField(input.connectedAt, 'account.connectedAt'),
    lastUsedAt: nullableString(input.lastUsedAt, 'account.lastUsedAt'),
    cooldownUntil: nullableString(input.cooldownUntil, 'account.cooldownUntil'),
  }
}

export function parseAccountBridgeModelRouteView(
  value: unknown,
): AccountBridgeModelRouteView {
  assertNoForbiddenKeys(value, 'route')
  const input = record(value, 'route')
  const rawModes = input.supportedThinkingModes
  if (!Array.isArray(rawModes)) {
    throw new TypeError('route.supportedThinkingModes must be an array')
  }
  const supportedThinkingModes = rawModes.map((mode, index) =>
    parseCrabCodeThinkingMode(mode, `route.supportedThinkingModes[${index}]`),
  )
  if (new Set(supportedThinkingModes).size !== supportedThinkingModes.length) {
    throw new TypeError('route.supportedThinkingModes must not contain duplicates')
  }
  const defaultThinkingMode =
    input.defaultThinkingMode === undefined || input.defaultThinkingMode === null
      ? null
      : parseCrabCodeThinkingMode(input.defaultThinkingMode, 'route.defaultThinkingMode')
  if (
    defaultThinkingMode !== null &&
    !supportedThinkingModes.includes(defaultThinkingMode)
  ) {
    throw new TypeError('route.defaultThinkingMode must be supported by the route')
  }
  return {
    routeId: routeIdField(input.routeId, 'route.routeId'),
    accountId: stringField(input.accountId, 'route.accountId'),
    connectorId: stringField(input.connectorId, 'route.connectorId'),
    modelId: stringField(input.modelId, 'route.modelId'),
    displayName: nullableString(input.displayName, 'route.displayName'),
    connectorLabel: stringField(input.connectorLabel, 'route.connectorLabel'),
    accountLabel: stringField(input.accountLabel, 'route.accountLabel'),
    chatRuntimeSupported: nullableBoolean(
      input.chatRuntimeSupported,
      'route.chatRuntimeSupported',
    ),
    supportsTools: nullableBoolean(input.supportsTools, 'route.supportsTools'),
    supportsThinking: nullableBoolean(
      input.supportsThinking,
      'route.supportsThinking',
    ),
    supportsAdaptiveThinking: nullableBoolean(
      input.supportsAdaptiveThinking,
      'route.supportsAdaptiveThinking',
    ),
    supportsEffort: nullableBoolean(input.supportsEffort, 'route.supportsEffort'),
    supportsMaxEffort: nullableBoolean(
      input.supportsMaxEffort,
      'route.supportsMaxEffort',
    ),
    supportsVision: nullableBoolean(input.supportsVision, 'route.supportsVision'),
    supportsJsonMode: nullableBoolean(
      input.supportsJsonMode,
      'route.supportsJsonMode',
    ),
    supportedThinkingModes,
    defaultThinkingMode,
    contextWindow: nullablePositiveInteger(input.contextWindow, 'route.contextWindow'),
    maxOutputTokens: nullablePositiveInteger(
      input.maxOutputTokens,
      'route.maxOutputTokens',
    ),
  }
}

function clampPercent(value: number): number {
  return Math.min(100, Math.max(0, value))
}

export function parseAccountBridgeUsageWindow(
  value: unknown,
): AccountBridgeUsageWindow {
  assertNoForbiddenKeys(value, 'usageWindow')
  const input = record(value, 'usageWindow')
  const limit = nullableNonNegativeNumber(input.limit, 'usageWindow.limit')
  const used = nullableNonNegativeNumber(input.used, 'usageWindow.used')
  const directPercent = nullableNumber(
    input.remainingPercent,
    'usageWindow.remainingPercent',
  )
  const remainingPercent =
    directPercent !== null
      ? clampPercent(directPercent)
      : limit !== null && used !== null && limit > 0
        ? Math.floor(clampPercent(((limit - used) / limit) * 100))
        : null
  return {
    label: stringField(input.label, 'usageWindow.label'),
    limit,
    used,
    remainingPercent,
    resetsAt: nullableString(input.resetsAt, 'usageWindow.resetsAt'),
  }
}

export function parseAccountBridgeUsageSnapshot(
  value: unknown,
): AccountBridgeUsageSnapshot {
  assertNoForbiddenKeys(value, 'usageSnapshot')
  const input = record(value, 'usageSnapshot')
  if (!Array.isArray(input.windows)) {
    throw new TypeError('usageSnapshot.windows must be an array')
  }
  const windows = input.windows.map(parseAccountBridgeUsageWindow)
  const structured = windows.filter(
    (window): window is AccountBridgeUsageWindow & { remainingPercent: number } =>
      window.remainingPercent !== null,
  )
  const limiting = structured.reduce<
    (AccountBridgeUsageWindow & { remainingPercent: number }) | null
  >(
    (lowest, window) =>
      lowest === null || window.remainingPercent < lowest.remainingPercent
        ? window
        : lowest,
    null,
  )
  const directPercent = nullableNumber(
    input.remainingPercent,
    'usageSnapshot.remainingPercent',
  )
  return {
    routeId: routeIdField(input.routeId, 'usageSnapshot.routeId'),
    accountId: stringField(input.accountId, 'usageSnapshot.accountId'),
    state: enumField(
      input.state,
      ['available', 'cooldown', 'exhausted', 'unknown'],
      'usageSnapshot.state',
    ),
    remainingPercent:
      limiting?.remainingPercent ??
      (directPercent === null ? null : clampPercent(directPercent)),
    limitingWindowLabel:
      limiting?.label ??
      nullableString(input.limitingWindowLabel, 'usageSnapshot.limitingWindowLabel'),
    resetsAt:
      limiting?.resetsAt ?? nullableString(input.resetsAt, 'usageSnapshot.resetsAt'),
    windows,
    observedAt: stringField(input.observedAt, 'usageSnapshot.observedAt'),
  }
}

export function parseAccountBridgeRuntimeAccess(
  value: unknown,
): AccountBridgeRuntimeAccess {
  const input = record(value, 'accountBridgeRuntimeAccess')
  for (const key of Object.keys(input)) {
    if (!['endpoint', 'route', 'inferenceKey'].includes(key)) {
      throw new TypeError('accountBridgeRuntimeAccess contains an unsupported field')
    }
  }
  const endpoint = stringField(input.endpoint, 'accountBridgeRuntimeAccess.endpoint')
  if (
    !/^http:\/\/(?:127\.0\.0\.1|\[::1\]):(?:[1-9]\d{0,4})\/v1\/messages$/.test(
      endpoint,
    )
  ) {
    throw new TypeError(
      'accountBridgeRuntimeAccess.endpoint must be an exact loopback http /v1/messages URL',
    )
  }
  const url = new URL(endpoint)
  if (
    url.protocol !== 'http:' ||
    !['127.0.0.1', '[::1]'].includes(url.hostname) ||
    url.port === '' ||
    url.pathname !== '/v1/messages' ||
    url.username !== '' ||
    url.password !== '' ||
    url.search !== '' ||
    url.hash !== ''
  ) {
    throw new TypeError(
      'accountBridgeRuntimeAccess.endpoint must be an exact loopback http /v1/messages URL',
    )
  }
  const inferenceKey = stringField(
    input.inferenceKey,
    'accountBridgeRuntimeAccess.inferenceKey',
  )
  if (!/^[A-Za-z0-9_-]{43}$/.test(inferenceKey)) {
    throw new TypeError(
      'accountBridgeRuntimeAccess.inferenceKey must be an opaque 32-byte base64url value',
    )
  }
  return {
    endpoint: url.toString(),
    route: parseAccountBridgeModelRouteView(input.route),
    inferenceKey,
  }
}

export function parseAccountBridgeAccountList(value: unknown): AccountBridgeAccountView[] {
  assertNoForbiddenKeys(value, 'accountList')
  const input = record(value, 'accountList')
  if (!Array.isArray(input.accounts)) throw new TypeError('accountList.accounts must be an array')
  return input.accounts.map(parseAccountBridgeAccountView)
}

export function parseAccountBridgeConnectorList(
  value: unknown,
): AccountBridgeConnectorView[] {
  assertNoForbiddenKeys(value, 'connectorList')
  const input = record(value, 'connectorList')
  if (!Array.isArray(input.connectors)) {
    throw new TypeError('connectorList.connectors must be an array')
  }
  return input.connectors.map(parseAccountBridgeConnectorView)
}

export function parseAccountBridgeModelList(value: unknown): AccountBridgeModelRouteView[] {
  assertNoForbiddenKeys(value, 'modelList')
  const input = record(value, 'modelList')
  if (!Array.isArray(input.routes)) throw new TypeError('modelList.routes must be an array')
  return input.routes.map(parseAccountBridgeModelRouteView)
}

export function parseAccountBridgeUsageRead(value: unknown): AccountBridgeUsageSnapshot[] {
  assertNoForbiddenKeys(value, 'usageRead')
  const input = record(value, 'usageRead')
  if (!Array.isArray(input.snapshots)) {
    throw new TypeError('usageRead.snapshots must be an array')
  }
  return input.snapshots.map(parseAccountBridgeUsageSnapshot)
}

// ── eligibility reasonCode groups ──
// Keep region and terminal refusal classification beside the wire-safe types
// so all TUI call sites share one exhaustive definition.

/**
 * 地区判定码：描述的是**结论**（出口在哪 / 该地区开不开放），不是一次失败。
 * TUI 为它们准备了专属视图与文案，通用失败话术不得接管。
 */
export const ACCOUNT_BRIDGE_REGION_VERDICT_CODES = [
  'blocked-cn',
  'region-not-allowed',
  'unknown-region',
] as const

/**
 * 终态拒绝码：控制面对**这个账号 / 这个构建**给出了确定结论，且各有各的出路
 * （补会员 / 升客户端 / 换官方包 / 去登录）。共同性质是：同输入同会话重试必然
 * 同败。这个集合也是 TUI 状态徽标的分派判据：命中即**不是**「地区待确认」，
 * 因为地区判定已经成功，被拒的是其他条件。
 */
export const ACCOUNT_BRIDGE_TERMINAL_REFUSAL_CODES = [
  'eligibility-membership-required',
  'eligibility-upgrade-required',
  'eligibility-client-binding-rejected',
  'eligibility-endpoint-unavailable',
  'eligibility-auth-denied',
] as const

const REGION_VERDICT_SET: ReadonlySet<string> = new Set(
  ACCOUNT_BRIDGE_REGION_VERDICT_CODES,
)
const TERMINAL_REFUSAL_SET: ReadonlySet<string> = new Set(
  ACCOUNT_BRIDGE_TERMINAL_REFUSAL_CODES,
)

/** 该码是否为地区判定结论（而非一次失败）。 */
export function isAccountBridgeRegionVerdict(
  code: string | null | undefined,
): boolean {
  return code != null && REGION_VERDICT_SET.has(code)
}

/** 该码是否为确定性终态拒绝（重试永远无效，且与地区无关）。 */
export function isAccountBridgeTerminalRefusal(
  code: string | null | undefined,
): boolean {
  return code != null && TERMINAL_REFUSAL_SET.has(code)
}
