// OAuth client for handling authentication flows with CrabCode services
import axios from 'axios'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'
import {
  ALL_OAUTH_SCOPES,
  ACOSMI_INFERENCE_SCOPE,
  getOauthConfig,
} from '../../constants/oauth.js'
import {
  checkAndRefreshOAuthTokenIfNeeded,
  getAcosmiOAuthTokens,
  getAuthTokenSource,
  hasProfileScope,
  isAcosmiSubscriber,
  saveApiKey,
} from '../../utils/auth.js'
import { redactSecrets } from '../../utils/redactSecrets.js'
import type { AccountInfo } from '../../utils/config.js'
import { getGlobalConfig, saveGlobalConfig } from '../../utils/config.js'
import { logForDebugging } from '../../utils/debug.js'
import { getOauthProfileFromOauthToken } from './getOauthProfile.js'
import { deriveAccountInfoFromOAuthTokenClaims } from './tokenClaims.js'
import type {
  BillingType,
  OAuthProfileResponse,
  OAuthTokenExchangeResponse,
  OAuthTokens,
  RateLimitTier,
  SubscriptionType,
  UserRolesResponse,
} from './types.js'
import { normalizeOrgType } from './types.js'

/**
 * Check if the user has Acosmi authentication scope
 * @private Only call this if you're OAuth / auth related code!
 */
export function shouldUseAcosmiAuth(scopes: string[] | undefined): boolean {
  return Boolean(scopes?.includes(ACOSMI_INFERENCE_SCOPE))
}

export function parseScopes(scopeString?: string): string[] {
  return scopeString?.split(' ').filter(Boolean) ?? []
}

export function buildAuthUrl({
  codeChallenge,
  state,
  port,
  isManual,
  authorizationEndpoint,
  clientId,
  loginWithAcosmi,
  inferenceOnly,
  orgUUID,
  loginHint,
  loginMethod,
}: {
  codeChallenge: string
  state: string
  port: number
  isManual: boolean
  authorizationEndpoint: string
  clientId: string
  loginWithAcosmi?: boolean
  inferenceOnly?: boolean
  orgUUID?: string
  loginHint?: string
  loginMethod?: string
}): string {
  const authUrl = new URL(authorizationEndpoint)
  authUrl.searchParams.append('client_id', clientId)
  authUrl.searchParams.append('response_type', 'code')
  authUrl.searchParams.append(
    'redirect_uri',
    isManual
      ? getOauthConfig().MANUAL_REDIRECT_URL
      : `http://127.0.0.1:${port}/callback`,
  )
  const scopesToUse = inferenceOnly
    ? [ACOSMI_INFERENCE_SCOPE]
    : ALL_OAUTH_SCOPES
  authUrl.searchParams.append('scope', scopesToUse.join(' '))
  authUrl.searchParams.append('code_challenge', codeChallenge)
  authUrl.searchParams.append('code_challenge_method', 'S256')
  authUrl.searchParams.append('state', state)

  if (orgUUID) {
    authUrl.searchParams.append('orgUUID', orgUUID)
  }
  if (loginHint) {
    authUrl.searchParams.append('login_hint', loginHint)
  }
  if (loginMethod) {
    authUrl.searchParams.append('login_method', loginMethod)
  }

  return authUrl.toString()
}

/**
 * Renew credentials already present in local stores. The SDK is the sole
 * refresh-token rotator; this function only converts its TokenSet back into
 * the existing secure-storage shape and refreshes profile metadata.
 */
export async function renewStoredOAuthTokens(
  currentAccessToken?: string,
): Promise<OAuthTokens> {
  try {
    const { renewAcosmiTokenSet } = await import('../acosmi/client.js')
    const tokenSet = await renewAcosmiTokenSet(currentAccessToken)
    const expiresAtMs = Date.parse(tokenSet.expires_at ?? '')
    return await finalizeTokenResponse(
      {
        access_token: tokenSet.access_token,
        refresh_token: tokenSet.refresh_token || undefined,
        expires_in: Number.isFinite(expiresAtMs)
          ? Math.max(60, Math.round((expiresAtMs - Date.now()) / 1000))
          : 60,
        scope: tokenSet.scope || undefined,
      },
      tokenSet.refresh_token ?? '',
    )
  } catch (error) {
    logTokenRefreshFailure(error)
    throw error
  }
}

async function finalizeTokenResponse(
  data: OAuthTokenExchangeResponse,
  fallbackRefreshToken: string,
): Promise<OAuthTokens> {
  const {
    access_token: accessToken,
    refresh_token: newRefreshToken = fallbackRefreshToken,
    expires_in: expiresIn,
  } = data

  const expiresAt = Date.now() + (expiresIn ?? 3600) * 1000
  logEvent('tengu_oauth_token_refresh_success', {})

  // Skip the extra /api/oauth/profile round-trip when we already have both
  // the global-config profile fields AND the secure-storage subscription data.
  // Routine refreshes satisfy both, so we cut ~7M req/day fleet-wide.
  //
  // Checking secure storage (not just config) matters for the
  // CRABCODE_OAUTH_REFRESH_TOKEN re-login path: installOAuthTokens clears
  // local auth state AFTER we return, wiping secure storage. If we returned
  // null for subscriptionType here, saveOAuthTokensIfNeeded would persist
  // null ?? (wiped) ?? null = null, and every future refresh would see the
  // config guard fields satisfied and skip again, permanently losing the
  // subscription type for paying users. By passing through existing values,
  // the re-login path writes cached ?? wiped ?? null = cached; and if secure
  // storage was already empty we fall through to the fetch.
  const config = getGlobalConfig()
  const existing = getAcosmiOAuthTokens()
  const parsedScopes = parseScopes(data.scope)
  const scopes =
    parsedScopes.length > 0 ? parsedScopes : (existing?.scopes ?? [])
  const haveProfileAlready =
    config.oauthAccount?.billingType !== undefined &&
    config.oauthAccount?.accountCreatedAt !== undefined &&
    config.oauthAccount?.subscriptionCreatedAt !== undefined &&
    existing?.subscriptionType != null &&
    existing?.rateLimitTier != null

  const profileInfo = haveProfileAlready
    ? null
    : await fetchProfileInfo(accessToken)

  // Update the stored properties if they have changed
  if (profileInfo && config.oauthAccount) {
    const updates: Partial<AccountInfo> = {}
    if (profileInfo.displayName !== undefined) {
      updates.displayName = profileInfo.displayName
    }
    if (profileInfo.avatarUrl !== undefined) {
      updates.avatarUrl = profileInfo.avatarUrl
    }
    if (profileInfo.imageUrl !== undefined) {
      updates.imageUrl = profileInfo.imageUrl
    }
    if (typeof profileInfo.hasExtraUsageEnabled === 'boolean') {
      updates.hasExtraUsageEnabled = profileInfo.hasExtraUsageEnabled
    }
    if (profileInfo.billingType !== null) {
      updates.billingType = profileInfo.billingType
    }
    if (profileInfo.accountCreatedAt !== undefined) {
      updates.accountCreatedAt = profileInfo.accountCreatedAt
    }
    if (profileInfo.subscriptionCreatedAt !== undefined) {
      updates.subscriptionCreatedAt = profileInfo.subscriptionCreatedAt
    }
    // V116.1 P0-3: 布尔才写入 —— true→false 的解除态也要随 refresh 传播
    // (用户完成绑定后无需重登即可解除预检拦截);null(老网关)不动现值。
    if (typeof profileInfo.requiresPhoneBinding === 'boolean') {
      updates.requiresPhoneBinding = profileInfo.requiresPhoneBinding
    }
    if (Object.keys(updates).length > 0) {
      saveGlobalConfig(current => ({
        ...current,
        oauthAccount: current.oauthAccount
          ? { ...current.oauthAccount, ...updates }
          : current.oauthAccount,
      }))
    }
  }

  return {
    accessToken,
    refreshToken: newRefreshToken,
    expiresAt,
    scopes,
    subscriptionType:
      profileInfo?.subscriptionType ?? existing?.subscriptionType ?? null,
    rateLimitTier:
      profileInfo?.rateLimitTier ?? existing?.rateLimitTier ?? null,
    // Membership is account state, not token state — it does not change on
    // refresh. Pass through the stored value (syncMembershipActive owns
    // populating it); saveOAuthTokensIfNeeded also guards against clobber.
    membershipActive: existing?.membershipActive ?? null,
    profile: profileInfo?.rawProfile,
    tokenAccount: data.account
      ? {
          uuid: data.account.uuid,
          emailAddress: data.account.email_address,
          organizationUuid: data.organization?.uuid ?? '',
          displayName: data.account.display_name,
          avatarUrl: data.account.avatar_url,
          imageUrl: data.account.image_url,
        }
      : undefined,
  }
}

function logTokenRefreshFailure(error: unknown): void {
  const responseBody =
    axios.isAxiosError(error) && error.response?.data
      ? redactSecrets(JSON.stringify(error.response.data))
      : undefined
  logEvent('tengu_oauth_token_refresh_failure', {
    error: (error as Error)
      .message as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    ...(responseBody && {
      responseBody:
        responseBody as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    }),
  })
}

export async function fetchAndStoreUserRoles(
  accessToken: string,
): Promise<void> {
  const response = await axios.get(getOauthConfig().ROLES_URL, {
    headers: { Authorization: `Bearer ${accessToken}` },
    timeout: 10000,
  })

  if (response.status !== 200) {
    throw new Error(`Failed to fetch user roles: ${response.statusText}`)
  }
  const data = response.data as UserRolesResponse
  const config = getGlobalConfig()

  if (!config.oauthAccount) {
    throw new Error('OAuth account information not found in config')
  }

  saveGlobalConfig(current => ({
    ...current,
    oauthAccount: current.oauthAccount
      ? {
          ...current.oauthAccount,
          organizationRole: data.organization_role,
          workspaceRole: data.workspace_role,
          organizationName: data.organization_name,
        }
      : current.oauthAccount,
  }))

  logEvent('tengu_oauth_roles_stored', {
    org_role:
      data.organization_role as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })
}

export async function createAndStoreApiKey(
  accessToken: string,
): Promise<string | null> {
  try {
    // timeout matches the sibling OAuth POSTs. This call is awaited on the login path
    // (cli/handlers/auth.ts), so without a timeout a slow/hung proxy blocks
    // login completion indefinitely (axios.defaults.timeout is 0 = unbounded).
    const response = await axios.post(getOauthConfig().API_KEY_URL, null, {
      headers: { Authorization: `Bearer ${accessToken}` },
      timeout: 15000,
    })

    const apiKey = response.data?.raw_key
    if (apiKey) {
      await saveApiKey(apiKey)
      logEvent('tengu_oauth_api_key', {
        status:
          'success' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        statusCode: response.status,
      })
      return apiKey
    }
    return null
  } catch (error) {
    logEvent('tengu_oauth_api_key', {
      status:
        'failure' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      error: (error instanceof Error
        ? error.message
        : String(
            error,
          )) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    })
    throw error
  }
}

export function isOAuthTokenExpired(expiresAt: number | null): boolean {
  if (expiresAt === null) {
    return false
  }

  const bufferTime = 5 * 60 * 1000
  const now = Date.now()
  const expiresWithBuffer = now + bufferTime
  return expiresWithBuffer >= expiresAt
}

export async function fetchProfileInfo(accessToken: string): Promise<{
  subscriptionType: SubscriptionType | null
  displayName?: string
  avatarUrl?: string
  imageUrl?: string
  rateLimitTier: RateLimitTier | null
  hasExtraUsageEnabled: boolean | null
  billingType: BillingType | null
  // V116.1 P0-3: null = 老网关未下发该字段(不可当 false 消费)
  requiresPhoneBinding: boolean | null
  accountCreatedAt?: string
  subscriptionCreatedAt?: string
  rawProfile?: OAuthProfileResponse
}> {
  const profile = await getOauthProfileFromOauthToken(accessToken)
  const orgType = profile?.organization?.organization_type
  const subscriptionType = normalizeOrgType(orgType)

  const result: {
    subscriptionType: SubscriptionType | null
    displayName?: string
    avatarUrl?: string
    imageUrl?: string
    rateLimitTier: RateLimitTier | null
    hasExtraUsageEnabled: boolean | null
    billingType: BillingType | null
    requiresPhoneBinding: boolean | null
    accountCreatedAt?: string
    subscriptionCreatedAt?: string
  } = {
    subscriptionType,
    rateLimitTier: profile?.organization?.rate_limit_tier ?? null,
    hasExtraUsageEnabled:
      profile?.organization?.has_extra_usage_enabled ?? null,
    billingType: profile?.organization?.billing_type ?? null,
    requiresPhoneBinding: profile?.account?.requires_phone_binding ?? null,
  }

  if (profile?.account?.display_name) {
    result.displayName = profile.account.display_name
  }

  const avatarUrl =
    profile?.account?.avatar_url ?? profile?.avatar_url ?? profile?.account?.picture ?? profile?.picture
  if (avatarUrl && /^https?:\/\//i.test(avatarUrl)) {
    result.avatarUrl = avatarUrl
  }

  const imageUrl = profile?.account?.image_url ?? profile?.image_url
  if (imageUrl && /^https?:\/\//i.test(imageUrl)) {
    result.imageUrl = imageUrl
  }

  if (profile?.account?.created_at) {
    result.accountCreatedAt = profile.account.created_at
  }

  if (profile?.organization?.subscription_created_at) {
    result.subscriptionCreatedAt = profile.organization.subscription_created_at
  }

  logEvent('tengu_oauth_profile_fetch_success', {})

  return { ...result, rawProfile: profile }
}

/**
 * Gets the organization UUID from the OAuth access token
 * @returns The organization UUID or null if not authenticated
 */
export async function getOrganizationUUID(): Promise<string | null> {
  // Check global config first to avoid unnecessary API call
  const globalConfig = getGlobalConfig()
  const orgUUID = globalConfig.oauthAccount?.organizationUuid
  if (orgUUID) {
    return orgUUID
  }

  // Fall back to fetching from profile (requires user:profile scope)
  const accessToken = getAcosmiOAuthTokens()?.accessToken
  if (accessToken === undefined || !hasProfileScope()) {
    return null
  }
  const profile = await getOauthProfileFromOauthToken(accessToken)
  const profileOrgUUID = profile?.organization?.uuid
  if (!profileOrgUUID) {
    return null
  }
  return profileOrgUUID
}

/**
 * Populate the OAuth account info if it has not already been cached in config.
 * @returns Whether or not the oauth account info was populated.
 */
export async function populateOAuthAccountInfoIfNeeded(): Promise<boolean> {
  // Check env vars first (synchronous, no network call needed).
  // SDK callers like Cowork can provide account info directly, which also
  // eliminates the race condition where early telemetry events lack account info.
  // NB: If/when adding additional SDK-relevant functionality requiring _other_ OAuth account properties,
  // please reach out to #proj-cowork so the team can add additional env var fallbacks.
  const envAccountUuid = process.env.CRABCODE_ACCOUNT_UUID
  const envUserEmail = process.env.CRABCODE_USER_EMAIL
  const envOrganizationUuid = process.env.CRABCODE_ORGANIZATION_UUID
  const hasEnvVars = Boolean(
    envAccountUuid && envUserEmail && envOrganizationUuid,
  )
  if (envAccountUuid && envUserEmail && envOrganizationUuid) {
    if (!getGlobalConfig().oauthAccount) {
      storeOAuthAccountInfo({
        accountUuid: envAccountUuid,
        emailAddress: envUserEmail,
        organizationUuid: envOrganizationUuid,
      })
    }
  }

  // Wait for any in-flight token refresh to complete first; the SDK refresh
  // path also fetches and stores profile info.
  await checkAndRefreshOAuthTokenIfNeeded()

  const config = getGlobalConfig()
  const tokens = getAcosmiOAuthTokens()
  const canUseManagedTokenClaims =
    getAuthTokenSource().source === 'acosmi.com' &&
    Boolean(tokens?.accessToken && shouldUseAcosmiAuth(tokens.scopes))
  if (
    (config.oauthAccount &&
      config.oauthAccount.billingType !== undefined &&
      config.oauthAccount.accountCreatedAt !== undefined &&
      config.oauthAccount.subscriptionCreatedAt !== undefined) ||
    (!isAcosmiSubscriber() && !canUseManagedTokenClaims) ||
    (!hasProfileScope() && !canUseManagedTokenClaims)
  ) {
    return false
  }

  if (tokens?.accessToken) {
    const profile = hasProfileScope()
      ? await getOauthProfileFromOauthToken(tokens.accessToken)
      : undefined
    if (profile) {
      if (hasEnvVars) {
        logForDebugging(
          'OAuth profile fetch succeeded, overriding env var account info',
          { level: 'info' },
        )
      }
      storeOAuthAccountInfo({
        accountUuid: profile.account.uuid,
        emailAddress: profile.account.email,
        organizationUuid: profile.organization.uuid,
        displayName: profile.account.display_name || undefined,
        avatarUrl:
          profile.account.avatar_url ??
          profile.avatar_url ??
          profile.account.picture ??
          profile.picture,
        imageUrl: profile.account.image_url ?? profile.image_url,
        hasExtraUsageEnabled:
          profile.organization.has_extra_usage_enabled ?? false,
        billingType: profile.organization.billing_type ?? undefined,
        accountCreatedAt: profile.account.created_at,
        subscriptionCreatedAt:
          profile.organization.subscription_created_at ?? undefined,
        // V116.1 P0-3: 老网关不下发时保持 undefined(消费方仅 `=== true` 拦截)
        requiresPhoneBinding: profile.account.requires_phone_binding,
      })
      return true
    }
    if (canUseManagedTokenClaims) {
      const accountInfo = deriveAccountInfoFromOAuthTokenClaims(tokens.accessToken)
      if (accountInfo) {
        storeOAuthAccountInfo({
          ...accountInfo,
          organizationUuid: accountInfo.organizationUuid,
        })
        return true
      }
    }
  }
  return false
}

export function storeOAuthAccountInfo({
  accountUuid,
  emailAddress,
  organizationUuid,
  displayName,
  avatarUrl,
  imageUrl,
  hasExtraUsageEnabled,
  billingType,
  accountCreatedAt,
  subscriptionCreatedAt,
  requiresPhoneBinding,
}: {
  accountUuid: string
  emailAddress: string
  organizationUuid: string | undefined
  displayName?: string
  avatarUrl?: string
  imageUrl?: string
  hasExtraUsageEnabled?: boolean
  billingType?: BillingType | null
  accountCreatedAt?: string
  subscriptionCreatedAt?: string
  // V116.1 P0-3:老网关不下发时 undefined(消费方仅 `=== true` 拦截)
  requiresPhoneBinding?: boolean
}): void {
  const accountInfo: AccountInfo = {
    accountUuid,
    emailAddress,
    organizationUuid,
    hasExtraUsageEnabled,
    billingType,
    accountCreatedAt,
    subscriptionCreatedAt,
    requiresPhoneBinding,
  }
  if (displayName) {
    accountInfo.displayName = displayName
  }
  if (avatarUrl && /^https?:\/\//i.test(avatarUrl)) {
    accountInfo.avatarUrl = avatarUrl
  }
  if (imageUrl && /^https?:\/\//i.test(imageUrl)) {
    accountInfo.imageUrl = imageUrl
  }
  saveGlobalConfig(current => {
    // For oauthAccount we need to compare content since it's an object
    if (
      current.oauthAccount?.accountUuid === accountInfo.accountUuid &&
      current.oauthAccount?.emailAddress === accountInfo.emailAddress &&
      current.oauthAccount?.organizationUuid === accountInfo.organizationUuid &&
      current.oauthAccount?.displayName === accountInfo.displayName &&
      current.oauthAccount?.avatarUrl === accountInfo.avatarUrl &&
      current.oauthAccount?.imageUrl === accountInfo.imageUrl &&
      current.oauthAccount?.hasExtraUsageEnabled ===
        accountInfo.hasExtraUsageEnabled &&
      current.oauthAccount?.billingType === accountInfo.billingType &&
      current.oauthAccount?.accountCreatedAt === accountInfo.accountCreatedAt &&
      current.oauthAccount?.subscriptionCreatedAt ===
        accountInfo.subscriptionCreatedAt
    ) {
      return current
    }
    return { ...current, oauthAccount: accountInfo }
  })
}
