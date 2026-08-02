import { latNow, latTrace } from '../latencyTrace.js'
import type {
  SecureStorage,
  SecureStorageData,
  SecureStorageMutator,
  SecureStorageReadStatus,
  SecureStorageWriteResult,
} from './types.js'

/**
 * Two-backend secure storage: a primary (on darwin, the macOS Keychain) with a
 * plaintext secondary as fallback.
 *
 * This layer uses generation stamps and a degradation latch:
 *
 *  - §3 generation stamp — every write stamps a monotonic `gen` into a reserved
 *    `__crabcode_storage_meta` key; reads consult BOTH backends and return the
 *    higher-`gen` record. This structurally closes #30337: a stale primary
 *    entry left behind by a failed keychain write can no longer shadow the
 *    fresh credentials written to the secondary, because the fresh write
 *    carries a strictly higher generation. The reserved key is stamped on
 *    write and stripped on read, so callers never observe it.
 *  - §3 degrade latch — once a write falls through to the secondary, `read()`
 *    skips the (known-failing) primary entirely until a primary write succeeds
 *    again. A pure latency optimisation inside the degraded window.
 *  - §3 error containment — a backend that throws is treated as "returned
 *    null / failed"; the error never escapes this layer.
 *  - §2 mutateAsync — a process-level FIFO serialised read-modify-write, so a
 *    caller changing a subset of keys cannot be clobbered by a concurrent
 *    writer (lost-update race).
 *
 * The previous best-effort `primary.delete()` cleanup is kept but, per RFC
 * §3.2(d), is now only an optimisation: correctness no longer depends on it.
 *
 * Latency tracing (T2-P0-TRACE): every method emits a `[lat][ts]
 * secureStorage.fallback.*` line under `CRABCODE_LATENCY_TRACE=1`. Off by
 * default; adds one `latNow()` + a self-guarded `latTrace()` per call.
 */

/** Reserved key carrying the generation stamp. Stamped on write, stripped on
 *  read — callers never see it. RFC §3.2(a). */
const STORAGE_META_KEY = '__crabcode_storage_meta'

interface StorageMeta {
  gen: number
}

// --- process-level state -----------------------------------------------------

/**
 * Process-level degrade latch (RFC §3.2(c)). Set when a write falls through to
 * the secondary (primary write failed); cleared when a primary write succeeds.
 * While set, `read()` / `readAsync()` skip the primary backend.
 */
let primaryDegraded = false

/**
 * Strictly-monotonic generation counter (RFC §3.2(a)). Tracks the wall clock
 * (`Date.now() * 1000`) when it advances, and increments by 1 within the same
 * millisecond — so it is monotonic in-process and roughly comparable across
 * processes (last writer by wall clock wins).
 */
let lastGen = 0

function nextGen(): number {
  const wall = Date.now() * 1000
  lastGen = wall > lastGen ? wall : lastGen + 1
  return lastGen
}

function observeGeneration(...records: Array<SecureStorageData | null>): void {
  for (const record of records) {
    lastGen = Math.max(lastGen, genOf(record))
  }
}

/**
 * Process-level FIFO queue for `mutateAsync` (RFC §2.2). The full credential
 * record is a single shared resource, so one process-wide chain suffices. The
 * queue itself never rejects — failures surface through each call's own
 * returned promise.
 */
let writeQueue: Promise<unknown> = Promise.resolve()

const NOOP = (): void => {}

/**
 * Reset the process-level latch and write queue. Test-only — the production
 * code has no reason to reset these. The generation counter is deliberately
 * NOT reset: it is a process-lifetime monotonic counter by design.
 */
export function __resetFallbackStorageForTests__(): void {
  primaryDegraded = false
  writeQueue = Promise.resolve()
}

/**
 * Called only after the shared cross-process credential lock is held. A
 * failure latched in this process may be stale after another process repaired
 * the primary, so the next mutation must consult both backends again.
 */
export function prepareFallbackStorageForCrossProcessMutation(): void {
  primaryDegraded = false
}

/** Reset the degrade latch for one explicit requested-handle miss refresh. */
export function prepareFallbackStorageForCrossProcessFreshRead(): void {
  primaryDegraded = false
}

/** Test-only read of the degrade latch. */
export function __isPrimaryDegradedForTests__(): boolean {
  return primaryDegraded
}

// --- generation meta helpers -------------------------------------------------

/** Stamp a fresh generation onto a copy of `data`. Any caller-supplied meta is
 *  overwritten — the reserved key is owned by this layer (RFC §8). */
function stampGeneration(data: SecureStorageData): SecureStorageData {
  return { ...data, [STORAGE_META_KEY]: { gen: nextGen() } satisfies StorageMeta }
}

/** Extract the generation from a record. Missing / malformed meta ⇒ -1, so a
 *  legacy unstamped record always loses to any stamped write (RFC §3.2(f)). */
function genOf(data: SecureStorageData | null): number {
  if (data === null) return -1
  const meta = data[STORAGE_META_KEY]
  if (
    meta !== null &&
    typeof meta === 'object' &&
    typeof (meta as { gen?: unknown }).gen === 'number'
  ) {
    return (meta as StorageMeta).gen
  }
  return -1
}

/** Return `data` without the reserved meta key. Returns the input untouched
 *  when no meta is present (legacy / Linux records), else a shallow copy. */
function stripMeta(data: SecureStorageData): SecureStorageData {
  if (!(STORAGE_META_KEY in data)) return data
  const { [STORAGE_META_KEY]: _omit, ...rest } = data
  return rest
}

/**
 * Pick the winning record by generation: the strictly-higher `gen` wins; a tie
 * (including two unstamped legacy records, both gen -1) prefers the primary,
 * preserving the pre-T4 primary-first behaviour. The chosen record is returned
 * meta-stripped.
 */
function pickHigherGen(
  pri: SecureStorageData | null,
  sec: SecureStorageData | null,
): { data: SecureStorageData; source: 'primary' | 'secondary' | 'empty' } {
  observeGeneration(pri, sec)
  if (pri === null && sec === null) return { data: {}, source: 'empty' }
  if (pri === null) return { data: stripMeta(sec as SecureStorageData), source: 'secondary' }
  if (sec === null) return { data: stripMeta(pri), source: 'primary' }
  if (genOf(sec) > genOf(pri)) return { data: stripMeta(sec), source: 'secondary' }
  return { data: stripMeta(pri), source: 'primary' }
}

// --- error-contained backend calls (RFC §3.2(e)) -----------------------------

function safeRead(backend: SecureStorage): SecureStorageData | null {
  try {
    return backend.read()
  } catch {
    return null
  }
}

async function safeReadAsync(
  backend: SecureStorage,
): Promise<SecureStorageData | null> {
  try {
    return await backend.readAsync()
  } catch {
    return null
  }
}

async function safeReadStatusAsync(
  backend: SecureStorage,
): Promise<SecureStorageReadStatus> {
  try {
    if (backend.readStatusAsync) return await backend.readStatusAsync()
    const data = await backend.readAsync()
    return data === null ? { status: 'absent' } : { status: 'data', data }
  } catch {
    return { status: 'error' }
  }
}

function dataFromReadStatus(
  result: Exclude<SecureStorageReadStatus, { status: 'error' }>,
): SecureStorageData | null {
  return result.status === 'data' ? result.data : null
}

function safeUpdate(
  backend: SecureStorage,
  data: SecureStorageData,
): SecureStorageWriteResult {
  try {
    return backend.update(data)
  } catch {
    return { success: false }
  }
}

async function safeUpdateAsync(
  backend: SecureStorage,
  data: SecureStorageData,
): Promise<SecureStorageWriteResult> {
  try {
    return await backend.updateAsync(data)
  } catch {
    return { success: false }
  }
}

function safeDelete(backend: SecureStorage): boolean {
  try {
    return backend.delete()
  } catch {
    return false
  }
}

async function safeDeleteAsync(backend: SecureStorage): Promise<boolean> {
  try {
    return await backend.deleteAsync()
  } catch {
    return false
  }
}

/**
 * Creates a fallback storage that consults both backends, reconciling them by
 * generation stamp so a stale primary can never shadow a fresh secondary.
 */
export function createFallbackStorage(
  primary: SecureStorage,
  secondary: SecureStorage,
  options: {
    onWritePath?: (path: 'primary' | 'secondary' | 'failed') => void
  } = {},
): SecureStorage {
  // -- reads: consult both backends, return the higher-generation record -----

  function read(): SecureStorageData {
    const start = latNow()
    const sec = safeRead(secondary)
    // Skip the (known-failing) primary while degraded — its entry is stale and
    // would lose the generation comparison anyway. RFC §3.2(c).
    const pri = primaryDegraded ? null : safeRead(primary)
    const picked = pickHigherGen(pri, sec)
    latTrace('secureStorage.fallback.read', {
      source: picked.source,
      degraded: primaryDegraded,
      dur_ms: latNow() - start,
    })
    return picked.data
  }

  async function readAsync(): Promise<SecureStorageData> {
    const start = latNow()
    const [sec, pri] = await Promise.all([
      safeReadAsync(secondary),
      primaryDegraded ? Promise.resolve(null) : safeReadAsync(primary),
    ])
    const picked = pickHigherGen(pri, sec)
    latTrace('secureStorage.fallback.readAsync', {
      source: picked.source,
      degraded: primaryDegraded,
      dur_ms: latNow() - start,
    })
    return picked.data
  }

  // -- writes: stamp a fresh generation, then primary-first with secondary ---
  //    fallback. The migration delete (#1414) and stale-cleanup delete
  //    (#30337) are best-effort optimisations only — RFC §3.2(d).

  function update(data: SecureStorageData): SecureStorageWriteResult {
    const start = latNow()
    // Read-before-write: distinguishes a first migration (#1414) from a normal
    // overwrite, and a stale-shadow risk (#30337) from a clean primary.
    const primaryBefore = safeRead(primary)
    const secondaryBefore = safeRead(secondary)
    observeGeneration(primaryBefore, secondaryBefore)
    const stamped = stampGeneration(data)

    const result = safeUpdate(primary, stamped)
    if (result.success) {
      primaryDegraded = false
      let migrateDelete = false
      if (primaryBefore === null) {
        // First migration to primary — drop the secondary copy so a shared
        // .crabcode dir between host and container does not keep a stale one.
        migrateDelete = true
        safeDelete(secondary)
      }
      latTrace('secureStorage.fallback.update', {
        path: 'primary',
        migrateDelete,
        dur_ms: latNow() - start,
      })
      options.onWritePath?.('primary')
      return result
    }

    const fallbackResult = safeUpdate(secondary, stamped)
    if (fallbackResult.success) {
      primaryDegraded = true
      // Best-effort: clear the now-stale primary entry. Correctness no longer
      // depends on this — the fresh secondary carries a higher generation, so
      // a leftover stale primary loses the read() comparison regardless.
      let staleDelete: 'attempted' | 'skipped' = 'skipped'
      let staleDeleteOk = false
      if (primaryBefore !== null) {
        staleDelete = 'attempted'
        staleDeleteOk = safeDelete(primary)
      }
      latTrace('secureStorage.fallback.update', {
        path: 'secondary',
        staleDelete,
        staleDeleteOk,
        degraded: true,
        dur_ms: latNow() - start,
      })
      options.onWritePath?.('secondary')
      return { success: true, warning: fallbackResult.warning }
    }

    latTrace('secureStorage.fallback.update', {
      path: 'failed',
      dur_ms: latNow() - start,
    })
    options.onWritePath?.('failed')
    return { success: false }
  }

  async function commitUpdateAsyncFromSnapshot(
    data: SecureStorageData,
    primaryBefore: SecureStorageData | null,
    secondaryBefore: SecureStorageData | null,
    start: number,
  ): Promise<SecureStorageWriteResult> {
    observeGeneration(primaryBefore, secondaryBefore)
    const stamped = stampGeneration(data)

    const result = await safeUpdateAsync(primary, stamped)
    if (result.success) {
      primaryDegraded = false
      let migrateDelete = false
      if (primaryBefore === null) {
        migrateDelete = true
        await safeDeleteAsync(secondary)
      }
      latTrace('secureStorage.fallback.updateAsync', {
        path: 'primary',
        migrateDelete,
        dur_ms: latNow() - start,
      })
      options.onWritePath?.('primary')
      return result
    }

    const fallbackResult = await safeUpdateAsync(secondary, stamped)
    if (fallbackResult.success) {
      primaryDegraded = true
      let staleDelete: 'attempted' | 'skipped' = 'skipped'
      let staleDeleteOk = false
      if (primaryBefore !== null) {
        staleDelete = 'attempted'
        staleDeleteOk = await safeDeleteAsync(primary)
      }
      latTrace('secureStorage.fallback.updateAsync', {
        path: 'secondary',
        staleDelete,
        staleDeleteOk,
        degraded: true,
        dur_ms: latNow() - start,
      })
      options.onWritePath?.('secondary')
      return { success: true, warning: fallbackResult.warning }
    }

    latTrace('secureStorage.fallback.updateAsync', {
      path: 'failed',
      dur_ms: latNow() - start,
    })
    options.onWritePath?.('failed')
    return { success: false }
  }

  async function updateAsync(
    data: SecureStorageData,
  ): Promise<SecureStorageWriteResult> {
    const start = latNow()
    const [primaryBefore, secondaryBefore] = await Promise.all([
      safeReadAsync(primary),
      safeReadAsync(secondary),
    ])
    return commitUpdateAsyncFromSnapshot(
      data,
      primaryBefore,
      secondaryBefore,
      start,
    )
  }

  // -- mutateAsync: serialised read-modify-write (RFC §2.2) ------------------

  function mutateAsync(
    fn: SecureStorageMutator,
  ): Promise<SecureStorageWriteResult> {
    const enqueued = latNow()
    const runCritical = async (): Promise<SecureStorageWriteResult> => {
      const start = latNow()
      // A full-record mutator may proceed only after both backends were read
      // authoritatively. Treating a transient primary failure as "absent"
      // would let a stale plaintext mirror overwrite newer Keychain siblings.
      const [primaryStatus, secondaryStatus] = await Promise.all([
        safeReadStatusAsync(primary),
        safeReadStatusAsync(secondary),
      ])
      if (
        primaryStatus.status === 'error' ||
        secondaryStatus.status === 'error'
      ) {
        latTrace('secureStorage.fallback.mutateAsync', {
          success: false,
          read_error: true,
          queued_ms: start - enqueued,
          dur_ms: latNow() - start,
        })
        return {
          success: false,
          warning: 'secure storage authoritative read unavailable',
        }
      }
      const primaryBefore = dataFromReadStatus(primaryStatus)
      const secondaryBefore = dataFromReadStatus(secondaryStatus)
      const current = pickHigherGen(primaryBefore, secondaryBefore).data
      const next = await fn(current)
      // Commit from the exact snapshot passed to fn. The outer cross-process
      // mutex prevents another CrabCode mutation from interleaving here.
      const result = await commitUpdateAsyncFromSnapshot(
        next,
        primaryBefore,
        secondaryBefore,
        start,
      )
      latTrace('secureStorage.fallback.mutateAsync', {
        success: result.success,
        queued_ms: start - enqueued,
        dur_ms: latNow() - start,
      })
      return result
    }
    // Chain regardless of whether the previous entry settled or rejected, and
    // keep the queue itself non-rejecting.
    const result = writeQueue.then(runCritical, runCritical)
    writeQueue = result.then(NOOP, NOOP)
    return result
  }

  // -- deletes ---------------------------------------------------------------

  function deleteSync(): boolean {
    const start = latNow()
    const primaryOk = safeDelete(primary)
    const secondaryOk = safeDelete(secondary)
    latTrace('secureStorage.fallback.delete', {
      primaryOk,
      secondaryOk,
      dur_ms: latNow() - start,
    })
    return primaryOk || secondaryOk
  }

  async function deleteAsync(): Promise<boolean> {
    const start = latNow()
    const [primaryOk, secondaryOk] = await Promise.all([
      safeDeleteAsync(primary),
      safeDeleteAsync(secondary),
    ])
    latTrace('secureStorage.fallback.deleteAsync', {
      primaryOk,
      secondaryOk,
      dur_ms: latNow() - start,
    })
    return primaryOk || secondaryOk
  }

  return {
    name: `${primary.name}-with-${secondary.name}-fallback`,
    read,
    readAsync,
    update,
    updateAsync,
    mutateAsync,
    delete: deleteSync,
    deleteAsync,
  }
}
