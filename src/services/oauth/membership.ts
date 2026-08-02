/**
 * Membership projection — the authoritative paid/free signal for the
 * local/custom-model entitlement gate.
 *
 * `syncMembershipActive()` fetches `Client.getMembership().hasActive` (Acosmi's
 * real membership state, gateway `/entitlements/membership`) and persists it
 * onto `OAuthTokens.membershipActive`, where the synchronous gate accessor
 * `getMembershipActive()` reads it. This is the only place that translates the
 * SDK membership into the persisted gate signal.
 *
 * Fail-soft by contract: any error (no client, SDK lacking getMembership,
 * network) leaves the stored value untouched — a transient gateway hiccup must
 * never lock a paying member out. The value also survives token refresh via the
 * `?? existing` preserve in `saveOAuthTokensIfNeeded`.
 */

import { getAcosmiClient, syncAcosmiClientFromDisk } from '../acosmi/client.js'
import {
  getAcosmiOAuthTokens,
  isAcosmiAuthEnabled,
  saveOAuthTokensIfNeeded,
} from '../../utils/auth.js'

/**
 * Persist an already-resolved membership flag onto the stored OAuth tokens.
 * Use this when the caller already holds an authoritative `hasActive` (e.g.
 * the account/plans handler that just called `getMembership()`) to avoid a
 * redundant round-trip. A `null` argument is treated as "unknown" and never
 * downgrades a stored value. Never throws.
 *
 * `plan` (optional) carries `getMembership().planCode`/`planName` for the
 * display layer (/usage tier label). Fields that are null/undefined/empty
 * never downgrade a stored value (same fail-soft contract). A plan-only
 * change (same `hasActive`, different `planCode`/`planName`) still writes.
 */
export async function persistMembershipActive(
  hasActive: boolean | null,
  plan?: { planCode?: string | null; planName?: string | null },
): Promise<boolean | null> {
  if (!isAcosmiAuthEnabled()) return null
  const tokens = getAcosmiOAuthTokens()
  if (!tokens?.accessToken || !tokens.refreshToken || !tokens.expiresAt) {
    return tokens?.membershipActive ?? null
  }
  // Normalize: only a non-empty string counts as "provided".
  const nextPlanCode =
    typeof plan?.planCode === 'string' && plan.planCode.trim().length > 0
      ? plan.planCode
      : null
  const nextPlanName =
    typeof plan?.planName === 'string' && plan.planName.trim().length > 0
      ? plan.planName
      : null
  const activeChanged =
    hasActive !== null && (tokens.membershipActive ?? null) !== hasActive
  const planCodeChanged =
    nextPlanCode !== null && nextPlanCode !== (tokens.membershipPlanCode ?? null)
  const planNameChanged =
    nextPlanName !== null && nextPlanName !== (tokens.membershipPlanName ?? null)
  if (!activeChanged && !planCodeChanged && !planNameChanged) {
    return tokens.membershipActive ?? null
  }
  const resolvedActive = hasActive ?? tokens.membershipActive ?? null
  try {
    await saveOAuthTokensIfNeeded({
      ...tokens,
      membershipActive: resolvedActive,
      // null here = "not provided"; saveOAuthTokensIfNeeded's `?? existing`
      // preserve keeps the stored value (no downgrade).
      membershipPlanCode: nextPlanCode ?? tokens.membershipPlanCode ?? null,
      membershipPlanName: nextPlanName ?? tokens.membershipPlanName ?? null,
    })
  } catch {
    // Fail-soft — keep last-known value.
    return tokens.membershipActive ?? null
  }
  return resolvedActive
}

/**
 * Fetch the current membership flag and persist it onto the stored OAuth
 * tokens. Returns the resolved `hasActive` (or the last-known stored value on
 * failure / unknown). Never throws.
 */
export async function syncMembershipActive(): Promise<boolean | null> {
  if (!isAcosmiAuthEnabled()) return null

  const tokens = getAcosmiOAuthTokens()
  // Inference-only tokens (env / fd, no refresh+expiry) are not persisted and
  // are never paid members — nothing to sync.
  if (!tokens?.accessToken || !tokens.refreshToken || !tokens.expiresAt) {
    return tokens?.membershipActive ?? null
  }

  let hasActive: boolean | null = null
  let planCode: string | null = null
  let planName: string | null = null
  try {
    // Pick up the freshest on-disk token (e.g. just persisted by
    // installOAuthTokens) before the membership round-trip. Cooldown-guarded.
    await syncAcosmiClientFromDisk()
    const client = await getAcosmiClient()
    // Feature-detect: SDKs < 2.6.0 lack getMembership entirely.
    if (!client || typeof client.getMembership !== 'function') {
      return tokens.membershipActive ?? null
    }
    const membership = (await client.getMembership()) as
      | { hasActive?: unknown; planCode?: unknown; planName?: unknown }
      | null
    hasActive =
      typeof membership?.hasActive === 'boolean' ? membership.hasActive : null
    // Display-only plan metadata for the /usage tier label — defensive typeof,
    // null = unknown (never downgrades the stored value).
    planCode =
      typeof membership?.planCode === 'string' ? membership.planCode : null
    planName =
      typeof membership?.planName === 'string' ? membership.planName : null
  } catch {
    // Fail-soft: keep the stored value (do not clobber a paying member).
    return tokens.membershipActive ?? null
  }

  // persistMembershipActive treats null as "unknown" (preserve last-known) and
  // skips the write when nothing (flag or plan) changed.
  return persistMembershipActive(hasActive, { planCode, planName })
}
