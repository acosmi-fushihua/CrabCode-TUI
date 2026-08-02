import { createHash, randomUUID } from 'crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'
import { stripBOM } from '../jsonRead.js'
import { prepareFallbackStorageForCrossProcessFreshRead } from '../secureStorage/fallbackStorage.js'
import { getSecureStorage } from '../secureStorage/index.js'
import { clearKeychainCache } from '../secureStorage/macOsKeychainHelpers.js'
import { withCustomModelConfigTransaction } from './customModelConfigTransaction.js'

const CUSTOM_MODEL_API_KEYS_FIELD = 'customModelApiKeys'
const CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD = 'customModelApiKeyCreatedAtMs'
const CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD = 'customModelApiKeyRetiredAtMs'
export const CUSTOM_MODEL_SECRET_GC_GRACE_MS = 5 * 60 * 1_000

export interface CustomModelSecretDescriptor {
  provider: 'anthropic-compatible' | 'openai-compatible'
  baseUrl: string
  modelId: string
}

export interface SaveCustomModelApiKeyInput extends CustomModelSecretDescriptor {
  apiKey: string
}

export function buildCustomModelApiKeyHandle(
  descriptor: CustomModelSecretDescriptor,
): string {
  const digest = createHash('sha256')
    .update(descriptor.provider)
    .update('\0')
    .update(descriptor.baseUrl.trim())
    .update('\0')
    .update(descriptor.modelId.trim())
    .digest('hex')
    .slice(0, 24)
  return `custom-model:${digest}`
}

export function buildVersionedCustomModelApiKeyHandle(
  descriptor: CustomModelSecretDescriptor,
  generation: string,
): string {
  if (!generation || generation.includes('\0')) {
    throw new Error('custom model secret generation must be non-empty')
  }
  return `${buildCustomModelApiKeyHandle(descriptor)}:${generation}`
}

export async function saveCustomModelApiKey(
  input: SaveCustomModelApiKeyInput,
): Promise<string> {
  const apiKey = input.apiKey.trim()
  if (!apiKey) {
    throw new Error('custom model API key must be non-empty')
  }
  // Copy-on-write generation: never overwrite a handle that the current
  // settings registry may still reference. The caller publishes this new
  // handle only after the secure write succeeds.
  const handle = buildVersionedCustomModelApiKeyHandle(input, randomUUID())
  const storage = getSecureStorage()
  const result = await storage.mutateAsync(current => {
    const existing =
      current[CUSTOM_MODEL_API_KEYS_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEYS_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEYS_FIELD] as Record<string, string>)
        : {}
    const createdAt =
      current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] as Record<
            string,
            number
          >)
        : {}
    return {
      ...current,
      [CUSTOM_MODEL_API_KEYS_FIELD]: {
        ...existing,
        [handle]: apiKey,
      },
      [CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD]: {
        ...createdAt,
        [handle]: Date.now(),
      },
    }
  })
  if (!result.success) {
    throw new Error(result.warning ?? 'failed to save custom model API key')
  }
  return handle
}

/**
 * Per-entry secure-storage handle for the
 * multi-provider custom model registry. The entry `id` supplies the stable
 * handle prefix; each save appends a fresh generation so rotations never
 * overwrite the still-referenced credential (unlike the legacy prefix, whose
 * stable portion hashes provider/baseUrl/modelId).
 */
export function buildCustomModelEntryApiKeyHandle(id: string): string {
  return `custom-model:${id}`
}

export function buildVersionedCustomModelEntryApiKeyHandle(
  id: string,
  generation: string,
): string {
  if (!generation || generation.includes('\0')) {
    throw new Error('custom model secret generation must be non-empty')
  }
  return `${buildCustomModelEntryApiKeyHandle(id)}:${generation}`
}

/**
 * Store the plaintext API key for one registry entry
 * in secure storage under its derived handle. Returns the handle so callers
 * persist only the handle (never the plaintext key) in settings.
 */
export async function saveCustomModelEntryApiKey(
  id: string,
  apiKey: string,
): Promise<string> {
  const trimmed = apiKey.trim()
  if (!trimmed) {
    throw new Error('custom model API key must be non-empty')
  }
  // A rotation stages a fresh handle so the old registry/key pair remains
  // valid until settings atomically publishes the new generation.
  const handle = buildVersionedCustomModelEntryApiKeyHandle(id, randomUUID())
  const storage = getSecureStorage()
  const result = await storage.mutateAsync(current => {
    const existing =
      current[CUSTOM_MODEL_API_KEYS_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEYS_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEYS_FIELD] as Record<string, string>)
        : {}
    const createdAt =
      current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] as Record<
            string,
            number
          >)
        : {}
    return {
      ...current,
      [CUSTOM_MODEL_API_KEYS_FIELD]: {
        ...existing,
        [handle]: trimmed,
      },
      [CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD]: {
        ...createdAt,
        [handle]: Date.now(),
      },
    }
  })
  if (!result.success) {
    throw new Error(result.warning ?? 'failed to save custom model API key')
  }
  return handle
}

export function readCustomModelApiKeySync(handle: string | undefined): string | null {
  if (!handle) return null
  const storage = getSecureStorage()
  const readHandle = (): string | null => {
    const data = storage.read()
    const keys = data?.[CUSTOM_MODEL_API_KEYS_FIELD]
    if (!keys || typeof keys !== 'object') return null
    const value = (keys as Record<string, unknown>)[handle]
    return typeof value === 'string' && value.length > 0 ? value : null
  }

  // Keep the normal 30s Keychain cache hot. Only a requested-handle miss can
  // mean another process just published a fresh generation, so force one
  // cross-process refresh then rather than spawning `security` on every hit.
  const cached = readHandle()
  if (cached || process.platform !== 'darwin') return cached
  clearKeychainCache()
  prepareFallbackStorageForCrossProcessFreshRead()
  return readHandle()
}

export async function deleteCustomModelApiKey(handle: string | undefined): Promise<void> {
  if (!handle) return
  const storage = getSecureStorage()
  const result = await storage.mutateAsync(current => {
    const existing =
      current[CUSTOM_MODEL_API_KEYS_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEYS_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEYS_FIELD] as Record<string, string>)
        : {}
    const next = { ...existing }
    delete next[handle]
    const createdAt =
      current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] as Record<
            string,
            number
          >)
        : {}
    const nextCreatedAt = { ...createdAt }
    delete nextCreatedAt[handle]
    const retiredAt =
      current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] as Record<
            string,
            number
          >)
        : {}
    const nextRetiredAt = { ...retiredAt }
    delete nextRetiredAt[handle]
    return {
      ...current,
      [CUSTOM_MODEL_API_KEYS_FIELD]: next,
      [CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD]: nextCreatedAt,
      [CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD]: nextRetiredAt,
    }
  })
  if (!result.success) {
    throw new Error(result.warning ?? 'failed to delete custom model API key')
  }
}

export async function retireCustomModelApiKey(
  handle: string | undefined,
): Promise<void> {
  if (!handle) return
  const storage = getSecureStorage()
  const result = await storage.mutateAsync(current => {
    const existing =
      current[CUSTOM_MODEL_API_KEYS_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEYS_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEYS_FIELD] as Record<string, string>)
        : {}
    // A missing handle needs no tombstone. Never create metadata that could
    // later authorize deletion of an unrelated future key with the same name.
    if (!(handle in existing)) return current
    const retiredAt =
      current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] &&
      typeof current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] === 'object'
        ? (current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] as Record<
            string,
            number
          >)
        : {}
    return {
      ...current,
      [CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD]: {
        ...retiredAt,
        [handle]: Date.now(),
      },
    }
  })
  if (!result.success) {
    throw new Error(result.warning ?? 'failed to retire custom model API key')
  }
}

function collectHandlesFromSettingsFile(
  filePath: string,
  handles: Set<string>,
): void {
  let content: string
  try {
    content = readFileSync(filePath, 'utf8')
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return
    throw new Error('custom model settings reference scan failed')
  }
  try {
    const normalized = stripBOM(content)
    const parsed = (normalized.trim() === ''
      ? {}
      : JSON.parse(normalized)) as Record<string, unknown>
    const legacy = parsed.customModel
    if (legacy && typeof legacy === 'object') {
      const handle = (legacy as Record<string, unknown>).apiKeyHandle
      if (typeof handle === 'string' && handle) handles.add(handle)
    }
    if (Array.isArray(parsed.customModels)) {
      for (const entry of parsed.customModels) {
        if (!entry || typeof entry !== 'object') continue
        const handle = (entry as Record<string, unknown>).apiKeyHandle
        if (typeof handle === 'string' && handle) handles.add(handle)
      }
    }
  } catch {
    // Missing or malformed settings fail closed: the GC caller treats an
    // unreadable file as a reason to skip deletion entirely below.
    throw new Error('custom model settings reference scan failed')
  }
}

export interface CustomModelSecretGcOptions {
  nowMs?: number
  minAgeMs?: number
}

export interface CustomModelSecretGcResult {
  deleted: number
  nextDueAtMs: number | null
}

function hasCustomModelSecretRetirementTombstone(
  data: Record<string, unknown> | null,
): boolean {
  const retired = data?.[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD]
  return Boolean(
    retired &&
      typeof retired === 'object' &&
      !Array.isArray(retired) &&
      Object.keys(retired as Record<string, unknown>).length > 0,
  )
}

/**
 * Deferred, crash-recoverable retirement. Both standard and cowork settings
 * are scanned under the same custom-model transaction lock. Only handles with
 * an explicit retirement tombstone and no durable reference are eligible.
 */
export async function garbageCollectUnreferencedCustomModelSecrets(
  options: CustomModelSecretGcOptions = {},
): Promise<CustomModelSecretGcResult> {
  // Cold starts overwhelmingly have no tombstones. Avoid turning a harmless
  // startup sweep into a full Keychain/plaintext generation rewrite.
  const initialStorage = getSecureStorage()
  if (
    !hasCustomModelSecretRetirementTombstone(await initialStorage.readAsync())
  ) {
    return { deleted: 0, nextDueAtMs: null }
  }
  return withCustomModelConfigTransaction(async () => {
    const storage = getSecureStorage()
    // Recheck after acquiring the custom registry lock: another process may
    // already have completed the only retirement while we waited.
    if (!hasCustomModelSecretRetirementTombstone(await storage.readAsync())) {
      return { deleted: 0, nextDueAtMs: null }
    }
    const handles = new Set<string>()
    const configHome = getCrabCodeConfigHomeDir()
    for (const fileName of ['settings.json', 'cowork_settings.json']) {
      const filePath = join(configHome, fileName)
      collectHandlesFromSettingsFile(filePath, handles)
    }

    const nowMs = options.nowMs ?? Date.now()
    const minAgeMs = options.minAgeMs ?? CUSTOM_MODEL_SECRET_GC_GRACE_MS
    let deleted = 0
    let nextDueAtMs: number | null = null
    const result = await storage.mutateAsync(current => {
      const existing =
        current[CUSTOM_MODEL_API_KEYS_FIELD] &&
        typeof current[CUSTOM_MODEL_API_KEYS_FIELD] === 'object'
          ? (current[CUSTOM_MODEL_API_KEYS_FIELD] as Record<string, string>)
          : {}
      const createdAt =
        current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] &&
        typeof current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] === 'object'
          ? (current[CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD] as Record<
              string,
              number
          >)
          : {}
      const retiredAt =
        current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] &&
        typeof current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] === 'object'
          ? (current[CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD] as Record<
              string,
              number
            >)
          : {}
      const next = { ...existing }
      const nextCreatedAt = { ...createdAt }
      const nextRetiredAt = { ...retiredAt }
      for (const [handle, retiredAtMs] of Object.entries(retiredAt)) {
        if (handles.has(handle)) {
          // A handle can be restored from a backup/cowork settings file.
          // Clear its retirement marker; a later removal must retire anew.
          delete nextRetiredAt[handle]
          continue
        }
        if (!Number.isFinite(retiredAtMs) || retiredAtMs < 0) continue
        const dueAtMs = retiredAtMs + minAgeMs
        if (nowMs < dueAtMs) {
          nextDueAtMs =
            nextDueAtMs === null ? dueAtMs : Math.min(nextDueAtMs, dueAtMs)
          continue
        }
        if (handle in next) {
          delete next[handle]
          deleted += 1
        }
        delete nextCreatedAt[handle]
        delete nextRetiredAt[handle]
      }
      return {
        ...current,
        [CUSTOM_MODEL_API_KEYS_FIELD]: next,
        [CUSTOM_MODEL_API_KEY_CREATED_AT_FIELD]: nextCreatedAt,
        [CUSTOM_MODEL_API_KEY_RETIRED_AT_FIELD]: nextRetiredAt,
      }
    })
    if (!result.success) {
      throw new Error(result.warning ?? 'custom model secret GC failed')
    }
    return { deleted, nextDueAtMs }
  })
}

const CUSTOM_MODEL_SECRET_GC_RETRY_MS = 30_000
let secretGcTimer: ReturnType<typeof setTimeout> | undefined
let secretGcScheduledAtMs = 0

export function computeCustomModelSecretGcDelayMs(
  nowMs: number,
  nextDueAtMs: number,
): number {
  return Math.max(0, Math.min(nextDueAtMs - nowMs, 2_147_483_647))
}

export function scheduleCustomModelSecretGarbageCollection(delayMs = 0): void {
  const nowMs = Date.now()
  const scheduledAtMs = nowMs + Math.max(0, delayMs)
  if (secretGcTimer && secretGcScheduledAtMs <= scheduledAtMs) return
  if (secretGcTimer) clearTimeout(secretGcTimer)
  secretGcScheduledAtMs = scheduledAtMs
  secretGcTimer = setTimeout(() => {
    secretGcTimer = undefined
    secretGcScheduledAtMs = 0
    void garbageCollectUnreferencedCustomModelSecrets()
      .then(result => {
        if (result.nextDueAtMs !== null) {
          scheduleCustomModelSecretGarbageCollection(
            computeCustomModelSecretGcDelayMs(Date.now(), result.nextDueAtMs),
          )
        }
      })
      .catch(() => {
        console.warn('[customModel] deferred secret cleanup is pending')
        scheduleCustomModelSecretGarbageCollection(
          CUSTOM_MODEL_SECRET_GC_RETRY_MS,
        )
      })
  }, computeCustomModelSecretGcDelayMs(nowMs, scheduledAtMs))
  secretGcTimer.unref?.()
}

export function __resetCustomModelSecretGcSchedulerForTests(): void {
  if (secretGcTimer) clearTimeout(secretGcTimer)
  secretGcTimer = undefined
  secretGcScheduledAtMs = 0
}
