import {
  getGroveNoticeConfig,
  getGroveSettings,
} from '../api/grove.js'
import { clearPolicyLimitsCache } from '../policyLimits/index.js'
import { clearRemoteManagedSettingsCache } from '../remoteManagedSettings/index.js'
import {
  getAcosmiOAuthTokens,
  markAuthStateChanged,
  removeApiKey,
} from '../../utils/auth.js'
import { clearBetasCaches } from '../../utils/betas.js'
import { saveGlobalConfig } from '../../utils/config.js'
import { clearCachedCapabilities } from '../../utils/model/modelCapabilities.js'
import { clearValidModelCache } from '../../utils/model/validateModel.js'
import { getSecureStorage } from '../../utils/secureStorage/index.js'
import { stripAcosmiAccountCredentials } from '../../utils/secureStorage/authProjection.js'
import { clearToolSchemaCache } from '../../utils/toolSchemaCache.js'
import { resetUserCache } from '../../utils/user.js'

/**
 * Clear process-local and persisted Acosmi authentication state.
 *
 * This is deliberately renderer- and transport-independent: OAuth token
 * installation is a backend operation shared by ordinary CLI and the direct
 * native TUI runtime.
 */
export async function clearLocalAuthState({
  clearOnboarding = false,
  flushTelemetry = false,
}: {
  clearOnboarding?: boolean
  flushTelemetry?: boolean
} = {}): Promise<void> {
  if (flushTelemetry) {
    const { flushTelemetry: flush } = await import(
      '../../utils/telemetry/instrumentation.js'
    )
    await flush()
  }

  await removeApiKey()

  // Logout owns only Acosmi account/device state. Preserve user-owned MCP,
  // plugin and custom-model credentials (plus unknown future siblings).
  const secureStorage = getSecureStorage()
  const clearResult =
    process.platform === 'darwin'
      ? await (
          await import('../../utils/secureStorage/index.js')
        ).mutateSecureStorageWithPrimaryPlaintextCleanup(
          stripAcosmiAccountCredentials,
          stripAcosmiAccountCredentials,
        )
      : await secureStorage.mutateAsync(stripAcosmiAccountCredentials)
  const accountStateCommitted =
    clearResult.success || clearResult.committed
  if (!accountStateCommitted) {
    throw new Error(
      clearResult.warning ??
        'Failed to clear local account credentials',
    )
  }

  await markAuthStateChanged()
  await clearAuthRelatedCaches()
  saveGlobalConfig(current => {
    const updated = { ...current }
    if (clearOnboarding) {
      updated.hasCompletedOnboarding = false
      updated.subscriptionNoticeCount = 0
      updated.hasAvailableSubscription = false
      if (updated.customApiKeyResponses?.approved) {
        updated.customApiKeyResponses = {
          ...updated.customApiKeyResponses,
          approved: [],
        }
      }
    }
    updated.oauthAccount = undefined
    return updated
  })

  // A Keychain commit followed by plaintext cleanup failure is explicitly
  // post-commit: local caches/global projection were already cleared above.
  if (!clearResult.success) {
    throw new Error(
      clearResult.warning ??
        'Failed to finish local account cleanup',
    )
  }
}

/** Invalidate every cache whose value depends on the active account. */
export async function clearAuthRelatedCaches(): Promise<void> {
  getAcosmiOAuthTokens.cache?.clear?.()
  clearBetasCaches()
  clearToolSchemaCache()
  resetUserCache()
  getGroveNoticeConfig.cache?.clear?.()
  getGroveSettings.cache?.clear?.()
  await clearRemoteManagedSettingsCache()
  await clearPolicyLimitsCache()
  await clearCachedCapabilities()
  clearValidModelCache()
}
