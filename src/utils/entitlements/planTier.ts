/**
 * Membership plan-tier ladder for the custom/local-model entitlement gate.
 *
 * This is product membership configuration, not a model-brand literal; tier
 * changes only touch this table.
 *
 * planCode values come from SDK `getMembership().planCode` (gateway
 * `/entitlements/membership`), persisted on `OAuthTokens.membershipPlanCode`.
 * Real-machine vocabulary (2026-07-04): FREE / GO / BASIC / PRO / PRO_MAX /
 * ULTRA / ENT_PRO / ENT_ULTRA — uppercase with underscores; yearly billing
 * variants carry a `_YEARLY` suffix. Display-name mapping: BASIC =「Plus」
 * (the wire code stays BASIC).
 *
 * Kept dependency-free on purpose (pure functions + one env read): the
 * entitlement modules are shared by TUI runtime paths and tests, and must not
 * drag in heavy import graphs.
 */

/**
 * planCode → ordinal rank. Higher = bigger plan. ENT_* map onto their
 * personal-tier equivalents. Keep this table and `normalizePlanCode` in
 * lockstep; `tests/unit/plan-tier-rank.test.ts` pins the vocabulary.
 */
export const PLAN_TIER_RANK: Readonly<Record<string, number>> = {
  FREE: 0,
  GO: 1,
  BASIC: 2, // display name「Plus」— the gate floor
  PRO: 3,
  PRO_MAX: 4,
  ULTRA: 5,
  ENT_PRO: 3,
  ENT_ULTRA: 5,
}

/** Minimum plan for custom/local models:「Plus」(wire code BASIC). */
export const CUSTOM_MODEL_MIN_TIER = 'BASIC'

/**
 * Human-facing display name of the gate floor. UI copy that names the
 * required tier (badges, locked hints) must use this constant instead of a
 * hardcoded string so a future floor change stays a one-table edit.
 */
export const CUSTOM_MODEL_MIN_TIER_DISPLAY_NAME = 'Plus'

/**
 * Normalize a raw planCode: trim, uppercase, hyphens/spaces → underscores,
 * strip the yearly-billing suffix (`"pro_max_yearly"` → `"PRO_MAX"`). Same
 * stable normalized shape.
 */
export function normalizePlanCode(code: string | null | undefined): string {
  return (code ?? '')
    .trim()
    .toUpperCase()
    .replace(/[-\s]+/g, '_')
    .replace(/_YEARLY$/, '')
}

// Warn once per unknown plan code — surfaces a new gateway tier without a log
// storm.
const warnedUnknownPlanCodes = new Set<string>()

function resolveMinTierRank(): number {
  // Ops rollback / canary switch (裁决记录 2026-07-04): setting
  // CRABCODE_CUSTOM_MODEL_MIN_TIER=FREE restores the pre-tier "any active
  // paid membership" gate; unknown override values are ignored.
  //
  // `typeof process` guard keeps this dependency-free helper safe in runtime
  // environments where `process` does not exist. The override is an ops lever for
  // the enforcing processes (TUI / worker, which have a real env); the
  // webview display layer always runs the default floor.
  const raw =
    typeof process !== 'undefined'
      ? process.env?.CRABCODE_CUSTOM_MODEL_MIN_TIER
      : undefined
  if (raw) {
    const rank = PLAN_TIER_RANK[normalizePlanCode(raw)]
    if (rank !== undefined) return rank
  }
  return PLAN_TIER_RANK[CUSTOM_MODEL_MIN_TIER]!
}

/**
 * Does this plan meet the custom/local-model tier floor?
 *
 *   `true`  → at or above the floor (Plus/Pro/Pro-Max/Ultra/ENT_*)
 *   `false` → known plan below the floor (FREE/GO)
 *   `null`  → plan code missing or unregistered — the gate fail-closes on
 *             this (裁决① 2026-07-04) with an on-demand membership refresh as
 *             the self-heal path; a warn-once log exposes unregistered codes.
 *
 * When the floor is rank 0 (env override `FREE`) every active membership
 * passes regardless of code — exact pre-tier behaviour.
 */
export function meetsCustomModelTier(
  planCode: string | null | undefined,
): boolean | null {
  const minRank = resolveMinTierRank()
  if (minRank <= 0) return true
  const normalized = normalizePlanCode(planCode)
  if (!normalized) return null
  const rank = PLAN_TIER_RANK[normalized]
  if (rank === undefined) {
    if (!warnedUnknownPlanCodes.has(normalized)) {
      warnedUnknownPlanCodes.add(normalized)
      console.warn(
        `[entitlements] unknown membership planCode "${planCode}" (normalized=${normalized}); ` +
          'treating as unknown tier (locked). If this is a legitimate new plan, register it in ' +
          'src/utils/entitlements/planTier.ts PLAN_TIER_RANK.',
      )
    }
    return null
  }
  return rank >= minRank
}

/** Test helper: reset the warn-once set between cases. */
export function _resetUnknownPlanCodeWarningsForTests(): void {
  warnedUnknownPlanCodes.clear()
}
