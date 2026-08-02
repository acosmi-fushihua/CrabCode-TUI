/**
 * Types for the OAuth service layer.
 */

export type BillingType = 'usage' | 'subscription' | string

export type ReferralCampaign = string

export interface ReferralEligibilityResponse {
  eligible: boolean
  reason?: string
  campaign?: string
  referral_url?: string
  referral_code_details?: {
    referral_link?: string
    campaign?: string
  }
  referrer_reward?: ReferrerRewardInfo
}

export interface ReferrerRewardInfo {
  amount_cents?: number
  currency?: string
  credit_amount?: number
  description?: string
}

export interface ReferralRedemptionsResponse {
  redemptions: Array<{
    id: string
    redeemed_at: string
    reward?: ReferrerRewardInfo
  }>
  limit?: number
}

export interface OAuthProfileResponse {
  id: string
  uuid?: string
  display_name?: string
  email?: string
  avatar_url?: string
  image_url?: string
  picture?: string
  account: {
    uuid: string
    email: string
    display_name?: string
    avatar_url?: string
    image_url?: string
    picture?: string
    created_at?: string
    has_max?: boolean
    has_pro?: boolean
    // V116.1 P0-3 (2026-07-24): nexus desktop_account.go 契约字段 —— true 表示
    // 账号未绑定手机号(注册赠送权益领取的前置)。老网关不下发,absence ≠ false,
    // 消费方只允许 `=== true` 判断。
    requires_phone_binding?: boolean
  }
  organization: {
    uuid: string
    has_extra_usage_enabled?: boolean
    billing_type?: string
    subscription_created_at?: string
    organization_type?: string
    rate_limit_tier?: string
  }
}

export interface OAuthTokenExchangeResponse {
  access_token: string
  refresh_token?: string
  expires_in?: number
  token_type?: string
  scope?: string
  account?: {
    uuid: string
    email_address: string
    display_name?: string
    avatar_url?: string
    image_url?: string
  }
  organization?: {
    uuid?: string
  }
}

export interface UserRolesResponse {
  roles: string[]
  organization_role?: string
  workspace_role?: string
  organization_name?: string
}

/**
 * Subscription tier for CrabCode.ai accounts.
 */
export type SubscriptionType =
  | 'free'
  | 'pro'
  | 'max'
  | 'team'
  | 'enterprise'

/**
 * Subscription tiers that count as paid. 'free' is intentionally excluded:
 * free users have a null org type, not "free". This is the single source of
 * truth for "is this a paying account"; do not copy a second list elsewhere.
 */
export const VALID_SUBSCRIPTION_TYPES: readonly SubscriptionType[] = ['pro', 'max', 'team', 'enterprise']

/**
 * Normalize org type string to a clean SubscriptionType.
 * SDK v0.8.0+ handles vendor-prefix stripping server-side; raw values reaching
 * here should already be plain. 'free' is intentionally excluded — free users
 * have null org type, not "free". Downstream null-guards depend on this.
 */
export function normalizeOrgType(raw: string | undefined | null): SubscriptionType | null {
  if (!raw) return null
  return VALID_SUBSCRIPTION_TYPES.includes(raw as SubscriptionType)
    ? (raw as SubscriptionType)
    : null
}

/**
 * Check account-level subscription flag, handling both current and legacy API field names.
 * API may return `has_max` (new) or legacy prefixed form — this checks both.
 */
export function hasSubscriptionFlag(
  account: Record<string, unknown>,
  tier: 'max' | 'pro',
): boolean {
  return !!account[`has_${tier}`]
}

/**
 * Rate-limit tier that controls how aggressively requests are throttled.
 */
export type RateLimitTier = string

/**
 * OAuth tokens returned from the CrabCode.ai authorization server.
 * `refreshToken` and `expiresAt` are null for inference-only tokens
 * obtained via environment variables (CRABCODE_OAUTH_TOKEN).
 */
export interface OAuthTokens {
  accessToken: string
  refreshToken: string | null
  expiresAt: number | null
  scopes: string[]
  /** SDK refresh metadata persisted by the Go bridge token store. */
  clientId?: string
  serverUrl?: string
  subscriptionType: SubscriptionType | null
  rateLimitTier: RateLimitTier | null
  /**
   * Authoritative Acosmi membership flag — `getMembership().hasActive`.
   * `true` = active paid member, `false` = free tier, `null` = unknown
   * (inference-only token, or not yet fetched). This is the single signal the
   * local/custom-model entitlement gate reads (see utils/entitlements/*); it is
   * deliberately separate from `subscriptionType` (legacy CrabClaw tier
   * taxonomy consumed by unrelated tier-granular features). Populated by
   * `syncMembershipActive()` and preserved across token refresh.
   */
  membershipActive: boolean | null
  /**
   * Acosmi membership plan code/name — `getMembership().planCode`/`planName`
   * (e.g. `PRO_MAX`, `ULTRA`). Display-layer only (the /usage tier label);
   * entitlement gating reads `membershipActive` exclusively, never these.
   * Display MUST gate on `membershipActive === true` before rendering a plan.
   * `null`/absent = unknown — persisted writes never downgrade a stored value
   * (same fail-soft contract as `membershipActive`).
   */
  membershipPlanCode?: string | null
  membershipPlanName?: string | null
  /** Optional pre-fetched profile data from token exchange */
  profile?: {
    account: {
      uuid: string
      email: string
      display_name?: string
      avatar_url?: string
      image_url?: string
      picture?: string
      created_at?: string
      // V116.1 P0-3:与 OAuthProfileResponse.account 同源字段(老网关不下发)
      requires_phone_binding?: boolean
    }
    organization: {
      uuid: string
      has_extra_usage_enabled?: boolean
      billing_type?: string
      subscription_created_at?: string
    }
    avatar_url?: string
    image_url?: string
    picture?: string
  }
  /** Optional account data from token exchange */
  tokenAccount?: {
    uuid: string
    emailAddress: string
    organizationUuid: string
    displayName?: string
    avatarUrl?: string
    imageUrl?: string
  }
}
