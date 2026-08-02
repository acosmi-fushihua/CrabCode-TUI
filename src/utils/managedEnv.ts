import { isRemoteManagedSettingsEligible } from '../services/remoteManagedSettings/syncCache.js'
import mergeWith from 'lodash-es/mergeWith.js'
import { clearCACertsCache } from './caCerts.js'
import { getGlobalConfig } from './config.js'
import { isEnvTruthy } from './envUtils.js'
import {
  classifyManagedEnvVar,
  isProviderManagedEnvVar,
  isTrustedManagedEnvSource,
  type ManagedEnvPhase,
  type ManagedEnvSource,
} from './managedEnvConstants.js'
import { clearMTLSCache } from './mtls.js'
import { clearProxyCache, configureGlobalAgents } from './proxy.js'
import { getMembershipGateInput } from './auth.js'
import { shouldBridgeCustomModelSettings } from './entitlements/customModels.js'
import { readCustomModelApiKeySync } from './model/customModelSecrets.js'
import {
  isSettingSourceEnabled,
  SETTING_SOURCES,
} from './settings/constants.js'
import { getSettingsForSource } from './settings/settings.js'
import type { SettingsJson } from './settings/types.js'

export type ManagedEnvLayer = {
  source: ManagedEnvSource
  env: Record<string, string> | undefined
}

export type ManagedStructuredSettingsLayer = {
  source: ManagedEnvSource
  settings: SettingsJson | null | undefined
}

type StructuredBridgeEnvTarget = Record<string, string | undefined>

// Tracks only values injected by the structured-settings bridge. This lets a
// later singular → registry migration retract its own CRABCODE_CUSTOM_* values
// without deleting an explicit shell/host/settings.env value that happens to
// use the same key.
const structuredBridgeOwnedEnv = new Map<string, string>()
const BOOTSTRAP_CONFIG_ENV = 'CRABCODE_BOOTSTRAP_CONFIG'
const STRUCTURED_CUSTOM_BRIDGE_KEYS_FIELD =
  '_internalStructuredCustomBridgeKeys'
const LEGACY_STRUCTURED_CUSTOM_BRIDGE_KEYS_ENV =
  'CRABCODE_INTERNAL_STRUCTURED_CUSTOM_BRIDGE_KEYS'
const STRUCTURED_CUSTOM_BRIDGE_KEYS = new Set([
  'CRABCODE_CUSTOM_BASE_URL',
  'CRABCODE_CUSTOM_API_KEY',
])
const SETTINGS_ENV_ALWAYS_DENIED = new Set([
  BOOTSTRAP_CONFIG_ENV,
  LEGACY_STRUCTURED_CUSTOM_BRIDGE_KEYS_ENV,
])
let bootstrapStructuredOwnershipConsumed = false
let structuredSettingsReaderForTest:
  | ((source: (typeof SETTING_SOURCES)[number]) => SettingsJson | null)
  | null = null

function hasOwnSetting(
  settings: SettingsJson,
  key: 'customModel' | 'customModels',
): boolean {
  return Object.prototype.hasOwnProperty.call(settings, key)
}

export function adoptBootstrapStructuredBridgeOwnership(
  target: StructuredBridgeEnvTarget,
  ownership: Map<string, string>,
  serializedBootstrap: string | undefined,
): void {
  if (!serializedBootstrap) return
  let parsed: unknown
  try {
    parsed = JSON.parse(serializedBootstrap)
  } catch {
    return
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return
  const keys = (parsed as Record<string, unknown>)[
    STRUCTURED_CUSTOM_BRIDGE_KEYS_FIELD
  ]
  if (!Array.isArray(keys)) return
  for (const key of keys) {
    if (typeof key !== 'string') continue
    if (!STRUCTURED_CUSTOM_BRIDGE_KEYS.has(key)) continue
    const value = target[key]
    if (value) ownership.set(key, value)
  }
}

export function clearOwnedStructuredBridgeEnvironment(
  target: StructuredBridgeEnvTarget,
  ownership: Map<string, string>,
): void {
  for (const [key, ownedValue] of ownership) {
    if (target[key] === ownedValue) delete target[key]
  }
  ownership.clear()
}

/**
 * Retract only the legacy custom-provider projection.
 *
 * The same ownership map also contains Web Search bridge values. A custom
 * registry mutation must not reset those unrelated values or their ownership;
 * otherwise a model edit can silently disable a separately configured tool.
 */
export function clearOwnedStructuredCustomBridgeEnvironment(
  target: StructuredBridgeEnvTarget,
  ownership: Map<string, string>,
): void {
  for (const [key, ownedValue] of ownership) {
    if (!STRUCTURED_CUSTOM_BRIDGE_KEYS.has(key)) continue
    if (target[key] === ownedValue) delete target[key]
    ownership.delete(key)
  }
}

export function setOwnedStructuredBridgeValueIfMissing(
  target: StructuredBridgeEnvTarget,
  ownership: Map<string, string>,
  key: string,
  value: string | undefined,
): void {
  if (!value || target[key]) return
  target[key] = value
  ownership.set(key, value)
}

/**
 * Project/local/plugin settings are not credential or runtime owners.
 *
 * - trusted user/flag/policy/global sources may provide every key;
 * - plugin settings never become process-wide env;
 * - before trust, project/local may provide only PRE_TRUST_SAFE keys;
 * - after trust, project/local may additionally provide known post-trust and
 *   unknown ordinary keys, but never a key classified as sensitive.
 *
 * This helper is pure and is the single source-specific filter used by both
 * startup phases and the attack-contract fixtures.
 */
export function filterManagedEnvForSource(
  source: ManagedEnvSource,
  env: Record<string, string> | undefined,
  phase: ManagedEnvPhase,
): Record<string, string> {
  if (!env || source === 'plugin') return {}
  const eligibleEntries = Object.entries(env).filter(
    // Windows environment names are case-insensitive. Normalize only for the
    // internal deny decision so a trusted settings layer cannot alias-overwrite
    // the supervisor envelope with `crabcode_bootstrap_config`.
    ([key]) => !SETTINGS_ENV_ALWAYS_DENIED.has(key.toUpperCase()),
  )
  if (isTrustedManagedEnvSource(source)) {
    return Object.fromEntries(eligibleEntries)
  }

  const filtered: Record<string, string> = {}
  for (const [key, value] of eligibleEntries) {
    const classification = classifyManagedEnvVar(key)
    if (
      classification === 'pre-trust-safe' ||
      (phase === 'post-trust' && classification !== 'sensitive')
    ) {
      filtered[key] = value
    }
  }
  return filtered
}

/** Merge already-ordered source layers using the same filter as live startup. */
export function mergeManagedEnvLayers(
  layers: ReadonlyArray<ManagedEnvLayer>,
  phase: ManagedEnvPhase,
): Record<string, string> {
  const merged: Record<string, string> = {}
  for (const layer of layers) {
    Object.assign(
      merged,
      filterManagedEnvForSource(layer.source, layer.env, phase),
    )
  }
  return merged
}

/**
 * Structured provider bridges carry endpoint/key fields too, so they must keep
 * source identity just like settings.env. Only trusted sources participate;
 * project/local/plugin structured values are ignored rather than merged first
 * and filtered after provenance has been lost.
 */
export function mergeTrustedStructuredSettings(
  layers: ReadonlyArray<ManagedStructuredSettingsLayer>,
): SettingsJson {
  let merged: SettingsJson = {}
  for (const { source, settings } of layers) {
    if (!settings || !isTrustedManagedEnvSource(source)) continue

    // Preserve source priority for the mutually exclusive provider modes.
    // A registry marker in one source suppresses a singular value only from
    // that source or a lower source; a higher policy/flag singular value must
    // still override a lower user registry. When the same source contains both
    // (an older mixed-state write), keep both so registry presence suppresses
    // the stale singular bridge.
    const hasSingular = settings.customModel !== undefined
    // Presence, not validity/truthiness, is the representation marker. An
    // empty, malformed, or explicitly-undefined registry must still suppress
    // the stale singular provider bridge (fail closed for gateway visibility).
    const hasRegistry = hasOwnSetting(settings, 'customModels')
    if (hasRegistry) {
      delete merged.customModel
      merged.customModels = settings.customModels
      if (hasSingular) merged.customModel = settings.customModel
    } else if (hasSingular) {
      delete merged.customModels
      merged.customModel = mergeWith(
        {},
        merged.customModel ?? {},
        settings.customModel,
      ) as SettingsJson['customModel']
    }

    const providerSettings: SettingsJson = {}
    if (settings.webSearch !== undefined) {
      providerSettings.webSearch = settings.webSearch
    }
    if (settings.mediaSidecar !== undefined) {
      providerSettings.mediaSidecar = settings.mediaSidecar
    }
    merged = mergeWith(merged, providerSettings) as SettingsJson
  }
  return merged
}

/**
 * `crabcode ssh` remote: ACOSMI_UNIX_SOCKET routes auth through a -R forwarded
 * socket to a local proxy, and the launcher sets a handful of placeholder auth
 * env vars that the remote's ~/.crabcode settings.env MUST NOT clobber (see
 * isAcosmiAuthEnabled). Strip them from any settings-sourced env object.
 */
function withoutSSHTunnelVars(
  env: Record<string, string> | undefined,
): Record<string, string> {
  if (!env || !process.env.ACOSMI_UNIX_SOCKET) return env || {}
  const {
    ACOSMI_UNIX_SOCKET: _1,
    ACOSMI_BASE_URL: _2,
    ACOSMI_API_KEY: _3,
    ACOSMI_AUTH_TOKEN: _4,
    CRABCODE_OAUTH_TOKEN: _5,
    ...rest
  } = env
  return rest
}

/**
 * When the host owns inference routing (sets
 * CRABCODE_PROVIDER_MANAGED_BY_HOST in spawn env), strip
 * provider-selection / model-default vars from settings-sourced env so a
 * user's ~/.crabcode/settings.json can't redirect requests away from the
 * host-configured provider.
 */
function withoutHostManagedProviderVars(
  env: Record<string, string> | undefined,
): Record<string, string> {
  if (!env) return {}
  if (!isEnvTruthy(process.env.CRABCODE_PROVIDER_MANAGED_BY_HOST)) {
    return env
  }
  const out: Record<string, string> = {}
  for (const [key, value] of Object.entries(env)) {
    if (!isProviderManagedEnvVar(key)) {
      out[key] = value
    }
  }
  return out
}

/**
 * Snapshot of env keys present before any settings.env is applied — for CCD,
 * these are the keys the desktop host set to orchestrate the subprocess.
 * Settings must not override them (OTEL_LOGS_EXPORTER=console would corrupt
 * the stdio JSON-RPC transport). Keys added LATER by user/project settings
 * are not in this set, so mid-session settings.json changes still apply.
 * Lazy-captured on first applySafeConfigEnvironmentVariables() call.
 */
let ccdSpawnEnvKeys: Set<string> | null | undefined

function withoutCcdSpawnEnvKeys(
  env: Record<string, string> | undefined,
): Record<string, string> {
  if (!env || !ccdSpawnEnvKeys) return env || {}
  const out: Record<string, string> = {}
  for (const [key, value] of Object.entries(env)) {
    if (!ccdSpawnEnvKeys.has(key)) out[key] = value
  }
  return out
}

/**
 * Compose the strip filters applied to every settings-sourced env object.
 */
function filterSettingsEnv(
  env: Record<string, string> | undefined,
): Record<string, string> {
  return withoutCcdSpawnEnvKeys(
    withoutHostManagedProviderVars(withoutSSHTunnelVars(env)),
  )
}

/**
 * Apply one source layer after both its trust-tier projection and the existing
 * host/SSH/desktop-spawn ownership filters. Applying layers sequentially is
 * intentional: if an earlier trusted layer enables host-managed-provider mode,
 * later settings layers must observe it.
 */
function applyManagedEnvLayer(
  source: ManagedEnvSource,
  env: Record<string, string> | undefined,
  phase: ManagedEnvPhase,
): void {
  Object.assign(
    process.env,
    filterSettingsEnv(filterManagedEnvForSource(source, env, phase)),
  )
}

function applyEnabledSettingsEnvLayers(
  phase: ManagedEnvPhase,
  includePolicy: boolean,
): void {
  for (const source of SETTING_SOURCES) {
    if (source === 'policySettings' && !includePolicy) continue
    if (!isSettingSourceEnabled(source)) continue
    applyManagedEnvLayer(source, getSettingsForSource(source)?.env, phase)
  }
}

function consumeBootstrapStructuredOwnership(): void {
  if (bootstrapStructuredOwnershipConsumed) return
  adoptBootstrapStructuredBridgeOwnership(
    process.env,
    structuredBridgeOwnedEnv,
    process.env[BOOTSTRAP_CONFIG_ENV],
  )
  bootstrapStructuredOwnershipConsumed = true
}

function deleteLegacyStructuredOwnershipMarkerAliases(): void {
  for (const key of Object.keys(process.env)) {
    if (key.toUpperCase() === LEGACY_STRUCTURED_CUSTOM_BRIDGE_KEYS_ENV) {
      delete process.env[key]
    }
  }
}

function readTrustedStructuredSettings(): SettingsJson {
  const readSettings = structuredSettingsReaderForTest ?? getSettingsForSource
  return mergeTrustedStructuredSettings(
    SETTING_SOURCES.filter(
      source =>
        isSettingSourceEnabled(source) && isTrustedManagedEnvSource(source),
    ).map(source => ({ source, settings: readSettings(source) })),
  )
}

/**
 * Apply the pre-trust projection to process.env. Trusted user/flag/policy/global
 * layers keep their full values. Project/local layers can contribute only the
 * small PRE_TRUST_SAFE_ENV_VARS class; plugin settings are always denied as a
 * process-wide env source.
 */
export function applySafeConfigEnvironmentVariables(): void {
  // Capture CCD spawn-env keys before any settings.env is applied (once).
  if (ccdSpawnEnvKeys === undefined) {
    ccdSpawnEnvKeys =
      process.env.CRABCODE_ENTRYPOINT === 'crabcode-desktop'
        ? new Set(Object.keys(process.env))
        : null
  }

  // Global config (~/.crabcode.json) is user-controlled. In CCD mode the
  // existing spawn-key filter still prevents it from replacing host env.
  applyManagedEnvLayer('globalConfig', getGlobalConfig().env, 'pre-trust')

  // Apply enabled sources in normal priority order, stopping before policy.
  // The source-specific filter keeps project/local provenance intact instead
  // of using a merged settings object that has already lost it.
  applyEnabledSettingsEnvLayers('pre-trust', false)

  // Compute remote-managed-settings eligibility now, with userSettings and
  // flagSettings env applied. Eligibility reads CRABCODE_USE_BEDROCK,
  // ACOSMI_BASE_URL — both settable via settings.env.
  // getSettingsForSource('policySettings') below consults the remote cache,
  // which guards on this. The two-phase structure makes the ordering
  // dependency visible: non-policy env → eligibility → policy env.
  isRemoteManagedSettingsEligible()

  if (isSettingSourceEnabled('policySettings')) {
    applyManagedEnvLayer(
      'policySettings',
      getSettingsForSource('policySettings')?.env,
      'pre-trust',
    )
  }
}

/**
 * Apply the post-trust projection to process.env.
 *
 * Trusted user/flag/policy/global values remain authoritative. Project/local
 * values may add ordinary env after trust, but cannot set credentials,
 * endpoints, headers, proxy/TLS trust, config roots or dynamic loader/executable
 * paths. Plugin settings never become process-wide env.
 */
export function applyConfigEnvironmentVariables(): void {
  // Rust may have expanded the same trusted legacy object before spawning the
  // TS child. Consume ownership exactly once from the supervisor-generated
  // bootstrap envelope. A separate inherited marker is deliberately ignored:
  // parent shells and every settings.env source can otherwise forge it.
  consumeBootstrapStructuredOwnership()
  deleteLegacyStructuredOwnershipMarkerAliases()

  // Reconcile bridge-owned values before reapplying explicit source layers.
  // This order is essential: an explicit settings.env value must survive and
  // must not be mistaken for the structured bridge's previous projection.
  clearOwnedStructuredBridgeEnvironment(process.env, structuredBridgeOwnedEnv)

  applyManagedEnvLayer('globalConfig', getGlobalConfig().env, 'post-trust')
  applyEnabledSettingsEnvLayers('post-trust', true)

  // Bridge selected structured settings → env vars for TS-side provider detection.
  // Rust bootstrap already does this for Go, but TS process.env also needs
  // these values for getAPIProvider() and WebSearchTool.isEnabled().
  bridgeStructuredSettingsToEnv(readTrustedStructuredSettings())

  // Clear caches so agents are rebuilt with the new env vars
  clearCACertsCache()
  clearMTLSCache()
  clearProxyCache()

  // Reconfigure proxy/mTLS agents to pick up any proxy env vars from settings
  configureGlobalAgents()
}

/**
 * Decide the CRABCODE_CUSTOM_* provider-env bridge from settings.
 *
 * ONLY a legacy singular `settings.customModel` with no registry field (a
 * whole-app custom-provider replacement) is bridged. Presence of the additive
 * `settings.customModels` registry is authoritative, even when it is empty;
 * those entries are per-session models selected via a
 * `custom:<uuid>` reference and routed by resolveCustomModelRuntime, which reads
 * each entry's baseUrl/apiKeyHandle DIRECTLY. Bridging them would set
 * CRABCODE_CUSTOM_BASE_URL → getAPIProvider() returns 'custom' →
 * isModelCapabilitiesEligible() false + getModelOptionsBase skips the gateway
 * branch, so the gateway model list vanishes from the TUI picker whenever a
 * custom model exists (W-CUSTOM-MODEL-PROVIDER-FLIP, 2026-07-07).
 *
 * Pure + exported for regression coverage. Returns null when nothing should be
 * bridged; otherwise the base URL + resolved API key (either may be undefined).
 */
export function resolveCustomProviderEnvBridge(
  settings: Pick<SettingsJson, 'customModel' | 'customModels'>,
  gate: {
    membershipActive?: boolean | null
    membershipPlanCode?: string | null
  },
  readApiKey: (handle: string | undefined) => string | null,
): { baseUrl: string | undefined; apiKey: string | undefined } | null {
  // Upgrade safety: a first registry write atomically materializes legacy
  // entries and deletes `customModel`, but older builds could leave both keys
  // behind. Registry presence must win in that mixed state or the stale
  // singular key would set CRABCODE_CUSTOM_BASE_URL and hide gateway models.
  if (hasOwnSetting(settings as SettingsJson, 'customModels')) return null

  const cm = settings.customModel
  if (!cm || !shouldBridgeCustomModelSettings({ customModel: cm, ...gate })) {
    return null
  }
  const apiKey = cm.apiKey ?? readApiKey(cm.apiKeyHandle)
  return { baseUrl: cm.baseUrl, apiKey: apiKey ?? undefined }
}

/**
 * Extract trusted-source webSearch and customModel settings
 * into process.env so TS-side provider detection works.
 * Project/local/plugin structured settings cannot supply provider endpoints,
 * credentials through this bridge. Desktop enablement is never projected to
 * env: only the persisted trusted setting may enable it. Existing
 * explicit/spawn env still has priority (setIfMissing).
 */
function bridgeStructuredSettingsToEnv(settings: SettingsJson): void {
  // webSearch → CRABCODE_SEARCH_*
  const ws = settings.webSearch
  if (ws) {
    setStructuredBridgeIfMissing('CRABCODE_SEARCH_PROVIDER', ws.provider)
    if (ws.provider === 'ali' && ws.ali) {
      setStructuredBridgeIfMissing('CRABCODE_SEARCH_ALI_API_KEY', ws.ali.apiKey)
      if (ws.ali.endpoint)
        setStructuredBridgeIfMissing(
          'CRABCODE_SEARCH_ALI_ENDPOINT',
          ws.ali.endpoint,
        )
      if (ws.ali.model)
        setStructuredBridgeIfMissing('CRABCODE_SEARCH_ALI_MODEL', ws.ali.model)
    }
    if (ws.provider === 'bocha' && ws.bocha) {
      setStructuredBridgeIfMissing(
        'CRABCODE_SEARCH_BOCHA_API_KEY',
        ws.bocha.apiKey,
      )
      if (ws.bocha.endpoint)
        setStructuredBridgeIfMissing(
          'CRABCODE_SEARCH_BOCHA_ENDPOINT',
          ws.bocha.endpoint,
        )
    }
  }

  // customModel (LEGACY singular custom provider) → CRABCODE_CUSTOM_*
  //
  // W-CUSTOM-MODEL-PROVIDER-FLIP (2026-07-07 root-cause fix): ONLY the legacy
  // singular `settings.customModel` (a whole-app provider replacement) bridges
  // to CRABCODE_CUSTOM_BASE_URL. The additive `customModels` registry
  // is deliberately NOT bridged here.
  //
  // Why the array must NOT be bridged: `customModels` entries are per-session
  // models selected via a `custom:<uuid>` reference and routed by
  // resolveCustomModelRuntime, which reads each entry's `baseUrl`/`apiKeyHandle`
  // DIRECTLY (customModelResolver.ts) — the send path never reads
  // CRABCODE_CUSTOM_*. Bridging the array set CRABCODE_CUSTOM_BASE_URL, which
  // made getAPIProvider() (providers.ts) return 'custom' instead of 'acosmi' the
  // moment ANY custom model existed. That flipped isModelCapabilitiesEligible()
  // to false and made getModelOptionsBase skip the gateway branch entirely, so
  // the gateway model list VANISHED from the TUI picker whenever a custom model
  // was configured (delete it → provider back to 'acosmi' → gateway returns).
  // The additive registry is an OVERLAY on the Acosmi gateway, never a provider
  // replacement — every custom-model user is a Plus member with gateway access.
  //
  // This mirrors the Rust bootstrap (crates/{crabcode-cli,acosmi-runtime}/src/
  // config.rs), which likewise bridges ONLY the singular `customModel`.
  bridgeCustomProviderSettingsToEnv(settings)
}

function bridgeCustomProviderSettingsToEnv(settings: SettingsJson): void {
  // Short-circuit before auth/keychain argument evaluation. Registry mode (or
  // no legacy singular at all) needs neither dependency; touching auth here can
  // fail for a signed-out worker even though the correct action is simply to
  // leave CRABCODE_CUSTOM_* absent and keep the gateway catalog visible.
  if (hasOwnSetting(settings, 'customModels') || !settings.customModel) return
  const customBridge = resolveCustomProviderEnvBridge(
    settings,
    getMembershipGateInput(),
    readCustomModelApiKeySync,
  )
  if (customBridge) {
    setStructuredBridgeIfMissing(
      'CRABCODE_CUSTOM_BASE_URL',
      customBridge.baseUrl,
    )
    setStructuredBridgeIfMissing('CRABCODE_CUSTOM_API_KEY', customBridge.apiKey)
  }

}

/**
 * Reconcile only the legacy custom-provider environment from current settings.
 *
 * This is the control-worker repair edge used at startup and immediately after
 * a custom-model settings mutation. It deliberately does not re-run the full
 * managed-env pipeline: only values proven to be owned by the structured
 * custom bridge are retracted, Web Search bridge state is untouched, and an
 * explicit shell/host/settings.env CRABCODE_CUSTOM_* value is never adopted or
 * deleted. Registry own-property presence is authoritative, regardless of the
 * registry value.
 *
 * Settings/keychain/auth reads are inside the fail-closed boundary. On any
 * failure the bridge-owned custom values stay cleared so provider detection
 * falls back to the gateway, and callers receive `false` instead of an error.
 */
export function reconcileCustomProviderEnvironmentFromSettings(): boolean {
  consumeBootstrapStructuredOwnership()
  deleteLegacyStructuredOwnershipMarkerAliases()
  clearOwnedStructuredCustomBridgeEnvironment(
    process.env,
    structuredBridgeOwnedEnv,
  )

  try {
    bridgeCustomProviderSettingsToEnv(readTrustedStructuredSettings())
    return true
  } catch {
    return false
  }
}

/** Test-only deterministic reader seam for failure and source-priority cases. */
export function __setManagedEnvStructuredSettingsReaderForTests(
  reader:
    | ((source: (typeof SETTING_SOURCES)[number]) => SettingsJson | null)
    | null,
): void {
  structuredSettingsReaderForTest = reader
}

/** Test-only reset for module-level ownership inherited by the long-lived worker. */
export function __resetManagedEnvBridgeForTests(): void {
  structuredBridgeOwnedEnv.clear()
  bootstrapStructuredOwnershipConsumed = false
  structuredSettingsReaderForTest = null
}

function setStructuredBridgeIfMissing(
  key: string,
  value: string | undefined,
): void {
  setOwnedStructuredBridgeValueIfMissing(
    process.env,
    structuredBridgeOwnedEnv,
    key,
    value,
  )
}
