/**
 * Custom model resolver — reads model definitions from settings.json
 * customModel configuration. Used by context.ts and betas.ts to adapt
 * behavior for non-Acosmi models.
 */

import type { SettingsJson } from '../settings/types.js'
import { parseCustomModelReference } from './customModelReference.js'
import type { CustomModelProtocol } from './customProtocolAdapter.js'
import { readCustomModelApiKeySync } from './customModelSecrets.js'
import { DEFAULT_MAIN_LOOP_MODEL } from './defaults.js'

/** Subset of the custom model definition relevant to capability resolution. */
export interface CustomModelDef {
  id: string
  /**
   * Wire protocol of the endpoint this entry dials. Capability decisions that
   * are protocol-specific read it — e.g. `output_config.effort` is a native
   * Anthropic Messages API param, so only `anthropic-compatible` endpoints
   * accept it. Absent on the legacy single-connection map (treated as
   * non-anthropic — the conservative fallback).
   */
  protocol?: CustomModelProtocol
  displayName?: string
  contextWindow: number
  maxOutputTokens: number
  supportsThinking?: boolean
  supportsTools?: boolean
  supportsJsonMode?: boolean
  /** User-declared image-input support (PR-7). Absent → treat as text-only. */
  supportsVision?: boolean
  isDefault?: boolean
}

/** Flat custom-model registry entry shape. */
type CustomModelEntry = NonNullable<SettingsJson['customModels']>[number]

/**
 * Read the flat `customModels` registry from settings, returning only the
 * non-empty array (so callers can treat absent/empty uniformly as "fall back
 * to legacy `customModel`"). Disabled entries are NOT filtered here — callers
 * apply the `enabled !== false` predicate per their needs.
 */
function getCustomModelsArray(): CustomModelEntry[] | undefined {
  const arr = getUserSettingsSafe()?.customModels
  if (!Array.isArray(arr) || arr.length === 0) return undefined
  return arr
}

/** Map a flat registry entry to the capability-resolution `CustomModelDef`. */
function entryToDef(entry: CustomModelEntry): CustomModelDef {
  return {
    id: entry.modelId,
    protocol: entry.protocol,
    displayName: entry.displayName,
    contextWindow: entry.contextWindow,
    maxOutputTokens: entry.maxOutputTokens,
    supportsThinking: entry.supportsThinking,
    supportsTools: entry.supportsTools,
    supportsJsonMode: entry.supportsJsonMode,
    supportsVision: entry.supportsVision,
    isDefault: entry.isDefault,
  }
}

/**
 * Resolve a custom model definition from settings.json.
 *
 * Prefers the flat `customModels` registry: when it is non-empty, only its
 * `enabled !== false` entries are matched (by `entry.modelId`) and the legacy
 * `customModel.models` map is ignored (PR-2b already projects legacy into
 * `customModels`, so reading both would double-count). Falls back to the
 * legacy map only when `customModels` is absent/empty.
 */
export function resolveCustomModelDef(
  modelId: string,
  customModelsOverride?: SettingsJson['customModels'] | null,
  customModelOverride?: SettingsJson['customModel'] | null,
  gatewaySlugsOverride?: readonly string[] | null,
): CustomModelDef | undefined {
  const registryArr =
    customModelsOverride !== undefined
      ? Array.isArray(customModelsOverride) && customModelsOverride.length > 0
        ? customModelsOverride
        : undefined
      : getCustomModelsArray()

  // ── Track 1 (PR-4, 2026-07-01 audit finding 4): `custom:<entry.id>` ────────
  // The namespace-isolated reference format written by the TUI picker
  // since W-CUSTOM-MODEL-NAMESPACE-ISOLATION. Before this branch existed the
  // capability layer (context.ts contextWindow / maxOutputTokens / display
  // name) could never resolve a prefixed selection and silently fell back to
  // gateway defaults (200k/32k) — every capability field the user declared in
  // the form was decorative.
  const entryId = parseCustomModelReference(modelId)
  if (entryId !== null) {
    if (!registryArr) return undefined
    const entry = registryArr.find((e) => e.enabled !== false && e.id === entryId)
    return entry ? entryToDef(entry) : undefined
  }

  // ── Track 2 (bare ref): mirror the ROUTING rule, including the
  // reserved-gateway-slug guard — a custom entry whose `modelId` collides
  // with a gateway slug never routes to the custom endpoint (P0-guard), so
  // it must not lend its capability declaration to the gateway model either.
  if (isReservedGatewaySlug(modelId, gatewaySlugsOverride)) return undefined

  if (registryArr) {
    const entry = registryArr.find(
      (e) => e.enabled !== false && e.modelId === modelId,
    )
    return entry ? entryToDef(entry) : undefined
  }

  // Legacy fallback: single-connection `customModel.models` map.
  const customModel =
    customModelOverride !== undefined
      ? customModelOverride
      : getUserSettingsSafe()?.customModel
  if (!customModel?.models) return undefined

  // Search by exact model ID or by alias key
  for (const [key, def] of Object.entries(customModel.models)) {
    if (def.id === modelId || key === modelId) {
      return def as CustomModelDef
    }
  }
  return undefined
}

/**
 * Get the default custom model definition, if any.
 *
 * Prefers the flat `customModels` registry (enabled entries only — `isDefault`
 * or first); falls back to the legacy `customModel.models` map.
 */
export function getDefaultCustomModelDef(): CustomModelDef | undefined {
  const customModels = getCustomModelsArray()
  if (customModels) {
    const enabled = customModels.filter((e) => e.enabled !== false)
    const chosen = enabled.find((e) => e.isDefault) ?? enabled[0]
    return chosen ? entryToDef(chosen) : undefined
  }

  const settings = getUserSettingsSafe()
  const customModel = settings?.customModel
  if (!customModel?.models) return undefined

  // Find the model marked as default, or fall back to first
  const models = Object.values(customModel.models) as CustomModelDef[]
  return models.find((m) => m.isDefault) ?? models[0]
}

/**
 * Check if custom model supports a specific capability.
 */
export function customModelSupports(
  modelId: string,
  capability: 'thinking' | 'tools' | 'jsonMode',
): boolean {
  const def = resolveCustomModelDef(modelId)
  if (!def) return false
  switch (capability) {
    case 'thinking':
      return def.supportsThinking ?? false
    case 'tools':
      return def.supportsTools ?? false
    case 'jsonMode':
      return def.supportsJsonMode ?? false
  }
}

/**
 * Fully-resolved custom-model runtime descriptor — everything the custom
 * provider chat-stream adapter needs to dial the user-declared `baseUrl`.
 * `apiKey` is resolved from the legacy plaintext field or, preferred, the
 * secure-storage handle. `null` apiKey means the credential is missing /
 * unreadable; the caller surfaces a clear error rather than dialing unauthed.
 */
export interface ResolvedCustomModelRuntime {
  provider: CustomModelProtocol
  baseUrl: string
  apiKey: string | null
  /** Bare provider-side model id sent on the wire (never the alias key). */
  modelId: string
  displayName: string
}

export interface CustomModelRuntimeResolutionSeams {
  readApiKey?: (handle: string | undefined) => string | null
  getFreshUserSettings?: () => SettingsJson | null | undefined
}

/**
 * Resolve a `mainLoopModel` reference to its custom-model runtime descriptor.
 *
 * Returns `null` when the reference is not a configured custom model (a
 * managed-catalog slug, an alias, a `local:<id>` reference, or no
 * `settings.customModel` at all) — the caller then falls through to the
 * Acosmi gateway path.
 *
 * Two routing tracks (W-CUSTOM-MODEL-NAMESPACE-ISOLATION 2026-07-01):
 *
 * 1. `custom:<entry.id>` (current format, written by the TUI picker) —
 *    matched directly by the registry entry's stable `id` (a `randomUUID()`).
 *    This id lives in its own namespace by construction, so it can never
 *    collide with a gateway slug or another entry's `modelId`; no
 *    reserved-slug guard applies on this track.
 * 2. A bare, unprefixed reference (legacy format — pre-existing persisted
 *    `mainLoopModel` values written before this namespace-isolation change) —
 *    matched by `customModel.models` alias key / provider-side `id`, OR by a
 *    flat-registry entry's `modelId`, mirroring `isCustomModelReference`.
 *    Kept as a permanent compatibility fallback (see §五 in the design doc):
 *    there is no batch migration of on-disk settings, so old persisted values
 *    must keep resolving until the user re-selects a model. The legacy track
 *    retains the `isReservedGatewaySlug` collision guard as a safety net,
 *    since a bare string cannot otherwise distinguish "pick the gateway
 *    model" from "pick the custom entry".
 *
 * Intentionally NOT entitlement-gated: this is a pure config lookup. The
 * runtime entitlement check (BLOCKER-4) lives in `queryModel` so the gate is
 * explicit and independently testable.
 *
 * `customModelOverride` is a test seam: pass a `settings.customModel` object
 * directly to exercise the legacy matching logic without touching disk.
 * `customModelsOverride` is the parallel seam for the flat `customModels`
 * registry. When `customModelsOverride` is provided AND non-empty (or, when
 * omitted, when disk `customModels` is non-empty), the flat registry wins and
 * the legacy `customModel` path is skipped entirely (PR-2b projects legacy
 * into the array, so reading both would double-resolve).
 */
export function resolveCustomModelRuntime(
  modelRef: string | null | undefined,
  customModelOverride?: SettingsJson['customModel'] | null,
  customModelsOverride?: SettingsJson['customModels'] | null,
  gatewaySlugsOverride?: readonly string[] | null,
  seams?: CustomModelRuntimeResolutionSeams,
): ResolvedCustomModelRuntime | null {
  const ref = modelRef?.trim()
  if (!ref) return null

  const customModels =
    customModelsOverride !== undefined
      ? customModelsOverride
      : getUserSettingsSafe()?.customModels
  const readApiKey = seams?.readApiKey ?? readCustomModelApiKeySync
  const canRefreshMissingHandle =
    seams?.getFreshUserSettings !== undefined ||
    (customModelOverride === undefined && customModelsOverride === undefined)

  const refreshAfterMissingHandle = (): ResolvedCustomModelRuntime | null | undefined => {
    if (!canRefreshMissingHandle) return undefined
    const fresh = seams?.getFreshUserSettings
      ? { ok: true, settings: seams.getFreshUserSettings() }
      : getFreshUserSettingsSafe()
    if (!fresh.ok) return undefined
    return resolveCustomModelRuntime(
      ref,
      fresh.settings?.customModel ?? null,
      fresh.settings?.customModels ?? null,
      gatewaySlugsOverride,
      { readApiKey },
    )
  }

  // ── Track 1: `custom:<entry.id>` — the namespace-isolated primary path ───
  const entryId = parseCustomModelReference(ref)
  if (entryId !== null) {
    if (!Array.isArray(customModels)) return null
    const entry = customModels.find(
      (e) => e.enabled !== false && e.id === entryId,
    )
    if (!entry) return null
    const runtime = {
      provider: entry.protocol,
      baseUrl: entry.baseUrl,
      apiKey: readApiKey(entry.apiKeyHandle) ?? null,
      modelId: entry.modelId,
      displayName: entry.displayName ?? entry.modelId,
    }
    if (runtime.apiKey === null) {
      const refreshed = refreshAfterMissingHandle()
      if (refreshed !== undefined) return refreshed
    }
    return runtime
  }

  // ── Track 2: legacy bare-reference fallback ───────────────────────────────
  // ── P0-guard (W-CUSTOM-MODEL-SLUG-COLLISION 2026-06-29) ──────────────────
  // A managed gateway slug must NEVER be hijacked by a same-named custom entry.
  // queryModel routes custom-before-gateway, so a custom entry whose `modelId`
  // collides with a gateway slug (e.g. user filled `deepseek-v4-flash`, the
  // `DEFAULT_MAIN_LOOP_MODEL` value) would shadow the gateway's own model and
  // dial the user's keyless custom endpoint → "custom model has no API key" on
  // the *default* model. Reserving every gateway-known slug here makes the
  // gateway always win (return null → caller falls through to the Acosmi path),
  // and self-heals existing dirty config without requiring a delete. Only
  // applies to this legacy track — the `custom:<id>` track above never reaches
  // here, since it returns before this point.
  if (isReservedGatewaySlug(ref, gatewaySlugsOverride)) return null

  // ── Prefer the flat multi-provider `customModels` registry ───────────────
  // Each entry carries its own protocol / baseUrl / apiKeyHandle, so runtime
  // resolution is per-entry (no shared connection).
  if (Array.isArray(customModels) && customModels.length > 0) {
    const entry = customModels.find(
      (e) => e.enabled !== false && e.modelId === ref,
    )
    if (!entry) return null
    const runtime = {
      provider: entry.protocol,
      baseUrl: entry.baseUrl,
      apiKey: readApiKey(entry.apiKeyHandle) ?? null,
      modelId: entry.modelId,
      displayName: entry.displayName ?? entry.modelId,
    }
    if (runtime.apiKey === null) {
      const refreshed = refreshAfterMissingHandle()
      if (refreshed !== undefined) return refreshed
    }
    return runtime
  }

  // ── Legacy single-connection `customModel.models` fallback ───────────────
  const customModel =
    customModelOverride !== undefined
      ? customModelOverride
      : getUserSettingsSafe()?.customModel
  if (!customModel?.models) return null

  let matched: CustomModelDef | undefined
  for (const [alias, def] of Object.entries(customModel.models)) {
    if (def.id === ref || alias === ref) {
      matched = def as CustomModelDef
      break
    }
  }
  if (!matched) return null

  const apiKey =
    customModel.apiKey ?? readApiKey(customModel.apiKeyHandle)
  const runtime = {
    provider: customModel.provider,
    baseUrl: customModel.baseUrl,
    apiKey: apiKey ?? null,
    modelId: matched.id,
    displayName: matched.displayName ?? matched.id,
  }
  if (runtime.apiKey === null) {
    const refreshed = refreshAfterMissingHandle()
    if (refreshed !== undefined) return refreshed
  }
  return runtime
}

/**
 * PR-7 (W-DOC-VISION-QUALITY-REMEDIATION): does `modelRef` route to a custom
 * model, and did the user declare it vision-capable?
 *
 * Mirrors `resolveCustomModelRuntime`'s two-track matching (custom:<entry.id>
 * primary / legacy bare ref with the reserved-gateway-slug guard) WITHOUT the
 * apiKey read — this runs on the per-turn classify path (queryModel media
 * policy + FileReadTool routing), where a secure-storage/keychain hit per call
 * would be unacceptable.
 *
 * `supportsVision` semantics (裁决 D9, fail-closed): `true` only when the
 * matched definition explicitly declares it; `false | undefined` — and an
 * unresolvable `custom:<id>` reference — read as false. Blind-sending images
 * costs the whole turn (400 from the user's endpoint); degrading costs one
 * honest placeholder and a checkbox to flip.
 */
export function resolveCustomModelVisionDeclaration(
  modelRef: string | null | undefined,
  customModelsOverride?: SettingsJson['customModels'] | null,
  gatewaySlugsOverride?: readonly string[] | null,
  customModelOverride?: SettingsJson['customModel'] | null,
): { isCustom: true; supportsVision: boolean } | { isCustom: false } {
  const ref = modelRef?.trim()
  if (!ref) return { isCustom: false }

  const customModels =
    customModelsOverride !== undefined
      ? customModelsOverride
      : getUserSettingsSafe()?.customModels

  // Track 1: custom:<entry.id> — always the custom namespace, even when the
  // entry is missing/disabled (fail-closed rather than falling open to the
  // gateway classifier, which can never know this model).
  const entryId = parseCustomModelReference(ref)
  if (entryId !== null) {
    const entry = Array.isArray(customModels)
      ? customModels.find((e) => e.enabled !== false && e.id === entryId)
      : undefined
    return { isCustom: true, supportsVision: entry?.supportsVision === true }
  }

  // Track 2: legacy bare reference — gateway slugs always win (same guard as
  // the runtime resolver; a custom entry must never shadow a managed model).
  if (isReservedGatewaySlug(ref, gatewaySlugsOverride)) {
    return { isCustom: false }
  }
  if (Array.isArray(customModels) && customModels.length > 0) {
    const entry = customModels.find(
      (e) => e.enabled !== false && e.modelId === ref,
    )
    if (!entry) return { isCustom: false }
    return { isCustom: true, supportsVision: entry.supportsVision === true }
  }
  const customModel =
    customModelOverride !== undefined
      ? customModelOverride
      : getUserSettingsSafe()?.customModel
  if (customModel?.models) {
    for (const [alias, def] of Object.entries(customModel.models)) {
      if (def.id === ref || alias === ref) {
        return {
          isCustom: true,
          supportsVision: (def as CustomModelDef).supportsVision === true,
        }
      }
    }
  }
  return { isCustom: false }
}

/**
 * Custom model registry ownership is deliberately narrower than ordinary
 * effective settings: only `userSettings` may choose an endpoint or key
 * handle. Project/local/policy files are untrusted for this credential-bearing
 * routing decision and must not override the registry truth source.
 */
function getUserSettingsSafe(): SettingsJson | undefined {
  try {
    // Dynamic require keeps the existing circular-dependency boundary while
    // avoiding the deprecated merged reader.
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { getSettingsForSource } =
      require('../settings/settings.js') as typeof import('../settings/settings.js')
    return getSettingsForSource('userSettings') ?? undefined
  } catch {
    return undefined
  }
}

function getFreshUserSettingsSafe(): {
  ok: boolean
  settings: SettingsJson | null | undefined
} {
  try {
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { getSettingsForSource } =
      require('../settings/settings.js') as typeof import('../settings/settings.js')
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { resetSettingsCache } =
      require('../settings/settingsCache.js') as typeof import('../settings/settingsCache.js')
    resetSettingsCache()
    return { ok: true, settings: getSettingsForSource('userSettings') }
  } catch {
    return { ok: false, settings: undefined }
  }
}

/**
 * Is `ref` a slug that belongs to the managed gateway and therefore must never
 * resolve to a custom entry? True when `ref` matches any cached gateway model
 * id (all variants — hidden/locked included, so even those slugs stay reserved)
 * or the `DEFAULT_MAIN_LOOP_MODEL` floor (covers the brief cache-empty window at
 * boot, when the gateway list hasn't hydrated yet but the default is still live).
 *
 * `override` is a test seam: pass an explicit slug list to exercise the guard
 * deterministically without populating the on-disk capabilities cache.
 */
function isReservedGatewaySlug(
  ref: string,
  override?: readonly string[] | null,
): boolean {
  if (ref === DEFAULT_MAIN_LOOP_MODEL) return true
  const slugs = override ?? getCachedGatewaySlugsSafe()
  return slugs.includes(ref)
}

/** Cached gateway model slugs (all variants), or `[]` if unavailable. */
function getCachedGatewaySlugsSafe(): string[] {
  try {
    // Dynamic import mirrors `getUserSettingsSafe` — defensive against any future
    // import cycle through the capabilities module's transitive graph.
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { getCachedModels } =
      require('./modelCapabilities.js') as typeof import('./modelCapabilities.js')
    return getCachedModels({ includeHidden: true, includeLocked: true }).map(
      (m) => m.id,
    )
  } catch {
    return []
  }
}
