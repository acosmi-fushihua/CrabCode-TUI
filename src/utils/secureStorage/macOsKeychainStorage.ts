import { execaSync } from 'execa'
import { logForDebugging } from '../debug.js'
import { latNow, latTrace } from '../latencyTrace.js'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { jsonParse, jsonStringify } from '../slowOperations.js'
import {
  assemblePayloadHex,
  computeBudgets,
  encodeManifestValue,
  isWriteSafeValue,
  MAX_CHUNKS,
  parseManifestValue,
  SECURITY_STDIN_LINE_LIMIT,
  splitPayloadHex,
} from './keychainChunking.js'
import {
  CREDENTIALS_SERVICE_SUFFIX,
  clearKeychainCache,
  getMacOsKeychainStorageServiceName,
  getUsername,
  KEYCHAIN_CACHE_TTL_MS,
  keychainCacheState,
} from './macOsKeychainHelpers.js'
import type {
  SecureStorage,
  SecureStorageData,
  SecureStorageMutator,
  SecureStorageReadStatus,
  SecureStorageWriteResult,
} from './types.js'

/**
 * macOS Keychain secure-storage backend.
 *
 * The write/read/delete paths use the keychainChunking.ts scheme so the
 * credentials payload is
 * always stored across one OR MORE keychain entries, each small enough that
 * its `add-generic-password` command line fits in the `security -i` stdin
 * line buffer. Consequence: EVERY write goes through `security -i` stdin and
 * the `add-generic-password ... -X <hexValue>` argv branch — which leaked
 * recoverable hex of the credentials to any `ps` observer — has been deleted.
 *
 * Storage layout (keychainChunking.ts):
 *   - base entry `<service>`   → manifest `v2:<nHex>:<lenHex>:<headHex>`
 *   - entry      `<service>#i` → raw hex slice i (i = 1 .. n-1)
 * The common small payload (account OAuth tokens) fits entirely in the
 * manifest `head` ⇒ n = 1 ⇒ one entry, one spawn — no behavior difference
 * from before. Multi-entry writes commit by writing the chunks first and the
 * manifest last; a torn write therefore degrades to "entry absent" and falls
 * through to the plaintext fallback rather than serving corrupt data.
 *
 * A legacy single-entry credentials record written by the pre-chunking code
 * is still readable — `parseManifestValue()` classifies a non-`v2:` value as
 * `legacy` and the value is parsed as the credentials JSON directly.
 */

const NOOP = (): void => {}

/**
 * Process-level FIFO queue backing `mutateAsync`. The keychain backend is
 * always wrapped by `createFallbackStorage` on darwin (which owns mutateAsync)
 * and unused on Linux, so this is exercised only for interface completeness.
 */
let keychainWriteQueue: Promise<unknown> = Promise.resolve()

/** Keychain service name for chunk entry #index (index >= 1). */
function chunkService(baseService: string, index: number): string {
  return `${baseService}#${index}`
}

// --- pure encode / decode (exported for tests; no `security` subprocess) -----

/**
 * Serialize a credentials record into the keychain entry values: a manifest
 * value for the base entry plus zero or more raw hex chunk values. Pure — the
 * inverse of `decodeEntries()`.
 */
export function encodeCredentialsToEntries(
  data: SecureStorageData,
  username: string,
  baseService: string,
): { manifestValue: string; chunkValues: string[]; jsonBytes: number } {
  const json = jsonStringify(data)
  const payloadHex = Buffer.from(json, 'utf-8').toString('hex')
  const { maxHead, maxChunk } = computeBudgets(username, baseService)
  const plan = splitPayloadHex(payloadHex, maxHead, maxChunk)
  return {
    manifestValue: encodeManifestValue(plan.n, payloadHex.length, plan.head),
    chunkValues: plan.chunks,
    jsonBytes: json.length,
  }
}

/**
 * Reconstruct a credentials record from a base-entry value plus the chunk
 * values it references. Returns null on a missing chunk, a chunk-count
 * mismatch, a corrupt manifest, or a payload that fails hex/JSON decoding —
 * i.e. a torn multi-chunk write reads back as "absent" (RFC §4). Pure — the
 * inverse of `encodeCredentialsToEntries()`.
 */
export function decodeEntries(
  baseValue: string,
  chunkValues: (string | null)[],
): SecureStorageData | null {
  const parsed = parseManifestValue(baseValue)
  if (parsed.kind === 'legacy') {
    // Pre-chunking single-entry record: the entry value IS the JSON.
    try {
      return jsonParse(baseValue)
    } catch {
      return null
    }
  }
  if (parsed.kind === 'corrupt') return null
  if (chunkValues.length !== parsed.n - 1) return null
  if (chunkValues.some(c => c === null || c === undefined)) return null
  const hex = assemblePayloadHex(
    parsed.head,
    chunkValues as string[],
    parsed.totalLen,
  )
  if (hex === null) return null
  try {
    return jsonParse(Buffer.from(hex, 'hex').toString('utf-8'))
  } catch {
    return null
  }
}

// --- `security` subprocess helpers -------------------------------------------

// A wedged Keychain UI/daemon must not hold the cross-process credential lock
// forever. Chunk writes run in parallel and the manifest is one final command,
// so a mutation remains below the interactive operation deadline.
const KEYCHAIN_COMMAND_TIMEOUT_MS = 3_000

/** Synchronously read one keychain entry's value, or null if absent/failed. */
function fetchEntrySync(service: string): string | null {
  try {
    const result = execaSync(
      'security',
      ['find-generic-password', '-a', getUsername(), '-w', '-s', service],
      {
        reject: false,
        stdio: ['ignore', 'pipe', 'pipe'],
        timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
      },
    )
    if (result.exitCode === 0 && typeof result.stdout === 'string') {
      const trimmed = result.stdout.trim()
      return trimmed.length > 0 ? trimmed : null
    }
  } catch (_e) {
    // fall through
  }
  return null
}

/** Asynchronously read one keychain entry's value, or null if absent/failed. */
async function fetchEntryAsync(service: string): Promise<string | null> {
  try {
    const { stdout, code } = await localExecBridge.execCommand({
      command: 'security',
      args: ['find-generic-password', '-a', getUsername(), '-w', '-s', service],
      preserveOutputOnError: false,
      timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
    })
    if (code === 0 && stdout) {
      const trimmed = stdout.trim()
      return trimmed.length > 0 ? trimmed : null
    }
  } catch (_e) {
    // fall through
  }
  return null
}

type KeychainEntryReadStatus =
  | { status: 'data'; value: string }
  | { status: 'absent' }
  | { status: 'error' }

/** Mutation-grade read: exit 44 is absent; every other failure is an error. */
async function fetchEntryStatusAsync(
  service: string,
): Promise<KeychainEntryReadStatus> {
  try {
    const { stdout, code } = await localExecBridge.execCommand({
      command: 'security',
      args: ['find-generic-password', '-a', getUsername(), '-w', '-s', service],
      preserveOutputOnError: false,
      timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
    })
    if (code === 44) return { status: 'absent' }
    if (code !== 0 || !stdout) return { status: 'error' }
    const value = stdout.trim()
    return value.length > 0
      ? { status: 'data', value }
      : { status: 'error' }
  } catch {
    return { status: 'error' }
  }
}

/**
 * Build the `add-generic-password` line for an entry. Fails closed if the
 * value is not escape-safe or the command would overflow the stdin line
 * buffer — a truncated command writes nothing but leaves the stale entry
 * intact (#30337), so refusing is strictly safer.
 */
function buildWriteCommand(service: string, value: string): string | null {
  if (!isWriteSafeValue(value)) {
    logForDebugging('[keychain] refusing to write non-escape-safe value', {
      level: 'warn',
    })
    return null
  }
  const username = getUsername()
  const command = `add-generic-password -U -a "${username}" -s "${service}" -w "${value}"\n`
  if (command.length > SECURITY_STDIN_LINE_LIMIT) {
    // computeBudgets() sizes chunks to keep every command under the limit;
    // reaching here means a budgeting bug — abort rather than corrupt.
    logForDebugging(
      `[keychain] write command (${command.length}B) exceeds stdin line limit; aborting`,
      { level: 'warn' },
    )
    return null
  }
  return command
}

/** Synchronously upsert one keychain entry via `security -i` stdin. */
function writeEntrySync(service: string, value: string): boolean {
  const command = buildWriteCommand(service, value)
  if (command === null) return false
  try {
    const result = execaSync('security', ['-i'], {
      input: command,
      stdio: ['pipe', 'pipe', 'pipe'],
      reject: false,
      timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
    })
    return result.exitCode === 0
  } catch (_e) {
    return false
  }
}

/** Asynchronously upsert one keychain entry via `security -i` stdin. */
async function writeEntryAsync(service: string, value: string): Promise<boolean> {
  const command = buildWriteCommand(service, value)
  if (command === null) return false
  try {
    const { code } = await localExecBridge.execCommand({
      command: 'security',
      args: ['-i'],
      stdin: 'pipe',
      input: command,
      preserveOutputOnError: false,
      timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
    })
    return code === 0
  } catch (_e) {
    return false
  }
}

/** Delete one keychain entry. `existed` = entry was present (exit 0); `ok` =
 *  the entry is now gone (exit 0 deleted, exit 44 already-absent). */
function deleteEntrySync(service: string): { existed: boolean; ok: boolean } {
  try {
    const result = execaSync(
      'security',
      ['delete-generic-password', '-a', getUsername(), '-s', service],
      {
        reject: false,
        stdio: ['ignore', 'pipe', 'pipe'],
        timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
      },
    )
    if (result.exitCode === 0) return { existed: true, ok: true }
    if (result.exitCode === 44) return { existed: false, ok: true }
    return { existed: false, ok: false }
  } catch (_e) {
    return { existed: false, ok: false }
  }
}

/** Async sibling of `deleteEntrySync`. */
async function deleteEntryAsync(
  service: string,
): Promise<{ existed: boolean; ok: boolean }> {
  try {
    const { code } = await localExecBridge.execCommand({
      command: 'security',
      args: ['delete-generic-password', '-a', getUsername(), '-s', service],
      preserveOutputOnError: false,
      timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
    })
    if (code === 0) return { existed: true, ok: true }
    if (code === 44) return { existed: false, ok: true }
    return { existed: false, ok: false }
  } catch (_e) {
    return { existed: false, ok: false }
  }
}

// --- assemble (read) / commit (write) ----------------------------------------

/** Read + reassemble the chunked credentials record synchronously. */
function readAndAssembleSync(): SecureStorageData | null {
  const baseService = getMacOsKeychainStorageServiceName(
    CREDENTIALS_SERVICE_SUFFIX,
  )
  const baseValue = fetchEntrySync(baseService)
  if (baseValue === null) return null
  const parsed = parseManifestValue(baseValue)
  if (parsed.kind !== 'v2' || parsed.n === 1) {
    return decodeEntries(baseValue, [])
  }
  const chunkValues: (string | null)[] = []
  for (let i = 1; i < parsed.n; i++) {
    chunkValues.push(fetchEntrySync(chunkService(baseService, i)))
  }
  return decodeEntries(baseValue, chunkValues)
}

/** Read + reassemble the chunked credentials record asynchronously (chunk
 *  reads run in parallel). */
async function readAndAssembleAsync(): Promise<SecureStorageData | null> {
  const baseService = getMacOsKeychainStorageServiceName(
    CREDENTIALS_SERVICE_SUFFIX,
  )
  const baseValue = await fetchEntryAsync(baseService)
  if (baseValue === null) return null
  const parsed = parseManifestValue(baseValue)
  if (parsed.kind !== 'v2' || parsed.n === 1) {
    return decodeEntries(baseValue, [])
  }
  const indices: number[] = []
  for (let i = 1; i < parsed.n; i++) indices.push(i)
  const chunkValues = await Promise.all(
    indices.map(i => fetchEntryAsync(chunkService(baseService, i))),
  )
  return decodeEntries(baseValue, chunkValues)
}

/** Fresh, non-cached read that preserves absent vs transient/corrupt error. */
async function readAndAssembleStatusAsync(): Promise<SecureStorageReadStatus> {
  const baseService = getMacOsKeychainStorageServiceName(
    CREDENTIALS_SERVICE_SUFFIX,
  )
  const base = await fetchEntryStatusAsync(baseService)
  if (base.status !== 'data') return base
  const parsed = parseManifestValue(base.value)
  if (parsed.kind === 'corrupt') return { status: 'error' }
  if (parsed.kind === 'legacy' || parsed.n === 1) {
    const data = decodeEntries(base.value, [])
    return data ? { status: 'data', data } : { status: 'error' }
  }
  const entries = await Promise.all(
    Array.from({ length: parsed.n - 1 }, (_, index) =>
      fetchEntryStatusAsync(chunkService(baseService, index + 1)),
    ),
  )
  if (entries.some(entry => entry.status !== 'data')) {
    // A base manifest that references an absent chunk is torn/corrupt, not an
    // absent credential record, and must never authorize an overwrite.
    return { status: 'error' }
  }
  const data = decodeEntries(
    base.value,
    entries.map(entry =>
      entry.status === 'data' ? entry.value : null,
    ),
  )
  return data ? { status: 'data', data } : { status: 'error' }
}

/** Commit a chunk plan synchronously: chunks #1..#(n-1) first, manifest last. */
function commitWriteSync(
  baseService: string,
  manifestValue: string,
  chunkValues: string[],
): boolean {
  for (let i = 0; i < chunkValues.length; i++) {
    if (!writeEntrySync(chunkService(baseService, i + 1), chunkValues[i])) {
      return false
    }
  }
  // The manifest (base entry) is the commit point — written last.
  return writeEntrySync(baseService, manifestValue)
}

/** Async sibling of `commitWriteSync` (chunk writes run in parallel). */
async function commitWriteAsync(
  baseService: string,
  manifestValue: string,
  chunkValues: string[],
): Promise<boolean> {
  const chunkResults = await Promise.all(
    chunkValues.map((value, idx) =>
      writeEntryAsync(chunkService(baseService, idx + 1), value),
    ),
  )
  if (chunkResults.some(ok => !ok)) return false
  return writeEntryAsync(baseService, manifestValue)
}

/**
 * Delete the base entry and sweep chunk entries #1, #2, … until the first
 * absent index. Chunk indices are contiguous, so the first gap ends the
 * sweep; this also reclaims orphan chunks left by an earlier shrinking write.
 */
function deleteSweepSync(baseService: string): {
  baseOk: boolean
  swept: number
} {
  const base = deleteEntrySync(baseService)
  let swept = 0
  for (let i = 1; i <= MAX_CHUNKS; i++) {
    const result = deleteEntrySync(chunkService(baseService, i))
    if (!result.existed) break
    swept++
  }
  return { baseOk: base.ok, swept }
}

/** Async sibling of `deleteSweepSync`. */
async function deleteSweepAsync(baseService: string): Promise<{
  baseOk: boolean
  swept: number
}> {
  const base = await deleteEntryAsync(baseService)
  let swept = 0
  for (let i = 1; i <= MAX_CHUNKS; i++) {
    const result = await deleteEntryAsync(chunkService(baseService, i))
    if (!result.existed) break
    swept++
  }
  return { baseOk: base.ok, swept }
}

// --- backend -----------------------------------------------------------------

export const macOsKeychainStorage = {
  name: 'keychain',
  read(): SecureStorageData | null {
    const prev = keychainCacheState.cache
    if (Date.now() - prev.cachedAt < KEYCHAIN_CACHE_TTL_MS) {
      // T2-P0-TRACE — cache hit is the no-spawn fast path.
      latTrace('secureStorage.keychain.read', { cache: 'hit' })
      return prev.data
    }

    // T2-P0-TRACE — cache miss: synchronous `security` subprocess spawn(s).
    const start = latNow()
    let data: SecureStorageData | null = null
    try {
      data = readAndAssembleSync()
    } catch (_e) {
      data = null
    }
    if (data !== null) {
      keychainCacheState.cache = { data, cachedAt: Date.now() }
      latTrace('secureStorage.keychain.read', {
        cache: 'miss',
        spawn: 'sync',
        dur_ms: latNow() - start,
        result: 'data',
      })
      return data
    }
    // Stale-while-error: if we had a value before and the refresh failed,
    // keep serving the stale value rather than caching null. Since #23192
    // clears the upstream memoize on every API request (macOS path), a
    // single transient `security` spawn failure would otherwise poison the
    // cache and surface as "Not logged in" across all subsystems until the
    // next user interaction. clearKeychainCache() sets data=null, so
    // explicit invalidation (logout, delete) still reads through.
    if (prev.data !== null) {
      logForDebugging('[keychain] read failed; serving stale cache', {
        level: 'warn',
      })
      keychainCacheState.cache = { data: prev.data, cachedAt: Date.now() }
      latTrace('secureStorage.keychain.read', {
        cache: 'miss',
        spawn: 'sync',
        dur_ms: latNow() - start,
        result: 'stale',
      })
      return prev.data
    }
    keychainCacheState.cache = { data: null, cachedAt: Date.now() }
    latTrace('secureStorage.keychain.read', {
      cache: 'miss',
      spawn: 'sync',
      dur_ms: latNow() - start,
      result: 'null',
    })
    return null
  },
  readStatusAsync(): Promise<SecureStorageReadStatus> {
    return readAndAssembleStatusAsync()
  },
  async readAsync(): Promise<SecureStorageData | null> {
    const prev = keychainCacheState.cache
    if (Date.now() - prev.cachedAt < KEYCHAIN_CACHE_TTL_MS) {
      latTrace('secureStorage.keychain.readAsync', { cache: 'hit' })
      return prev.data
    }
    if (keychainCacheState.readInFlight) {
      // T2-P0-TRACE — in-flight dedupe: this caller joined an existing async
      // spawn instead of starting its own.
      latTrace('secureStorage.keychain.readAsync', { cache: 'inflight' })
      return keychainCacheState.readInFlight
    }

    const start = latNow()
    const gen = keychainCacheState.generation
    const promise = doReadAsync().then(data => {
      // If the cache was invalidated or updated while we were reading,
      // our subprocess result is stale — don't overwrite the newer entry.
      if (gen === keychainCacheState.generation) {
        // Stale-while-error — mirror read() above.
        if (data === null && prev.data !== null) {
          logForDebugging('[keychain] readAsync failed; serving stale cache', {
            level: 'warn',
          })
        }
        const next = data ?? prev.data
        keychainCacheState.cache = { data: next, cachedAt: Date.now() }
        keychainCacheState.readInFlight = null
        latTrace('secureStorage.keychain.readAsync', {
          cache: 'miss',
          spawn: 'async',
          dur_ms: latNow() - start,
          result: data !== null ? 'data' : prev.data !== null ? 'stale' : 'null',
        })
        return next
      }
      if (keychainCacheState.readInFlight === promise) {
        keychainCacheState.readInFlight = null
      }
      latTrace('secureStorage.keychain.readAsync', {
        cache: 'miss',
        spawn: 'async',
        dur_ms: latNow() - start,
        result: 'superseded',
      })
      return data
    })
    keychainCacheState.readInFlight = promise
    return promise
  },
  update(data: SecureStorageData): SecureStorageWriteResult {
    // Invalidate cache before update.
    clearKeychainCache()

    // T2-P0-TRACE — keychain write. Always `spawn: 'stdin'` now: the chunk
    // scheme (RFC §4 Tier 1) removed the `-X <hex>` argv branch. `chunks` is
    // the entry count (manifest + chunks); `jsonBytes` correlates payload
    // size with the chunk count.
    const start = latNow()
    try {
      const baseService = getMacOsKeychainStorageServiceName(
        CREDENTIALS_SERVICE_SUFFIX,
      )
      const { manifestValue, chunkValues, jsonBytes } =
        encodeCredentialsToEntries(data, getUsername(), baseService)
      const ok = commitWriteSync(baseService, manifestValue, chunkValues)
      if (ok) {
        keychainCacheState.cache = { data, cachedAt: Date.now() }
      }
      latTrace('secureStorage.keychain.update', {
        spawn: 'stdin',
        chunks: chunkValues.length + 1,
        dur_ms: latNow() - start,
        success: ok,
        jsonBytes,
      })
      return { success: ok }
    } catch (_e) {
      latTrace('secureStorage.keychain.update', {
        spawn: 'stdin',
        dur_ms: latNow() - start,
        success: false,
        error: true,
      })
      return { success: false }
    }
  },
  async updateAsync(data: SecureStorageData): Promise<SecureStorageWriteResult> {
    // Invalidate cache before update.
    clearKeychainCache()

    const start = latNow()
    try {
      const baseService = getMacOsKeychainStorageServiceName(
        CREDENTIALS_SERVICE_SUFFIX,
      )
      const { manifestValue, chunkValues, jsonBytes } =
        encodeCredentialsToEntries(data, getUsername(), baseService)
      const ok = await commitWriteAsync(baseService, manifestValue, chunkValues)
      if (ok) {
        keychainCacheState.cache = { data, cachedAt: Date.now() }
      }
      latTrace('secureStorage.keychain.update', {
        spawn: 'stdin',
        mode: 'async',
        chunks: chunkValues.length + 1,
        dur_ms: latNow() - start,
        success: ok,
        jsonBytes,
      })
      return { success: ok }
    } catch (_e) {
      latTrace('secureStorage.keychain.update', {
        spawn: 'stdin',
        mode: 'async',
        dur_ms: latNow() - start,
        success: false,
        error: true,
      })
      return { success: false }
    }
  },
  mutateAsync(fn: SecureStorageMutator): Promise<SecureStorageWriteResult> {
    const runCritical = async (): Promise<SecureStorageWriteResult> => {
      const read = await macOsKeychainStorage.readStatusAsync()
      if (read.status === 'error') {
        return {
          success: false,
          warning: 'secure storage authoritative read unavailable',
        }
      }
      const current = read.status === 'data' ? read.data : {}
      const next = await fn(current)
      return macOsKeychainStorage.updateAsync(next)
    }
    const result = keychainWriteQueue.then(runCritical, runCritical)
    keychainWriteQueue = result.then(NOOP, NOOP)
    return result
  },
  delete(): boolean {
    // Invalidate cache before delete.
    clearKeychainCache()

    // T2-P0-TRACE — keychain delete: a synchronous `security` spawn per entry
    // on the logout / clearLocalAuthState path (audit §1.5 / root-cause D).
    const start = latNow()
    try {
      const baseService = getMacOsKeychainStorageServiceName(
        CREDENTIALS_SERVICE_SUFFIX,
      )
      const { baseOk, swept } = deleteSweepSync(baseService)
      latTrace('secureStorage.keychain.delete', {
        spawn: 'sync',
        dur_ms: latNow() - start,
        success: baseOk,
        sweptChunks: swept,
      })
      return baseOk
    } catch (_e) {
      latTrace('secureStorage.keychain.delete', {
        spawn: 'sync',
        dur_ms: latNow() - start,
        success: false,
        error: true,
      })
      return false
    }
  },
  async deleteAsync(): Promise<boolean> {
    // Invalidate cache before delete.
    clearKeychainCache()

    const start = latNow()
    try {
      const baseService = getMacOsKeychainStorageServiceName(
        CREDENTIALS_SERVICE_SUFFIX,
      )
      const { baseOk, swept } = await deleteSweepAsync(baseService)
      latTrace('secureStorage.keychain.delete', {
        spawn: 'async',
        dur_ms: latNow() - start,
        success: baseOk,
        sweptChunks: swept,
      })
      return baseOk
    } catch (_e) {
      latTrace('secureStorage.keychain.delete', {
        spawn: 'async',
        dur_ms: latNow() - start,
        success: false,
        error: true,
      })
      return false
    }
  },
} satisfies SecureStorage

async function doReadAsync(): Promise<SecureStorageData | null> {
  try {
    return await readAndAssembleAsync()
  } catch (_e) {
    return null
  }
}

let keychainLockedCache: boolean | undefined

/**
 * Checks if the macOS keychain is locked.
 * Returns true if on macOS and keychain is locked (exit code 36 from security show-keychain-info).
 * This commonly happens in SSH sessions where the keychain isn't automatically unlocked.
 *
 * Cached for process lifetime — execaSync('security', ...) is a ~27ms sync
 * subprocess spawn, and this is called from render (AssistantTextMessage).
 * During virtual-scroll remounts on sessions with "Not logged in" messages,
 * each remount re-spawned security(1), adding 27ms/message to the commit.
 * Keychain lock state doesn't change during a CLI session.
 */
export function isMacOsKeychainLocked(): boolean {
  if (keychainLockedCache !== undefined) {
    latTrace('secureStorage.keychain.lockCheck', { cache: 'hit' })
    return keychainLockedCache
  }
  // Only check on macOS
  if (process.platform !== 'darwin') {
    keychainLockedCache = false
    return false
  }

  // T2-P0-TRACE — first-call only: a synchronous `security show-keychain-info`
  // spawn (audit §1.3 — "锁定检查首次调用仍有同步成本"). Subsequent calls hit
  // the process-lifetime cache above.
  const start = latNow()
  try {
    const result = execaSync('security', ['show-keychain-info'], {
      reject: false,
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: KEYCHAIN_COMMAND_TIMEOUT_MS,
    })
    // Exit code 36 indicates the keychain is locked
    keychainLockedCache = result.exitCode === 36
  } catch {
    // If the command fails for any reason, assume keychain is not locked
    keychainLockedCache = false
  }
  latTrace('secureStorage.keychain.lockCheck', {
    cache: 'miss',
    spawn: 'sync',
    dur_ms: latNow() - start,
    locked: keychainLockedCache,
  })
  return keychainLockedCache
}
