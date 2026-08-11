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
import type { SecureStorageWriteResult } from '../../utils/secureStorage/types.js'
import { clearToolSchemaCache } from '../../utils/toolSchemaCache.js'
import { resetUserCache } from '../../utils/user.js'

/**
 * A closed signal that the authoritative secure-storage mutation committed,
 * but a later cache/config/plaintext cleanup step failed. Callers must not
 * infer this state from error text: only this class can turn a rejected
 * cleanup promise into a truthful Acosmi-account-removed terminal state.
 */
export class AcosmiAccountRemovalCommittedCleanupError extends Error {
  readonly accountRemovalCommitted = true as const
  readonly cleanupCause: unknown

  constructor(message: string, cleanupCause: unknown) {
    super(message)
    this.name = 'AcosmiAccountRemovalCommittedCleanupError'
    this.cleanupCause = cleanupCause
  }
}

function cleanupErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

export type AcosmiAccountRemovalAuthorities = {
  removeAccountStorage: () => Promise<SecureStorageWriteResult>
  removeApiKeyStorage: () => Promise<void>
  finishCommittedRemoval: (
    accountStorageResult: SecureStorageWriteResult,
  ) => Promise<void>
}

/**
 * Serialize the two credential authorities around an explicit commit point.
 * The OAuth/device record is removed first. Therefore an uncommitted secure
 * storage failure has not touched the API-key authority, while every failure
 * after that commit carries a typed account-removal truth signal.
 */
export async function clearAcosmiAccountAuthorities(
  authorities: AcosmiAccountRemovalAuthorities,
): Promise<void> {
  const accountStorageResult = await authorities.removeAccountStorage()
  if (!accountStorageResult.success && !accountStorageResult.committed) {
    throw new Error(
      accountStorageResult.warning ??
        'Failed to clear local account credentials',
    )
  }

  try {
    await authorities.removeApiKeyStorage()
    await authorities.finishCommittedRemoval(accountStorageResult)
  } catch (error) {
    throw new AcosmiAccountRemovalCommittedCleanupError(
      cleanupErrorMessage(error),
      error,
    )
  }
}

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

  // Logout owns only Acosmi account/device state. Preserve user-owned MCP,
  // plugin and custom-model credentials (plus unknown future siblings).
  const secureStorage = getSecureStorage()
  await clearAcosmiAccountAuthorities({
    removeAccountStorage: async () =>
      process.platform === 'darwin'
        ? await (
            await import('../../utils/secureStorage/index.js')
          ).mutateSecureStorageWithPrimaryPlaintextCleanup(
            stripAcosmiAccountCredentials,
            stripAcosmiAccountCredentials,
          )
        : await secureStorage.mutateAsync(stripAcosmiAccountCredentials),
    removeApiKeyStorage: removeApiKey,
    finishCommittedRemoval: async clearResult => {
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
      // post-commit. The secure account authority already changed even if a
      // secondary-plaintext cleanup did not finish.
      if (!clearResult.success) {
        throw new Error(
          clearResult.warning ??
            'Failed to finish local account cleanup',
        )
      }
    },
  })
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
