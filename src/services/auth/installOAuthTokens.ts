import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import { t } from '../../i18n/index.js'
import { fetchAndStoreCrabCodeFirstTokenDate } from '../api/firstTokenDate.js'
import { enforceDeviceAccountCap } from '../device/enforceDeviceCap.js'
import {
  createAndStoreApiKey,
  fetchAndStoreUserRoles,
  storeOAuthAccountInfo,
} from '../oauth/client.js'
import { getOauthProfileFromOauthToken } from '../oauth/getOauthProfile.js'
import { syncMembershipActive } from '../oauth/membership.js'
import { deriveAccountInfoFromOAuthTokenClaims } from '../oauth/tokenClaims.js'
import type { OAuthTokens } from '../oauth/types.js'
import {
  clearOAuthTokenCache,
  saveOAuthTokensIfNeeded,
} from '../../utils/auth.js'
import { logForDebugging } from '../../utils/debug.js'
import { latNow, latTrace } from '../../utils/latencyTrace.js'
import { invalidateCredentialsCache } from '../../utils/model/providers.js'
import type { SecureStorageWriteResult } from '../../utils/secureStorage/types.js'
import {
  AcosmiAccountRemovalCommittedCleanupError,
  clearAuthRelatedCaches,
  clearLocalAuthState,
} from './localAuthState.js'
import { shouldUseAcosmiAuth } from '../oauth/client.js'

/**
 * Token replacement first removes the prior account. A typed rejection after
 * that secure-storage commit is a cleanup warning, not evidence that the old
 * account survived; continuing lets the new authoritative token replace it.
 * Every uncommitted or merely structural lookalike error still fails closed.
 */
export async function clearPriorAuthForOAuthInstall(
  clearPriorAuth: () => Promise<void> = () =>
    clearLocalAuthState({
      clearOnboarding: false,
      flushTelemetry: true,
    }),
): Promise<void> {
  try {
    await clearPriorAuth()
  } catch (error) {
    if (!(error instanceof AcosmiAccountRemovalCommittedCleanupError)) {
      throw error
    }
    logForDebugging(
      `[auth] prior Acosmi account removal committed before OAuth install; continuing after cleanup warning: ${error.message}`,
      { level: 'warn' },
    )
  }
}

/**
 * Shared post-token-acquisition backend operation.
 *
 * It intentionally lives outside CLI commands and React components so both
 * the ordinary CLI and the native direct TUI use one credential transaction.
 */
export async function installOAuthTokens(
  tokens: OAuthTokens,
): Promise<SecureStorageWriteResult> {
  const start = latNow()
  latTrace('installOAuthTokens.enter')
  if (shouldUseAcosmiAuth(tokens.scopes)) {
    if (
      typeof tokens.refreshToken !== 'string' ||
      tokens.refreshToken.trim().length === 0 ||
      typeof tokens.expiresAt !== 'number' ||
      !Number.isFinite(tokens.expiresAt) ||
      tokens.expiresAt <= 0
    ) {
      throw new Error(t('native_tui_auth_persistence_failed'))
    }
    await enforceDeviceAccountCap(tokens.accessToken)
  }

  const clearStart = latNow()
  await clearPriorAuthForOAuthInstall()
  latTrace('installOAuthTokens.clearLocalAuthState', {
    dur_ms: latNow() - clearStart,
  })

  const profileStart = latNow()
  const profile =
    tokens.profile ??
    (await getOauthProfileFromOauthToken(tokens.accessToken))
  latTrace('installOAuthTokens.profileFetch', {
    dur_ms: latNow() - profileStart,
    cached: !!tokens.profile,
  })
  if (profile) {
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
        profile.organization.has_extra_usage_enabled ?? undefined,
      billingType: profile.organization.billing_type ?? undefined,
      subscriptionCreatedAt:
        profile.organization.subscription_created_at ?? undefined,
      accountCreatedAt: profile.account.created_at,
    })
  } else if (tokens.tokenAccount) {
    storeOAuthAccountInfo({
      accountUuid: tokens.tokenAccount.uuid,
      emailAddress: tokens.tokenAccount.emailAddress,
      organizationUuid: tokens.tokenAccount.organizationUuid,
      displayName: tokens.tokenAccount.displayName,
      avatarUrl: tokens.tokenAccount.avatarUrl,
      imageUrl: tokens.tokenAccount.imageUrl,
    })
  } else {
    const accountInfo = deriveAccountInfoFromOAuthTokenClaims(
      tokens.accessToken,
    )
    if (accountInfo) {
      storeOAuthAccountInfo({
        ...accountInfo,
        organizationUuid: accountInfo.organizationUuid,
      })
    }
  }

  const saveStart = latNow()
  const storageResult = await saveOAuthTokensIfNeeded(tokens)
  if (!storageResult.success && !storageResult.committed) {
    throw new Error(t('native_tui_auth_persistence_failed'))
  }
  clearOAuthTokenCache()
  invalidateCredentialsCache()
  latTrace('installOAuthTokens.saveTokens', {
    dur_ms: latNow() - saveStart,
  })

  await syncMembershipActive()

  if (storageResult.warning) {
    logEvent('tengu_oauth_storage_warning', {
      warning:
        storageResult.warning as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    })
  }

  if (!shouldUseAcosmiAuth(tokens.scopes)) {
    const apiKeyStart = latNow()
    const apiKey = await createAndStoreApiKey(tokens.accessToken)
    latTrace('installOAuthTokens.createApiKey', {
      dur_ms: latNow() - apiKeyStart,
    })
    if (!apiKey) {
      throw new Error(
        'Unable to create API key. The server accepted the request but did not return a key.',
      )
    }
  }

  const cacheStart = latNow()
  await clearAuthRelatedCaches()
  latTrace('installOAuthTokens.clearAuthRelatedCaches', {
    dur_ms: latNow() - cacheStart,
  })
  latTrace('installOAuthTokens.done', { dur_ms: latNow() - start })
  triggerPostOAuthInstallWarmup(tokens)
  return storageResult
}

async function refreshPostLoginAuthMetadata(
  accessToken: string,
): Promise<void> {
  await fetchAndStoreUserRoles(accessToken).catch(err =>
    logForDebugging(String(err), { level: 'error' }),
  )
  await fetchAndStoreCrabCodeFirstTokenDate().catch(err =>
    logForDebugging(String(err), { level: 'error' }),
  )
}

function triggerPostOAuthInstallWarmup(tokens: OAuthTokens): void {
  void Promise.resolve()
    .then(async () => {
      const start = latNow()
      latTrace('installOAuthTokens.backgroundWarmup.enter')
      if (shouldUseAcosmiAuth(tokens.scopes)) {
        const metadataStart = latNow()
        await refreshPostLoginAuthMetadata(tokens.accessToken)
        latTrace('installOAuthTokens.backgroundWarmup.metadata', {
          dur_ms: latNow() - metadataStart,
        })
      }
      latTrace('installOAuthTokens.backgroundWarmup.done', {
        dur_ms: latNow() - start,
      })
    })
    .catch(err => {
      latTrace('installOAuthTokens.backgroundWarmup.error', {
        err: err instanceof Error ? err.message : String(err),
      })
      logForDebugging(
        `[auth] post-login auth warmup failed: ${
          err instanceof Error ? err.message : String(err)
        }`,
        { level: 'warn' },
      )
    })
}
