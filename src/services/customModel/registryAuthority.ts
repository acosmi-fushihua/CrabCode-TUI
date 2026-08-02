import { randomUUID } from 'node:crypto'
import { getMembershipGateInput } from '../../utils/auth.js'
import { canUseCustomModels } from '../../utils/entitlements/customModels.js'
import {
  deleteCustomModelApiKey,
  readCustomModelApiKeySync,
  retireCustomModelApiKey,
  saveCustomModelEntryApiKey,
  scheduleCustomModelSecretGarbageCollection,
  CUSTOM_MODEL_SECRET_GC_GRACE_MS,
} from '../../utils/model/customModelSecrets.js'
import {
  classifyCustomModelSecretReference,
  settleCustomModelSecretCleanup,
  withCustomModelConfigTransaction,
} from '../../utils/model/customModelConfigTransaction.js'
import { renderCustomModelReference } from '../../utils/model/customModelReference.js'
import {
  getSettingsForSource,
  refreshSettingsForSource,
  updateSettingsForSource,
} from '../../utils/settings/settings.js'
import { resetSettingsCache } from '../../utils/settings/settingsCache.js'
import { reconcileCustomProviderEnvironmentFromSettings } from '../../utils/managedEnv.js'
import { withConfigRevisionLock } from '../config/configRevision.js'
import { WorkerError } from '../rpc/workerError.js'

const INVALID_PARAMS = -32602
const INTERNAL_ERROR = -32603
const CUSTOM_MODEL_ENTITLEMENT_ERROR =
  'custom models require a Plus (or higher) paid subscription'

export type CustomModelProvider =
  | 'anthropic-compatible'
  | 'openai-compatible'

export interface CustomModelEntry {
  id: string
  brand: string
  protocol: CustomModelProvider
  baseUrl: string
  apiKeyHandle?: string
  modelId: string
  displayName?: string
  contextWindow: number
  maxOutputTokens: number
  supportsThinking?: boolean
  supportsTools?: boolean
  supportsJsonMode?: boolean
  supportsVision?: boolean
  enabled: boolean
  isDefault?: boolean
}

export interface CustomModelEntryInput {
  brand: string
  protocol: CustomModelProvider
  baseUrl: string
  modelId: string
  apiKey?: string
  displayName?: string
  contextWindow: number
  maxOutputTokens: number
  supportsThinking?: boolean
  supportsTools?: boolean
  supportsJsonMode?: boolean
  supportsVision?: boolean
  isDefault?: boolean
  enabled: boolean
}

export type AddCustomModelEdit = CustomModelEntryInput & {
  type: 'addCustomModel'
  id?: string
  apiKeyHandle?: string
}
export type UpdateCustomModelEdit = CustomModelEntryInput & {
  type: 'updateCustomModel'
  id: string
  apiKeyHandle?: string
}
export type RemoveCustomModelEdit = {
  type: 'removeCustomModel'
  id: string
}
export type ToggleCustomModelEdit = {
  type: 'toggleCustomModel'
  id: string
  enabled: boolean
}
export type CustomModelRegistryEdit =
  | AddCustomModelEdit
  | UpdateCustomModelEdit
  | RemoveCustomModelEdit
  | ToggleCustomModelEdit

export interface CustomModelSettingsFns {
  updateSettingsForSource(
    source: 'userSettings',
    value: Record<string, unknown>,
  ): { error?: Error | null }
  getSettingsForSource?(
    source: 'userSettings',
  ): Record<string, unknown> | null | undefined
  refreshSettingsForSource?(
    source: 'userSettings',
  ): unknown | Promise<unknown>
  resetSettingsCache?(): void
}

export interface CustomModelWriteFns {
  getMembershipGateInput: typeof getMembershipGateInput
  canUseCustomModels: typeof canUseCustomModels
  saveCustomModelEntryApiKey: typeof saveCustomModelEntryApiKey
  deleteCustomModelApiKey: typeof deleteCustomModelApiKey
  retireCustomModelApiKey?: typeof retireCustomModelApiKey
  readCustomModelApiKeySync?: typeof readCustomModelApiKeySync
  retiredSecretCleanupTimeoutMs?: number
}

type RegistryApplyOptions = {
  testSeam?: boolean
}

const productionSettings: CustomModelSettingsFns = {
  updateSettingsForSource,
  getSettingsForSource,
  refreshSettingsForSource,
  resetSettingsCache,
}

const productionWriteFns: CustomModelWriteFns = {
  getMembershipGateInput,
  canUseCustomModels,
  saveCustomModelEntryApiKey,
  deleteCustomModelApiKey,
  retireCustomModelApiKey,
  readCustomModelApiKeySync,
}

export function materializeCustomModelsList(
  settings: CustomModelSettingsFns,
): CustomModelEntry[] {
  if (!settings.getSettingsForSource) return []
  const userSettings = settings.getSettingsForSource('userSettings')
  if (!userSettings || typeof userSettings !== 'object') return []
  const hasRegistry = Object.prototype.hasOwnProperty.call(
    userSettings,
    'customModels',
  )
  const raw = userSettings.customModels
  if (hasRegistry) {
    if (!Array.isArray(raw)) return []
    return raw.flatMap(item => {
      if (!item || typeof item !== 'object' || Array.isArray(item)) return []
      const d = item as Record<string, unknown>
      if (
        typeof d.id !== 'string' ||
        typeof d.modelId !== 'string' ||
        typeof d.baseUrl !== 'string' ||
        typeof d.contextWindow !== 'number' ||
        typeof d.maxOutputTokens !== 'number'
      ) {
        return []
      }
      const entry: CustomModelEntry = {
        id: d.id,
        brand:
          typeof d.brand === 'string' && d.brand.length > 0
            ? d.brand
            : 'custom',
        protocol:
          d.protocol === 'openai-compatible'
            ? 'openai-compatible'
            : 'anthropic-compatible',
        baseUrl: d.baseUrl,
        modelId: d.modelId,
        contextWindow: d.contextWindow,
        maxOutputTokens: d.maxOutputTokens,
        enabled: typeof d.enabled === 'boolean' ? d.enabled : true,
      }
      copyOptionalEntryFields(d, entry)
      return [entry]
    })
  }

  const legacy = userSettings.customModel
  if (!legacy || typeof legacy !== 'object' || Array.isArray(legacy)) return []
  const lc = legacy as Record<string, unknown>
  if (typeof lc.baseUrl !== 'string' || lc.baseUrl.length === 0) return []
  const protocol: CustomModelProvider =
    lc.provider === 'openai-compatible'
      ? 'openai-compatible'
      : 'anthropic-compatible'
  const apiKeyHandle =
    typeof lc.apiKeyHandle === 'string' && lc.apiKeyHandle.length > 0
      ? lc.apiKeyHandle
      : undefined
  if (!lc.models || typeof lc.models !== 'object') return []
  const out: CustomModelEntry[] = []
  for (const [alias, value] of Object.entries(
    lc.models as Record<string, unknown>,
  )) {
    if (!value || typeof value !== 'object' || Array.isArray(value)) continue
    const d = value as Record<string, unknown>
    if (
      typeof d.id !== 'string' ||
      d.id.length === 0 ||
      typeof d.contextWindow !== 'number' ||
      typeof d.maxOutputTokens !== 'number'
    ) {
      continue
    }
    const entry: CustomModelEntry = {
      id: `legacy:${alias}`,
      brand: 'custom',
      protocol,
      baseUrl: lc.baseUrl,
      modelId: d.id,
      contextWindow: d.contextWindow,
      maxOutputTokens: d.maxOutputTokens,
      enabled: true,
    }
    if (apiKeyHandle) entry.apiKeyHandle = apiKeyHandle
    copyOptionalEntryFields(d, entry)
    out.push(entry)
  }
  return out
}

export async function applyCustomModelRegistryEdit(
  rawEdit: CustomModelRegistryEdit,
  settings: CustomModelSettingsFns,
  loadWriteFns: () => Promise<CustomModelWriteFns>,
  options: RegistryApplyOptions = {},
): Promise<CustomModelRegistryEdit | undefined> {
  const edit = validateEdit(rawEdit)
  if (edit.type === 'addCustomModel') {
    const fns = await loadAuthorityFns(loadWriteFns)
    assertEntitled(fns)
    const before = materializeCustomModelsList(settings)
    assertNoCollision(edit.modelId, before)
    const id = randomUUID()
    let apiKeyHandle: string | undefined
    if (edit.apiKey) {
      try {
        apiKeyHandle = await fns.saveCustomModelEntryApiKey(id, edit.apiKey)
      } catch (error) {
        throw internal(`failed to save custom model API key: ${message(error)}`)
      }
    }
    const entry = entryFromEdit(edit, id, apiKeyHandle)
    const list = materializeCustomModelsList(settings)
    if (entry.isDefault === true) dedupeDefault(list, id)
    list.push(entry)
    const orphanedLegacy = orphanedLegacyHandle(settings, list)
    try {
      writeList(settings, list)
    } catch (error) {
      if (apiKeyHandle) {
        await reconcileFailedCommit(settings, fns, apiKeyHandle, error)
      } else {
        throw error
      }
    }
    await retireOrphans(
      [orphanedLegacy],
      loadWriteFns,
      fns,
      options,
    )
    return sanitizedAdd(entry)
  }

  if (edit.type === 'updateCustomModel') {
    const fns = await loadAuthorityFns(loadWriteFns)
    assertEntitled(fns)
    const list = materializeCustomModelsList(settings)
    const index = list.findIndex(entry => entry.id === edit.id)
    if (index < 0) {
      throw invalid(`custom model not found: ${edit.id}`)
    }
    const previous = list[index]!
    assertNoCollision(edit.modelId, list, edit.id)
    let apiKeyHandle = previous.apiKeyHandle
    let stagedHandle: string | undefined
    if (edit.apiKey) {
      try {
        stagedHandle = await fns.saveCustomModelEntryApiKey(
          edit.id,
          edit.apiKey,
        )
      } catch (error) {
        throw internal(`failed to save custom model API key: ${message(error)}`)
      }
      apiKeyHandle = stagedHandle
    }
    if (
      apiKeyHandle &&
      !stagedHandle &&
      fns.readCustomModelApiKeySync &&
      !fns.readCustomModelApiKeySync(apiKeyHandle)
    ) {
      throw internal(
        'custom model API key handle is not readable; save the API key again',
      )
    }
    const entry = entryFromEdit(edit, edit.id, apiKeyHandle)
    list[index] = entry
    if (entry.isDefault === true) dedupeDefault(list, edit.id)
    const orphanedLegacy = orphanedLegacyHandle(settings, list)
    try {
      writeList(settings, list)
    } catch (error) {
      if (stagedHandle && stagedHandle !== previous.apiKeyHandle) {
        await reconcileFailedCommit(settings, fns, stagedHandle, error)
      } else {
        throw error
      }
    }
    await retireOrphans(
      [
        stagedHandle &&
        previous.apiKeyHandle &&
        previous.apiKeyHandle !== stagedHandle
          ? previous.apiKeyHandle
          : undefined,
        orphanedLegacy,
      ],
      loadWriteFns,
      fns,
      options,
    )
    return sanitizedUpdate(entry)
  }

  if (edit.type === 'removeCustomModel') {
    const list = materializeCustomModelsList(settings)
    const index = list.findIndex(entry => entry.id === edit.id)
    if (index < 0) return undefined
    const removed = list[index]!
    list.splice(index, 1)
    const orphanedLegacy = orphanedLegacyHandle(settings, list)
    writeList(settings, list, {
      clearSelectedModel:
        readSelectedModel(settings) ===
        renderCustomModelReference(removed.id),
    })
    await retireOrphans(
      [removed.apiKeyHandle, orphanedLegacy],
      loadWriteFns,
      undefined,
      options,
    )
    return undefined
  }

  const list = materializeCustomModelsList(settings)
  const index = list.findIndex(entry => entry.id === edit.id)
  if (index < 0) return undefined
  const clearSelectedModel =
    edit.enabled === false &&
    readSelectedModel(settings) ===
      renderCustomModelReference(edit.id)
  list[index]!.enabled = edit.enabled
  const orphanedLegacy = orphanedLegacyHandle(settings, list)
  writeList(settings, list, { clearSelectedModel })
  await retireOrphans(
    [orphanedLegacy],
    loadWriteFns,
    undefined,
    options,
  )
  return undefined
}

export type DirectCustomModelMutationResult = {
  applied: CustomModelRegistryEdit
  version: number
  entries: CustomModelEntry[]
}

export async function readDirectCustomModels(): Promise<CustomModelEntry[]> {
  return withConfigRevisionLock(async revision =>
    withCustomModelConfigTransaction(async () => {
      resetSettingsCache()
      await refreshSettingsForSource('userSettings')
      await revision.refresh()
      return materializeCustomModelsList(productionSettings)
    }),
  )
}

export async function mutateDirectCustomModels(
  edit: CustomModelRegistryEdit,
): Promise<DirectCustomModelMutationResult> {
  // Resolve all required modules before mutation. This function's imports are
  // already the direct truth sources; no daemon or worker is started.
  return withConfigRevisionLock(async revision =>
    withCustomModelConfigTransaction(async () => {
      resetSettingsCache()
      await refreshSettingsForSource('userSettings')
      await revision.refresh()
      const applied =
        (await applyCustomModelRegistryEdit(
          edit,
          productionSettings,
          async () => productionWriteFns,
        )) ?? edit
      if (!reconcileCustomProviderEnvironmentFromSettings()) {
        console.warn(
          '[customModel] provider environment settings read failed; bridge-owned custom values remain cleared',
        )
      }
      const version = await revision.advance()
      return {
        applied,
        version,
        entries: materializeCustomModelsList(productionSettings),
      }
    }),
  )
}

function validateEdit(edit: CustomModelRegistryEdit): CustomModelRegistryEdit {
  if (edit.type === 'removeCustomModel') {
    return { type: edit.type, id: required(edit.id, `${edit.type}.id`) }
  }
  if (edit.type === 'toggleCustomModel') {
    if (typeof edit.enabled !== 'boolean') {
      throw invalid('toggleCustomModel.enabled must be a boolean')
    }
    return {
      type: edit.type,
      id: required(edit.id, `${edit.type}.id`),
      enabled: edit.enabled,
    }
  }
  const field = edit.type
  const rawBaseUrl = required(edit.baseUrl, `${field}.baseUrl`)
  let parsed: URL
  try {
    parsed = new URL(rawBaseUrl)
  } catch {
    throw invalid(`${field}.baseUrl must be a valid URL`)
  }
  if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') {
    throw invalid(`${field}.baseUrl must use http(s)`)
  }
  if (
    edit.protocol !== 'anthropic-compatible' &&
    edit.protocol !== 'openai-compatible'
  ) {
    throw invalid(
      `${field}.protocol must be one of 'anthropic-compatible' | 'openai-compatible'`,
    )
  }
  if (
    !Number.isInteger(edit.contextWindow) ||
    edit.contextWindow <= 0
  ) {
    throw invalid(`${field}.contextWindow must be a positive integer`)
  }
  if (
    !Number.isInteger(edit.maxOutputTokens) ||
    edit.maxOutputTokens <= 0
  ) {
    throw invalid(`${field}.maxOutputTokens must be a positive integer`)
  }
  if (typeof edit.enabled !== 'boolean') {
    throw invalid(`${field}.enabled must be a boolean`)
  }
  const common: CustomModelEntryInput = {
    brand: required(edit.brand, `${field}.brand`),
    protocol: edit.protocol,
    baseUrl: parsed.toString(),
    modelId: required(edit.modelId, `${field}.modelId`),
    contextWindow: edit.contextWindow,
    maxOutputTokens: edit.maxOutputTokens,
    enabled: edit.enabled,
  }
  copyInputOptionals(edit, common, field)
  return edit.type === 'updateCustomModel'
    ? {
        type: edit.type,
        id: required(edit.id, `${field}.id`),
        ...common,
      }
    : { type: edit.type, ...common }
}

function copyInputOptionals(
  source: CustomModelEntryInput,
  target: CustomModelEntryInput,
  field: string,
): void {
  if (source.apiKey !== undefined) {
    target.apiKey = required(source.apiKey, `${field}.apiKey`)
  }
  if (source.displayName !== undefined) {
    target.displayName = required(
      source.displayName,
      `${field}.displayName`,
    )
  }
  for (const key of [
    'supportsThinking',
    'supportsTools',
    'supportsJsonMode',
    'supportsVision',
    'isDefault',
  ] as const) {
    const value = source[key]
    if (value === undefined) continue
    if (typeof value !== 'boolean') {
      throw invalid(`${field}.${key} must be a boolean when provided`)
    }
    target[key] = value
  }
}

function entryFromEdit(
  edit: AddCustomModelEdit | UpdateCustomModelEdit,
  id: string,
  apiKeyHandle: string | undefined,
): CustomModelEntry {
  const entry: CustomModelEntry = {
    id,
    brand: edit.brand,
    protocol: edit.protocol,
    baseUrl: edit.baseUrl,
    modelId: edit.modelId,
    contextWindow: edit.contextWindow,
    maxOutputTokens: edit.maxOutputTokens,
    enabled: edit.enabled,
  }
  if (apiKeyHandle) entry.apiKeyHandle = apiKeyHandle
  copyOptionalEntryFields(edit as unknown as Record<string, unknown>, entry)
  return entry
}

function copyOptionalEntryFields(
  source: Record<string, unknown>,
  target: CustomModelEntry,
): void {
  if (typeof source.apiKeyHandle === 'string' && source.apiKeyHandle.length) {
    target.apiKeyHandle = source.apiKeyHandle
  }
  if (typeof source.displayName === 'string') {
    target.displayName = source.displayName
  }
  for (const key of [
    'supportsThinking',
    'supportsTools',
    'supportsJsonMode',
    'supportsVision',
    'isDefault',
  ] as const) {
    if (typeof source[key] === 'boolean') target[key] = source[key]
  }
}

function writeList(
  settings: CustomModelSettingsFns,
  list: CustomModelEntry[],
  options: { clearSelectedModel?: boolean } = {},
): void {
  const partial: Record<string, unknown> = {
    customModels: list,
    customModel: undefined,
  }
  if (options.clearSelectedModel) partial.model = undefined
  const result = settings.updateSettingsForSource(
    'userSettings',
    partial,
  )
  if (result.error) {
    throw internal(
      `failed to write userSettings.customModels: ${result.error.message}`,
    )
  }
}

function readLegacyHandle(
  settings: CustomModelSettingsFns,
): string | undefined {
  const value = settings.getSettingsForSource?.('userSettings')?.customModel
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return undefined
  }
  const handle = (value as Record<string, unknown>).apiKeyHandle
  return typeof handle === 'string' && handle.length > 0
    ? handle
    : undefined
}

function orphanedLegacyHandle(
  settings: CustomModelSettingsFns,
  list: readonly CustomModelEntry[],
): string | undefined {
  const handle = readLegacyHandle(settings)
  return handle && !list.some(entry => entry.apiKeyHandle === handle)
    ? handle
    : undefined
}

function readSelectedModel(
  settings: CustomModelSettingsFns,
): string | undefined {
  const model = settings.getSettingsForSource?.('userSettings')?.model
  return typeof model === 'string' && model.length > 0
    ? model
    : undefined
}

function dedupeDefault(
  list: CustomModelEntry[],
  keepId: string,
): void {
  for (const entry of list) {
    if (entry.id !== keepId && entry.isDefault) {
      delete entry.isDefault
    }
  }
}

function assertNoCollision(
  modelId: string,
  entries: readonly CustomModelEntry[],
  selfId?: string,
): void {
  if (
    entries.some(
      entry => entry.id !== selfId && entry.modelId === modelId,
    )
  ) {
    throw invalid(
      `已存在 modelId 为 "${modelId}" 的自定义模型，请改用不同的模型 ID。`,
    )
  }
}

async function loadAuthorityFns(
  loader: () => Promise<CustomModelWriteFns>,
): Promise<CustomModelWriteFns> {
  try {
    return await loader()
  } catch (error) {
    throw internal(
      `custom model truth source unavailable: ${message(error)}`,
    )
  }
}

function assertEntitled(fns: CustomModelWriteFns): void {
  if (!fns.canUseCustomModels(fns.getMembershipGateInput())) {
    throw invalid(CUSTOM_MODEL_ENTITLEMENT_ERROR)
  }
}

async function reconcileFailedCommit(
  settings: CustomModelSettingsFns,
  fns: CustomModelWriteFns,
  stagedHandle: string,
  writeError: unknown,
): Promise<void> {
  let state: ReturnType<typeof classifyCustomModelSecretReference> =
    'unknown'
  try {
    settings.resetSettingsCache?.()
    state = classifyCustomModelSecretReference(
      settings.getSettingsForSource?.('userSettings'),
      stagedHandle,
    )
  } catch {
    state = 'unknown'
  }
  if (state === 'referenced') return
  if (state === 'unknown') {
    throw internal(
      `${message(writeError)}; settings commit outcome unknown; staged custom model secret retained`,
    )
  }
  const outcome = await settleCustomModelSecretCleanup(
    () => fns.deleteCustomModelApiKey(stagedHandle),
    fns.retiredSecretCleanupTimeoutMs,
  )
  if (outcome !== 'cleaned') {
    throw internal(
      `${message(writeError)}; staged custom model secret cleanup ${outcome}`,
    )
  }
  if (writeError instanceof WorkerError) throw writeError
  throw internal(message(writeError))
}

async function retireOrphans(
  handles: readonly (string | undefined)[],
  loader: () => Promise<CustomModelWriteFns>,
  knownFns: CustomModelWriteFns | undefined,
  options: RegistryApplyOptions,
): Promise<void> {
  for (const handle of new Set(handles.filter(Boolean) as string[])) {
    try {
      const fns = knownFns ?? (await loader())
      if (!fns.retireCustomModelApiKey) {
        if (!options.testSeam) {
          console.warn(
            '[customModel] prior secret retirement remains pending',
          )
        }
        continue
      }
      const outcome = await settleCustomModelSecretCleanup(
        () => fns.retireCustomModelApiKey!(handle),
        fns.retiredSecretCleanupTimeoutMs,
      )
      if (!options.testSeam) {
        scheduleCustomModelSecretGarbageCollection(
          CUSTOM_MODEL_SECRET_GC_GRACE_MS,
        )
      }
      if (outcome !== 'cleaned') {
        console.warn(
          '[customModel] prior secret retirement remains pending',
        )
      }
    } catch (error) {
      console.warn(
        `[customModel] registry committed but retired secret cleanup could not start (${error instanceof Error ? error.name : 'unknown'})`,
      )
    }
  }
}

function sanitizedAdd(entry: CustomModelEntry): AddCustomModelEdit {
  return {
    type: 'addCustomModel',
    ...entry,
  }
}

function sanitizedUpdate(
  entry: CustomModelEntry,
): UpdateCustomModelEdit {
  return {
    type: 'updateCustomModel',
    ...entry,
  }
}

function required(value: unknown, field: string): string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw invalid(`${field} must be a non-empty string`)
  }
  return value.trim()
}

function invalid(messageText: string): WorkerError {
  return new WorkerError(INVALID_PARAMS, messageText)
}

function internal(messageText: string): WorkerError {
  return new WorkerError(INTERNAL_ERROR, messageText)
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
