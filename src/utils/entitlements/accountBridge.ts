/**
 * Account Bridge feature entitlement — the client half of the double-layer
 * membership gate (2026-07-24 三轮方案 §3.2#2).
 *
 * Layer 1 (authoritative) is server-side: the control plane refuses to sign an
 * eligibility grant for an account below the floor, so the hard boundary lives
 * where the private key lives. This module is layer 2: the UX + defence-in-
 * depth gate that keeps a below-floor account from selecting, persisting or
 * streaming through an `account:*` route while a previously signed grant is
 * still inside its five-minute TTL.
 *
 * Shape mirrors `localModels.ts` verbatim: pure functions over the persisted
 * membership signals (`OAuthTokens.membershipActive` +
 * `membershipPlanCode`), five machine-readable reasons, unknown states fail
 * closed, and a membership refresh self-heals. The floor is delegated to
 * `meetsCustomModelTier` — one Plus (wire code BASIC) floor and one ops
 * rollback lever for every paid-feature gate. No dedicated env of its own:
 * account routes are chosen in the TUI, and a
 * webview cannot read `process.env`, so a private lever would gate the two
 * surfaces differently.
 *
 * Fail-soft interplay: `membership.ts` deliberately keeps the last-known paid
 * state when the membership endpoint errors, so a gateway incident does not
 * lock paying members out here. That is intentional — the authoritative
 * refusal is the unsigned grant, not this predicate.
 */
import {
  CUSTOM_MODEL_MIN_TIER_DISPLAY_NAME,
  meetsCustomModelTier,
} from './planTier.js'

/**
 * Display name of the Account Bridge floor. Aliases the shared custom-model
 * floor constant rather than restating it, so UI copy can never drift from the
 * predicate that actually decides.
 */
export const ACCOUNT_BRIDGE_MIN_TIER_DISPLAY_NAME =
  CUSTOM_MODEL_MIN_TIER_DISPLAY_NAME

/** Machine-readable explanation of an Account Bridge entitlement decision. */
export type AccountBridgeEntitlementReason =
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

export interface AccountBridgeEntitlement {
  /** True when the account may connect / select / run `account:*` routes. */
  allowed: boolean
  /** Why the decision was reached, for diagnostics and UI messaging. */
  reason: AccountBridgeEntitlementReason
}

export interface AccountBridgeEntitlementInput {
  /**
   * Authoritative Acosmi membership flag (`getMembership().hasActive`).
   * `null` / `undefined` → unknown → fail-closed. See
   * {@link LocalModelEntitlementInput} for the full state table.
   */
  membershipActive?: boolean | null
  /**
   * Persisted `getMembership().planCode` (`OAuthTokens.membershipPlanCode`).
   * Only consulted when `membershipActive === true`.
   */
  membershipPlanCode?: string | null
}

/**
 * Resolve the Account Bridge entitlement for an account. Pure function over
 * the membership signals; every blocked path carries a distinct reason so
 * callers can word the upgrade path without re-deriving cause.
 */
export function resolveAccountBridgeEntitlement(
  input: AccountBridgeEntitlementInput = {},
): AccountBridgeEntitlement {
  if (input.membershipActive === true) {
    const meets = meetsCustomModelTier(input.membershipPlanCode)
    if (meets === true) {
      return { allowed: true, reason: 'paid-subscription' }
    }
    if (meets === false) {
      return { allowed: false, reason: 'tier-too-low' }
    }
    // Plan code missing/unregistered → fail-closed (裁决① 2026-07-04).
    return { allowed: false, reason: 'unknown-tier' }
  }
  if (input.membershipActive === false) {
    return { allowed: false, reason: 'free-tier' }
  }
  // null / undefined → unknown membership → fail-closed.
  return { allowed: false, reason: 'no-subscription' }
}

/** Thin boolean wrapper for callers that only need yes/no. */
export function canUseAccountBridge(
  input: AccountBridgeEntitlementInput = {},
): boolean {
  return resolveAccountBridgeEntitlement(input).allowed
}

/**
 * Human-facing blocked message. One place so every gated surface (TUI
 * directory, worker errors) words the upgrade path identically. Names the
 * floor through the shared display constant, never a literal.
 */
export function accountBridgeBlockedDetail(
  reason: AccountBridgeEntitlementReason,
): string {
  const base = `账户接入属于付费功能（${ACCOUNT_BRIDGE_MIN_TIER_DISPLAY_NAME} 会员及以上可用）。`
  switch (reason) {
    case 'no-subscription':
      return `${base}当前登录态没有会员信息 — 请先登录 Acosmi 账号；已登录则稍后重试（会员信息刷新中）。`
    case 'free-tier':
      return `${base}当前账号为免费档 — 升级会员后即可使用（设置 → 账户 → 升级）。`
    case 'tier-too-low':
      return `${base}当前会员档位低于 ${ACCOUNT_BRIDGE_MIN_TIER_DISPLAY_NAME} — 升级档位后即可使用。`
    case 'unknown-tier':
      return `${base}会员档位信息暂不可用（安全起见先锁定）— 重新打开应用或重新登录后自愈。`
    case 'paid-subscription':
      return base
  }
}
