import {
  createFallbackStorage,
  prepareFallbackStorageForCrossProcessMutation,
} from './fallbackStorage.js'
import { clearKeychainCache } from './macOsKeychainHelpers.js'
import { macOsKeychainStorage } from './macOsKeychainStorage.js'
import { plainTextStorage } from './plainTextStorage.js'
import { mutatePrimaryWithPlaintextCleanup } from './primaryPlaintextCleanup.js'
import {
  withCrossProcessResourceLock,
  withCrossProcessResourceLockSync,
} from '../crossProcessResourceLock.js'
import type {
  SecureStorage,
  SecureStorageData,
  SecureStorageMutator,
  SecureStorageReadStatus,
  SecureStorageWriteResult,
} from './types.js'

const LOCK_FAILURE: SecureStorageWriteResult = {
  success: false,
  warning: 'secure storage mutation lock unavailable',
}

/**
 * The credential record is a single read-modify-write resource shared by TUI
 * processes. Backend queues are process-local, so wrap every
 * mutating entry point in the same cross-process mutex. Reads stay lock-free:
 * keychain generations and the plaintext parser already fail closed, while
 * adding filesystem locking to every sync credential read would regress hot
 * startup paths.
 */
export function createCrossProcessLockedSecureStorage(
  storage: SecureStorage,
  prepareMutation: () => void = () => {},
): SecureStorage {
  return {
    name: storage.name,
    read: () => storage.read(),
    readAsync: () => storage.readAsync(),
    async readStatusAsync(): Promise<SecureStorageReadStatus> {
      if (storage.readStatusAsync) return storage.readStatusAsync()
      const data = await storage.readAsync()
      return data === null ? { status: 'absent' } : { status: 'data', data }
    },
    update(data) {
      try {
        return withCrossProcessResourceLockSync(
          'secure-storage-mutation',
          () => {
            prepareMutation()
            return storage.update(data)
          },
        )
      } catch {
        return LOCK_FAILURE
      }
    },
    async updateAsync(data) {
      try {
        return await withCrossProcessResourceLock(
          'secure-storage-mutation',
          () => {
            prepareMutation()
            return storage.updateAsync(data)
          },
        )
      } catch {
        return LOCK_FAILURE
      }
    },
    async mutateAsync(fn) {
      try {
        return await withCrossProcessResourceLock(
          'secure-storage-mutation',
          () => {
            prepareMutation()
            return storage.mutateAsync(fn)
          },
        )
      } catch {
        return LOCK_FAILURE
      }
    },
    delete() {
      try {
        return withCrossProcessResourceLockSync(
          'secure-storage-mutation',
          () => {
            prepareMutation()
            return storage.delete()
          },
        )
      } catch {
        return false
      }
    },
    async deleteAsync() {
      try {
        return await withCrossProcessResourceLock(
          'secure-storage-mutation',
          () => {
            prepareMutation()
            return storage.deleteAsync()
          },
        )
      } catch {
        return false
      }
    },
  }
}

/**
 * Run one full-record mutation and, only when macOS Keychain was the committed
 * destination, scrub the legacy plaintext auth projection before releasing
 * the shared credential lock. This prevents both cross-process races and the
 * historical full-record Keychain→plaintext secret copy.
 */
export async function mutateSecureStorageWithPrimaryPlaintextCleanup(
  mutation: SecureStorageMutator,
  plaintextCleanup: (current: SecureStorageData) => SecureStorageData,
): Promise<SecureStorageWriteResult> {
  try {
    return await withCrossProcessResourceLock(
      'secure-storage-mutation',
      async () => {
        if (process.platform !== 'darwin') {
          return plainTextStorage.mutateAsync(mutation)
        }

        clearKeychainCache()
        prepareFallbackStorageForCrossProcessMutation()
        return mutatePrimaryWithPlaintextCleanup(
          macOsKeychainStorage,
          plainTextStorage,
          mutation,
          plaintextCleanup,
        )
      },
    )
  } catch {
    return LOCK_FAILURE
  }
}

/**
 * Get the appropriate secure storage implementation for the current platform
 */
export function getSecureStorage(): SecureStorage {
  if (process.platform === 'darwin') {
    return createCrossProcessLockedSecureStorage(
      createFallbackStorage(macOsKeychainStorage, plainTextStorage),
      () => {
        clearKeychainCache()
        prepareFallbackStorageForCrossProcessMutation()
      },
    )
  }

  // KNOWN_LIMITATION: Linux SecureStorage requires libsecret system dependency — 需要 libsecret 集成，跟踪于全局审计 G-10A

  return createCrossProcessLockedSecureStorage(plainTextStorage)
}
