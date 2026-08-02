/**
 * Personal knowledge-base PREMIUM feature entitlement
 * (W-MEMORY-KB-UPLIFT P3, 2026-07-17).
 *
 * Gates the gateway-billed / outbound knowledge-base capabilities:
 *   - ingest connectors (`knowledge_ingest_url` / `knowledge_ingest_file` /
 *     `knowledge_ingest_db` MemoryManage actions)
 *   - remote sync (`knowledge_sync_now`)
 *
 * The FREE baseline is untouched by design: manual CRUD,
 * agent drafts (`knowledge_draft`), knowledge browsing
 * (`knowledge_list` / `knowledge_read` / `knowledge_promote` — local-only, no
 * gateway spend) and the full three-scope hybrid retrieval all stay free.
 * Rerank/embedding are not gated here — the gateway bills them per model
 * entitlement already (402 backstop) and retrieval fail-softs without them.
 *
 * Mirrors `customModels.ts` (W-CUSTOM-MODEL-PLUS-GATE) verbatim in shape:
 * pure functions over the persisted membership signals
 * (`OAuthTokens.membershipActive` + `membershipPlanCode`), five
 * machine-readable reasons, unknown states fail closed, and a membership
 * refresh self-heals. Floor =「Plus」(wire code BASIC) — same single-product
 * ruling as custom models; ops rollback env `CRABCODE_KB_PREMIUM_MIN_TIER`.
 */
import { normalizePlanCode, PLAN_TIER_RANK } from './planTier.js'

/** Minimum plan for knowledge-base premium features:「Plus」(wire BASIC). */
export const KB_PREMIUM_MIN_TIER = 'BASIC'

/** Display name of the KB premium floor (UI copy must use this constant). */
export const KB_PREMIUM_MIN_TIER_DISPLAY_NAME = 'Plus'

/** Machine-readable explanation of a KB-premium entitlement decision. */
export type KbPremiumEntitlementReason =
  /** Allowed: active paid membership at or above the Plus floor. */
  | 'paid-subscription'
  /** Blocked: membership unknown — signed out, inference-only token, or not yet fetched. */
  | 'no-subscription'
  /** Blocked: account is on the free tier (membership explicitly inactive). */
  | 'free-tier'
  /** Blocked: active membership below the Plus floor (e.g. GO). */
  | 'tier-too-low'
  /** Blocked: active membership but plan code missing/unregistered — fail-closed. */
  | 'unknown-tier'

export interface KbPremiumEntitlement {
  allowed: boolean
  reason: KbPremiumEntitlementReason
}

export interface KbPremiumEntitlementInput {
  /** `getMembership().hasActive` persisted signal; null/undefined = unknown → fail-closed. */
  membershipActive?: boolean | null
  /** `getMembership().planCode` persisted signal; consulted only when active. */
  membershipPlanCode?: string | null
}

function resolveKbMinTierRank(): number {
  // Ops rollback lever (mirrors CRABCODE_CUSTOM_MODEL_MIN_TIER): setting
  // CRABCODE_KB_PREMIUM_MIN_TIER=FREE opens KB premium to any active
  // membership. `typeof process` guard for webview bundling.
  const raw =
    typeof process !== 'undefined'
      ? process.env?.CRABCODE_KB_PREMIUM_MIN_TIER
      : undefined
  if (raw) {
    const rank = PLAN_TIER_RANK[normalizePlanCode(raw)]
    if (rank !== undefined) return rank
  }
  return PLAN_TIER_RANK[KB_PREMIUM_MIN_TIER]!
}

/**
 * Does this plan meet the KB premium floor? Same tri-state semantics as
 * `meetsCustomModelTier`: `null` = missing/unregistered code → caller
 * fail-closes.
 */
export function meetsKbPremiumTier(
  planCode: string | null | undefined,
): boolean | null {
  const minRank = resolveKbMinTierRank()
  if (minRank <= 0) return true
  const normalized = normalizePlanCode(planCode)
  if (!normalized) return null
  const rank = PLAN_TIER_RANK[normalized]
  if (rank === undefined) return null
  return rank >= minRank
}

/**
 * Resolve the KB-premium entitlement for an account. Pure function over the
 * membership signals; every blocked path gets a distinct reason so callers
 * can surface an accurate message without re-deriving cause.
 */
export function resolveKbPremiumEntitlement(
  input: KbPremiumEntitlementInput = {},
): KbPremiumEntitlement {
  if (input.membershipActive === true) {
    const meets = meetsKbPremiumTier(input.membershipPlanCode)
    if (meets === true) {
      return { allowed: true, reason: 'paid-subscription' }
    }
    if (meets === false) {
      return { allowed: false, reason: 'tier-too-low' }
    }
    // Plan code missing/unregistered → fail-closed (同 custom-model 裁决①).
    return { allowed: false, reason: 'unknown-tier' }
  }
  if (input.membershipActive === false) {
    return { allowed: false, reason: 'free-tier' }
  }
  // null / undefined → unknown membership → fail-closed.
  return { allowed: false, reason: 'no-subscription' }
}

/** Thin boolean wrapper for callers that only need yes/no. */
export function canUseKbPremium(input: KbPremiumEntitlementInput = {}): boolean {
  return resolveKbPremiumEntitlement(input).allowed
}

/**
 * Human-facing blocked-message for tool `detail` output. One place so every
 * gated action words the upgrade path identically.
 */
export function kbPremiumBlockedDetail(reason: KbPremiumEntitlementReason): string {
  const base = `该能力属于个人知识库高级功能（${KB_PREMIUM_MIN_TIER_DISPLAY_NAME} 会员及以上可用）。`
  switch (reason) {
    case 'no-subscription':
      return `${base}当前登录态没有会员信息 — 请先登录 Acosmi 账号；已登录则稍后重试（会员信息刷新中）。`
    case 'free-tier':
      return `${base}当前账号为免费档 — 升级会员后即可使用（设置 → 账户 → 升级）。`
    case 'tier-too-low':
      return `${base}当前会员档位低于 ${KB_PREMIUM_MIN_TIER_DISPLAY_NAME} — 升级档位后即可使用。`
    case 'unknown-tier':
      return `${base}会员档位信息暂不可用（安全起见先锁定）— 重新打开应用或重新登录后自愈。`
    case 'paid-subscription':
      return base
  }
}
