import { join } from 'path'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'
import { getErrnoCode } from '../errors.js'
import { writeFileSyncAtomicNoFallback } from '../file.js'
import { getFsImplementation } from '../fsOperations.js'
import { latNow, latTrace } from '../latencyTrace.js'
import {
  jsonParse,
  jsonStringify,
} from '../slowOperations.js'
import type {
  SecureStorage,
  SecureStorageData,
  SecureStorageMutator,
  SecureStorageReadStatus,
  SecureStorageWriteResult,
} from './types.js'

function getStoragePath(): { storageDir: string; storagePath: string } {
  const storageDir = getCrabCodeConfigHomeDir()
  const storageFileName = '.credentials.json'
  return { storageDir, storagePath: join(storageDir, storageFileName) }
}

const NOOP = (): void => {}

/**
 * Process-level FIFO queue backing `mutateAsync` on the Linux direct path
 * (RFC §5). On darwin the plaintext backend is the fallback's secondary and
 * its `mutateAsync` is never called — `createFallbackStorage` owns the
 * serialisation there — so this queue stays dormant.
 */
let plainTextWriteQueue: Promise<unknown> = Promise.resolve()

// On Darwin this is the `fallbackStorage`
// secondary; on Linux it is the only backend. Each method emits a
// `[lat][ts] secureStorage.plaintext.*` line under `CRABCODE_LATENCY_TRACE=1`
// so a reader can see the file-IO cost of the fallback write path.
export const plainTextStorage = {
  name: 'plaintext',
  read(): SecureStorageData | null {
    // sync IO: called from sync context (SecureStorage interface)
    const start = latNow()
    const { storagePath } = getStoragePath()
    try {
      const data = getFsImplementation().readFileSync(storagePath, {
        encoding: 'utf8',
      })
      const parsed = jsonParse(data)
      latTrace('secureStorage.plaintext.read', {
        result: 'data',
        dur_ms: latNow() - start,
      })
      return parsed
    } catch {
      latTrace('secureStorage.plaintext.read', {
        result: 'null',
        dur_ms: latNow() - start,
      })
      return null
    }
  },
  async readAsync(): Promise<SecureStorageData | null> {
    const start = latNow()
    const { storagePath } = getStoragePath()
    try {
      const data = await getFsImplementation().readFile(storagePath, {
        encoding: 'utf8',
      })
      const parsed = jsonParse(data)
      latTrace('secureStorage.plaintext.readAsync', {
        result: 'data',
        dur_ms: latNow() - start,
      })
      return parsed
    } catch {
      latTrace('secureStorage.plaintext.readAsync', {
        result: 'null',
        dur_ms: latNow() - start,
      })
      return null
    }
  },
  async readStatusAsync(): Promise<SecureStorageReadStatus> {
    const { storagePath } = getStoragePath()
    try {
      const data = await getFsImplementation().readFile(storagePath, {
        encoding: 'utf8',
      })
      const parsed = jsonParse(data)
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
        return { status: 'error' }
      }
      return { status: 'data', data: parsed as SecureStorageData }
    } catch (error) {
      return getErrnoCode(error) === 'ENOENT'
        ? { status: 'absent' }
        : { status: 'error' }
    }
  },
  update(data: SecureStorageData): { success: boolean; warning?: string } {
    // sync IO: called from sync context (SecureStorage interface)
    const start = latNow()
    try {
      const { storageDir, storagePath } = getStoragePath()
      try {
        getFsImplementation().mkdirSync(storageDir)
      } catch (e: unknown) {
        const code = getErrnoCode(e)
        if (code !== 'EEXIST') {
          throw e
        }
      }

      writeFileSyncAtomicNoFallback(storagePath, jsonStringify(data), {
        encoding: 'utf8',
        mode: 0o600,
      })
      latTrace('secureStorage.plaintext.update', {
        success: true,
        dur_ms: latNow() - start,
      })
      return {
        success: true,
        warning: 'Warning: Storing credentials in plaintext.',
      }
    } catch {
      latTrace('secureStorage.plaintext.update', {
        success: false,
        dur_ms: latNow() - start,
      })
      return { success: false }
    }
  },
  async updateAsync(
    data: SecureStorageData,
  ): Promise<SecureStorageWriteResult> {
    const start = latNow()
    try {
      const { storageDir, storagePath } = getStoragePath()
      // `mkdir` is async (recursive — no EEXIST to handle). The credential
      // file write itself is a few-KB local op (sub-millisecond) — not the
      // subprocess hazard RFC §1.2 targets — and the mockable FsOperations
      // layer exposes no async writeFile, so the write stays sync.
      await getFsImplementation().mkdir(storageDir)
      writeFileSyncAtomicNoFallback(storagePath, jsonStringify(data), {
        encoding: 'utf8',
        mode: 0o600,
      })
      latTrace('secureStorage.plaintext.updateAsync', {
        success: true,
        dur_ms: latNow() - start,
      })
      return {
        success: true,
        warning: 'Warning: Storing credentials in plaintext.',
      }
    } catch {
      latTrace('secureStorage.plaintext.updateAsync', {
        success: false,
        dur_ms: latNow() - start,
      })
      return { success: false }
    }
  },
  mutateAsync(fn: SecureStorageMutator): Promise<SecureStorageWriteResult> {
    // Linux direct path (RFC §5): a single backend has no shadowing risk, so
    // no generation stamp — but writes are still serialised so two concurrent
    // mutators cannot lose an update.
    const runCritical = async (): Promise<SecureStorageWriteResult> => {
      const read = await plainTextStorage.readStatusAsync()
      if (read.status === 'error') {
        return {
          success: false,
          warning: 'secure storage authoritative read unavailable',
        }
      }
      const current = read.status === 'data' ? read.data : {}
      const next = await fn(current)
      return plainTextStorage.updateAsync(next)
    }
    const result = plainTextWriteQueue.then(runCritical, runCritical)
    plainTextWriteQueue = result.then(NOOP, NOOP)
    return result
  },
  delete(): boolean {
    // sync IO: called from sync context (SecureStorage interface)
    const start = latNow()
    const { storagePath } = getStoragePath()
    try {
      getFsImplementation().unlinkSync(storagePath)
      latTrace('secureStorage.plaintext.delete', {
        success: true,
        dur_ms: latNow() - start,
      })
      return true
    } catch (e: unknown) {
      const code = getErrnoCode(e)
      if (code === 'ENOENT') {
        latTrace('secureStorage.plaintext.delete', {
          success: true,
          result: 'enoent',
          dur_ms: latNow() - start,
        })
        return true
      }
      latTrace('secureStorage.plaintext.delete', {
        success: false,
        dur_ms: latNow() - start,
      })
      return false
    }
  },
  async deleteAsync(): Promise<boolean> {
    const start = latNow()
    const { storagePath } = getStoragePath()
    try {
      await getFsImplementation().unlink(storagePath)
      latTrace('secureStorage.plaintext.deleteAsync', {
        success: true,
        dur_ms: latNow() - start,
      })
      return true
    } catch (e: unknown) {
      const code = getErrnoCode(e)
      if (code === 'ENOENT') {
        latTrace('secureStorage.plaintext.deleteAsync', {
          success: true,
          result: 'enoent',
          dur_ms: latNow() - start,
        })
        return true
      }
      latTrace('secureStorage.plaintext.deleteAsync', {
        success: false,
        dur_ms: latNow() - start,
      })
      return false
    }
  },
} satisfies SecureStorage
