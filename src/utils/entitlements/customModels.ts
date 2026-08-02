/**
 * Custom-model feature entitlement.
 *
 * Pure functions that decide whether an account may configure / use custom
 * bring-your-own models. Deliberately free of renderer and remote-server
 * dependencies so the TUI and its local runtime can reuse the same logic.
 *
 * Source of truth = the Acosmi membership signals persisted on OAuthTokens:
 * `membershipActive` (`getMembership().hasActive`) plus, since
 * W-CUSTOM-MODEL-PLUS-GATE (2026-07-04), `membershipPlanCode`
 * (`getMembership().planCode`) ranked through the plan-tier ladder in
 * `planTier.ts` — the gate floor is「Plus」(wire code BASIC). See
 * localModels.ts for why this does NOT read the legacy `subscriptionType`
 * field.
 */
import { parseCustomModelReference } from '../model/customModelReference.js'
import { meetsCustomModelTier } from './planTier.js'

/** Machine-readable explanation of a custom-model entitlement decision. */
export type CustomModelEntitlementReason =
  /** Allowed: active paid membership at or above the Plus floor. */
  | 'paid-subscription'
  /** Blocked: membership unknown — signed out, inference-only token, or not yet fetched. */
  | 'no-subscription'
  /** Blocked: account is on the free tier (membership explicitly inactive). */
  | 'free-tier'
  /** Blocked: active membership below the Plus floor (e.g. GO). */
  | 'tier-too-low'
  /**
   * Blocked: active membership but the plan code is missing or unregistered —
   * fail-closed (裁决① 2026-07-04); a membership refresh self-heals this.
   */
  | 'unknown-tier'

export interface CustomModelEntitlement {
  /** True when the account may configure / use custom models. */
  allowed: boolean
  /** Why the decision was reached, for diagnostics and UI messaging. */
  reason: CustomModelEntitlementReason
}

export interface CustomModelEntitlementInput {
  /**
   * Authoritative Acosmi membership flag (`getMembership().hasActive`).
   * `true` → active paid member; `false` → free tier (locked, show
   * upgrade); `null` / `undefined` → unknown → fail-closed (locked).
   */
  membershipActive?: boolean | null
  /**
   * Persisted `getMembership().planCode` (`OAuthTokens.membershipPlanCode`,
   * e.g. `"PRO_MAX"`, yearly variants `"..._YEARLY"`). Only consulted when
   * `membershipActive === true`; ranked via `planTier.ts` against the Plus
   * floor. Missing/unregistered codes fail closed as `unknown-tier`
   * (裁决① 2026-07-04) — `syncMembershipActive()` self-heals on the next
   * successful fetch.
   */
  membershipPlanCode?: string | null
}

export interface CustomModelReferenceInput {
  model?: string | null
  envCustomModelOption?: string | null
  customModel?: {
    models?: Record<
      string,
      {
        id?: string | null
      }
    >
  } | null
  /**
   * Flat multi-provider custom-model registry.
   * When present and non-empty, this takes priority over the legacy
   * `customModel` map (PR-2b projects legacy into this array, so matching both
   * would be redundant). Only `enabled !== false` entries are considered.
   *
   * `id` is the registry entry's stable identifier — required to match a
   * `custom:<id>` reference (W-CUSTOM-MODEL-NAMESPACE-ISOLATION 2026-07-01).
   * Optional so legacy callers that only ever matched bare `modelId` strings
   * keep compiling; those calls simply never hit the `custom:` track.
   */
  customModels?: Array<{
    id?: string | null
    modelId?: string | null
    enabled?: boolean
  }> | null
}

export interface CustomModelSettingsBridgeInput {
  customModel?: unknown
  membershipActive?: boolean | null
  /** See {@link CustomModelEntitlementInput.membershipPlanCode}. */
  membershipPlanCode?: string | null
}

/**
 * Resolve the custom-model entitlement for an account.
 *
 * Pure function over the membership signal. Every blocked path gets a distinct
 * reason so callers can surface an accurate message without re-deriving cause.
 */
export function resolveCustomModelEntitlement(
  input: CustomModelEntitlementInput = {},
): CustomModelEntitlement {
  if (input.membershipActive === true) {
    // W-CUSTOM-MODEL-PLUS-GATE (2026-07-04): active membership alone is no
    // longer enough — the plan must sit at or above the Plus floor. Missing
    // (`undefined`) and explicit-`null` plan codes are equivalent here: both
    // fail closed as `unknown-tier` (裁决①; the PR-1→PR-2 migration seam that
    // let a field-absent caller through was closed in PR-2 once every surface
    // passed the field explicitly). Callers that only need a deferral for
    // indeterminate tiers — e.g. destructive revocation flows — must branch
    // on the `unknown-tier` reason themselves, not on this resolver.
    const meets = meetsCustomModelTier(input.membershipPlanCode)
    if (meets === true) {
      return { allowed: true, reason: 'paid-subscription' }
    }
    if (meets === false) {
      return { allowed: false, reason: 'tier-too-low' }
    }
    // Plan code missing/unregistered → fail-closed (裁决①).
    return { allowed: false, reason: 'unknown-tier' }
  }
  if (input.membershipActive === false) {
    return { allowed: false, reason: 'free-tier' }
  }
  // null / undefined → unknown membership → fail-closed.
  return { allowed: false, reason: 'no-subscription' }
}

/**
 * Thin boolean wrapper over {@link resolveCustomModelEntitlement} for callers
 * that only need the yes/no answer (for example, a TUI gate).
 */
export function canUseCustomModels(input: CustomModelEntitlementInput = {}): boolean {
  return resolveCustomModelEntitlement(input).allowed
}

export function isCustomModelReference(
  input: CustomModelReferenceInput,
): boolean {
  const model = input.model?.trim()
  if (!model) return false

  // ── `custom:<entry.id>` track (W-CUSTOM-MODEL-NAMESPACE-ISOLATION 2026-07-01) ──
  // A well-formed reference in this namespace is ALWAYS a custom-model
  // reference by construction — it never needs to fall through to the env /
  // legacy-modelId checks below (those operate on a different, unprefixed
  // value space). Matching by `entry.id` here (not `modelId`) is required:
  // without it, a `custom:<uuid>` row would be misjudged as "not a custom
  // reference" and the entitlement gate in `validateModelSelection` would be
  // silently bypassed for non-entitled accounts.
  const entryId = parseCustomModelReference(model)
  if (entryId !== null) {
    const customModels = input.customModels
    if (!Array.isArray(customModels)) return false
    return customModels.some(
      (entry) => entry.enabled !== false && entry.id === entryId,
    )
  }

  const envCustomModel = input.envCustomModelOption?.trim()
  if (envCustomModel && model === envCustomModel) return true

  // ── Legacy bare-reference track ────────────────────────────────────────
  // Prefer the flat `customModels` registry when present and non-empty: match
  // by `entry.modelId` over `enabled !== false` entries. Legacy `customModel`
  // is ignored in that case (PR-2b projects legacy into `customModels`).
  const customModels = input.customModels
  if (Array.isArray(customModels) && customModels.length > 0) {
    for (const entry of customModels) {
      if (entry.enabled === false) continue
      if (entry.modelId?.trim() === model) return true
    }
    return false
  }

  const models = input.customModel?.models
  if (!models) return false

  for (const [alias, def] of Object.entries(models)) {
    if (alias.trim() === model) return true
    if (def.id?.trim() === model) return true
  }
  return false
}

export function shouldBridgeCustomModelSettings(
  input: CustomModelSettingsBridgeInput,
): boolean {
  return (
    input.customModel !== undefined &&
    input.customModel !== null &&
    canUseCustomModels({
      membershipActive: input.membershipActive,
      membershipPlanCode: input.membershipPlanCode,
    })
  )
}
