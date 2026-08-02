import { createFallbackStorage } from './fallbackStorage.js'
import type {
  SecureStorage,
  SecureStorageData,
  SecureStorageMutator,
  SecureStorageWriteResult,
} from './types.js'

/**
 * Mutate the primary/fallback credential record and, only when the primary
 * backend committed, scrub the legacy plaintext auth projection. The caller
 * must hold the shared secure-storage mutation lock.
 *
 * Kept in a leaf module so contract tests can inject fake backends without
 * importing (and pinning) the platform storage singleton used by other tests.
 */
export async function mutatePrimaryWithPlaintextCleanup(
  primary: SecureStorage,
  plaintext: SecureStorage,
  mutation: SecureStorageMutator,
  plaintextCleanup: (current: SecureStorageData) => SecureStorageData,
): Promise<SecureStorageWriteResult> {
  let writePath: 'primary' | 'secondary' | 'failed' | undefined
  const storage = createFallbackStorage(primary, plaintext, {
    onWritePath: path => {
      writePath = path
    },
  })
  const result = await storage.mutateAsync(mutation)
  if (!result.success || writePath !== 'primary') return result

  const cleanup = await plaintext.mutateAsync(plaintextCleanup)
  if (!cleanup.success) {
    return {
      success: false,
      committed: true,
      warning: 'secure storage committed but plaintext auth cleanup failed',
    }
  }
  return result
}
