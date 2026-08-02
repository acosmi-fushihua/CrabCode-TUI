/**
 * Local-model feature entitlement.
 *
 * Separate from custom-model entitlements by design: local runtimes have
 * their own product gate, messaging, and future rollout switches even though
 * the first pass uses the same "active paid membership" signal.
 *
 * Source of truth = the Acosmi membership signals persisted on OAuthTokens:
 * `membershipActive` (`getMembership().hasActive`) plus, since
 * W-CUSTOM-MODEL-PLUS-GATE (2026-07-04), `membershipPlanCode`
 * (`getMembership().planCode`) ranked through `planTier.ts` — the gate floor
 * is「Plus」(wire code BASIC). This deliberately does NOT read the legacy
 * `subscriptionType` (CrabClaw-era free/pro/max/team/enterprise taxonomy via
 * `/api/oauth/profile.organization_type`): that field is consumed by ~12
 * unrelated tier-granular features and does not carry the Acosmi membership
 * state. Gating on it would either lock paying members out (it is null on
 * Acosmi) or, if collapsed to a tier, corrupt those other consumers.
 */
import { meetsCustomModelTier } from './planTier.js'

/** Machine-readable explanation of a local-model entitlement decision. */
export type LocalModelEntitlementReason =
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

export interface LocalModelEntitlement {
  /** True when the account may configure / use local models. */
  allowed: boolean
  /** Why the decision was reached, for diagnostics and UI messaging. */
  reason: LocalModelEntitlementReason
}

export interface LocalModelEntitlementInput {
  /**
   * Authoritative Acosmi membership flag (`getMembership().hasActive`).
   * `true`  → active paid member (unlock).
   * `false` → free tier (locked, show upgrade).
   * `null` / `undefined` → unknown (not logged in, inference-only token, or
   * membership not yet fetched) → fail-closed (locked). Once a `true` value has
   * been persisted it survives token refresh, so this only bites genuinely
   * unknown states.
   */
  membershipActive?: boolean | null
  /**
   * Persisted `getMembership().planCode` (`OAuthTokens.membershipPlanCode`).
   * Only consulted when `membershipActive === true`; ranked via `planTier.ts`
   * against the Plus floor. Missing/unregistered codes fail closed as
   * `unknown-tier` (裁决① 2026-07-04).
   */
  membershipPlanCode?: string | null
}

/**
 * Resolve the local-model entitlement for an account.
 *
 * Pure function over the membership signal. Every blocked path gets a distinct
 * reason so callers can surface accurate messaging without re-deriving cause.
 */
export function resolveLocalModelEntitlement(
  input: LocalModelEntitlementInput = {},
): LocalModelEntitlement {
  if (input.membershipActive === true) {
    // W-CUSTOM-MODEL-PLUS-GATE (2026-07-04): active membership alone is no
    // longer enough — the plan must sit at or above the Plus floor. Local
    // models share the custom-model floor (single product ruling). Missing
    // (`undefined`) and explicit-`null` plan codes both fail closed as
    // `unknown-tier` (裁决①; the PR-1→PR-2 migration seam was closed in PR-2).
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
 * Thin boolean wrapper over {@link resolveLocalModelEntitlement} for callers
 * that only need the yes/no answer.
 */
export function canUseLocalModels(input: LocalModelEntitlementInput = {}): boolean {
  return resolveLocalModelEntitlement(input).allowed
}
