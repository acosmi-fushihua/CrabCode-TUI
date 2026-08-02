import {
  withCrossProcessResourceLock,
} from '../crossProcessResourceLock.js'

export const CUSTOM_MODEL_CLEANUP_TIMEOUT_MS = 1_000

export type SecretCleanupOutcome = 'cleaned' | 'failed' | 'pending'
export type CustomModelSecretReferenceState =
  | 'referenced'
  | 'unreferenced'
  | 'unknown'

export function classifyCustomModelSecretReference(
  settings: unknown,
  handle: string,
): CustomModelSecretReferenceState {
  if (!settings || typeof settings !== 'object' || Array.isArray(settings)) {
    return 'unknown'
  }
  const record = settings as Record<string, unknown>
  const legacy = record.customModel
  if (
    legacy &&
    typeof legacy === 'object' &&
    !Array.isArray(legacy) &&
    (legacy as Record<string, unknown>).apiKeyHandle === handle
  ) {
    return 'referenced'
  }
  const registry = record.customModels
  if (Array.isArray(registry)) {
    for (const entry of registry) {
      if (
        entry &&
        typeof entry === 'object' &&
        !Array.isArray(entry) &&
        (entry as Record<string, unknown>).apiKeyHandle === handle
      ) {
        return 'referenced'
      }
    }
  }
  return 'unreferenced'
}

export async function settleCustomModelSecretCleanup(
  cleanup: () => Promise<void>,
  timeoutMs = CUSTOM_MODEL_CLEANUP_TIMEOUT_MS,
): Promise<SecretCleanupOutcome> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const settled = Promise.resolve()
    .then(cleanup)
    .then((): SecretCleanupOutcome => 'cleaned')
    .catch((): SecretCleanupOutcome => 'failed')
  const timedOut = new Promise<SecretCleanupOutcome>(resolve => {
    timer = setTimeout(() => resolve('pending'), timeoutMs)
  })
  const outcome = await Promise.race([settled, timedOut])
  if (timer) clearTimeout(timer)
  return outcome
}

export async function withCustomModelConfigTransaction<T>(
  operation: () => Promise<T>,
): Promise<T> {
  return withCrossProcessResourceLock('custom-model-config', operation)
}

export interface StagedCustomModelSecretTransaction<T> {
  readPreviousHandle: () => string | undefined
  stage: () => Promise<string>
  commit: (stagedHandle: string) => T | Promise<T>
  deleteSecret: (handle: string) => Promise<void>
  retireSecret: (handle: string) => Promise<void>
  verifyCommitAfterError?: (
    stagedHandle: string,
  ) => CustomModelSecretReferenceState | Promise<CustomModelSecretReferenceState>
  recoverValueAfterReferencedCommit?: (
    stagedHandle: string,
  ) => T | Promise<T>
  cleanupTimeoutMs?: number
}

export interface StagedCustomModelSecretResult<T> {
  value: T
  apiKeyHandle: string
  retiredSecret: 'retired' | 'failed' | 'pending' | 'not-needed'
}

/**
 * Cross-process copy-on-write transaction used by direct TUI/migration paths.
 * The settings publisher decides the durable commit point; secure cleanup is
 * compensation before commit and bounded retirement after commit.
 */
export async function commitStagedCustomModelSecret<T>(
  transaction: StagedCustomModelSecretTransaction<T>,
): Promise<StagedCustomModelSecretResult<T>> {
  return withCustomModelConfigTransaction(() =>
    commitStagedCustomModelSecretWithinTransaction(transaction),
  )
}

/**
 * Same protocol for callers that already own `custom-model-config`. This is
 * intentionally separate to avoid recursively acquiring a proper-lockfile
 * mutex during startup migration.
 */
export async function commitStagedCustomModelSecretWithinTransaction<T>(
  transaction: StagedCustomModelSecretTransaction<T>,
): Promise<StagedCustomModelSecretResult<T>> {
    const previousHandle = transaction.readPreviousHandle()
    const stagedHandle = await transaction.stage()
    let value: T
    try {
      value = await transaction.commit(stagedHandle)
    } catch (error) {
      let commitState: CustomModelSecretReferenceState = 'unknown'
      try {
        commitState =
          (await transaction.verifyCommitAfterError?.(stagedHandle)) ?? 'unknown'
      } catch {
        commitState = 'unknown'
      }

      if (commitState === 'referenced') {
        if (!transaction.recoverValueAfterReferencedCommit) {
          throw new Error(
            `${error instanceof Error ? error.message : String(error)}; settings commit was recovered but no result recovery was provided; staged secret retained`,
          )
        }
        value = await transaction.recoverValueAfterReferencedCommit(stagedHandle)
      } else if (commitState === 'unreferenced') {
        const rollback = await settleCustomModelSecretCleanup(
          () => transaction.deleteSecret(stagedHandle),
          transaction.cleanupTimeoutMs,
        )
        if (rollback !== 'cleaned') {
          throw new Error(
            `${error instanceof Error ? error.message : String(error)}; staged custom model secret cleanup ${rollback}`,
          )
        }
        throw error
      } else {
        throw new Error(
          `${error instanceof Error ? error.message : String(error)}; settings commit outcome unknown; staged custom model secret retained`,
        )
      }
    }

    let retiredSecret: StagedCustomModelSecretResult<T>['retiredSecret'] =
      'not-needed'
    if (previousHandle && previousHandle !== stagedHandle) {
      // Persist the retirement intent instead of deleting inline. Another
      // process may still hold the prior settings generation; a restart-safe,
      // reference-scanning GC reclaims only after the grace deadline.
      const retirement = await settleCustomModelSecretCleanup(
        () => transaction.retireSecret(previousHandle),
        transaction.cleanupTimeoutMs,
      )
      retiredSecret = retirement === 'cleaned' ? 'retired' : retirement
    }
    return { value, apiKeyHandle: stagedHandle, retiredSecret }
}
