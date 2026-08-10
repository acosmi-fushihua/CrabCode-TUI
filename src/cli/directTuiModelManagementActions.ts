import { z } from 'zod'

import type {
  CustomModelDirectClient,
  CustomModelEntry,
} from '../services/customModel/directClient.js'
import type { LocalModelDirectClient } from '../services/localModel/directClient.js'
import type { AccountBridgeManager } from '../services/accountBridge/runtimeManager.js'
import { accountBridgeProtectedReadsPermitted } from '../services/accountBridge/domain.js'
import type {
  AccountBridgeAccountView,
  AccountBridgeConnectorView,
  AccountBridgeEligibilityView,
  AccountBridgeModelRouteView,
  AccountBridgeRuntimeView,
  AccountBridgeUsageSnapshot,
} from '../services/accountBridge/types.js'
import { renderCustomModelReference } from '../utils/model/customModelReference.js'
import { renderLocalModelReference } from '../utils/model/localModelReference.js'
import { renderAccountBridgeReference } from '../utils/model/accountBridgeReference.js'

const safeId = z.string().trim().min(1).max(512)
const safeShortText = z.string().min(1).max(240)
const safeUrl = z.string().min(1).max(4_096)
const safePath = z.string().min(1).max(4_096)
const safeSecret = z.string().min(1).max(8_192)
const optionalNullableId = z.string().trim().min(1).max(512).nullish()
const optionalNullablePath = z.string().min(1).max(4_096).nullish()
const positiveInteger = z.number().int().positive().safe()
const optionalBoolean = z.boolean().optional()

const customModelInputSchema = z
  .object({
    brand: safeShortText,
    protocol: z.enum(['anthropic-compatible', 'openai-compatible']),
    baseUrl: safeUrl,
    modelId: safeId,
    apiKey: safeSecret.optional(),
    displayName: safeShortText.optional(),
    contextWindow: positiveInteger,
    maxOutputTokens: positiveInteger,
    supportsThinking: optionalBoolean,
    supportsTools: optionalBoolean,
    supportsJsonMode: optionalBoolean,
    supportsVision: optionalBoolean,
    isDefault: optionalBoolean,
    enabled: z.boolean(),
  })
  .strict()

const customModelViewSchema = z
  .object({
    id: safeId,
    brand: safeShortText,
    protocol: z.enum(['anthropic-compatible', 'openai-compatible']),
    baseUrl: safeUrl,
    modelId: safeId,
    displayName: safeShortText.optional(),
    contextWindow: positiveInteger,
    maxOutputTokens: positiveInteger,
    supportsThinking: optionalBoolean,
    supportsTools: optionalBoolean,
    supportsJsonMode: optionalBoolean,
    supportsVision: optionalBoolean,
    enabled: z.boolean(),
    isDefault: optionalBoolean,
    hasStoredCredential: z.boolean(),
    modelReference: z.string().min(1).max(1_024),
  })
  .strict()

const customTestResultSchema = z
  .object({
    ok: z.boolean(),
    httpStatus: z.number().int().min(100).max(599).optional(),
    latencyMs: z.number().nonnegative().finite().optional(),
    errorReason: z.string().max(1_000).optional(),
  })
  .strict()

const localCatalogEntrySchema = z
  .object({
    id: safeId,
    displayName: z.string().max(240),
    description: z.string().max(8_192).nullable(),
    runtime: z.enum(['llama-server', 'ds4']),
    protocol: z.literal('openai-compatible'),
    format: z.literal('gguf'),
    source: z.enum(['curated', 'user-local-path']),
    license: z.string().max(1_024).nullable(),
    sizeBytes: z.number().nonnegative().finite().nullable(),
    sha256: z.string().max(128).nullable(),
    installed: z.boolean(),
    status: z.string().max(128),
    modelPath: z.string().max(4_096).nullable(),
    reason: z.string().max(2_048).nullable(),
    modelReference: z.string().min(1).max(1_024),
  })
  .strict()

const localCatalogSchema = z
  .object({
    data: z.array(localCatalogEntrySchema).max(10_000),
    source: z.enum(['curated', 'user-local-path']),
    manifestStatus: z.literal('awaiting-license-signoff'),
    manifestVersion: z.number().int().nonnegative().safe(),
  })
  .strict()

const localProfileSchema = z
  .object({
    platform: z.string().max(64),
    arch: z.string().max(64),
    memoryBytes: z.number().nonnegative().finite().nullable(),
    recommendedRuntime: z.enum(['llama-server', 'ds4']).nullable(),
    supportedRuntimes: z
      .array(
        z
          .object({
            id: z.enum(['llama-server', 'ds4']),
            displayName: z.string().max(240),
            supported: z.boolean(),
            acceleration: z.literal('metal').nullable(),
            reason: z.string().max(2_048),
          })
          .strict(),
      )
      .max(32),
  })
  .strict()

const localDownloadResultSchema = z
  .object({
    status: z
      .object({
        state: z.enum([
          'queued',
          'downloading',
          'completed',
          'failed',
          'cancelled',
          'not-found',
          'unavailable',
        ]),
        reason: z.string().max(2_048).nullable(),
        downloadId: z.string().max(512).nullable(),
        modelId: z.string().max(512).nullable(),
        bytesReceived: z.number().nonnegative().finite().nullable(),
        totalBytes: z.number().nonnegative().finite().nullable(),
        percentage: z.number().finite().nullable(),
        error: z.string().max(8_192).nullable(),
      })
      .strict(),
  })
  .strict()

const localInstallRemoveResultSchema = z
  .object({
    state: z.enum(['removed', 'not-found', 'failed', 'unavailable']),
    reason: z.string().max(2_048).nullable(),
    modelId: z.string().max(512).nullable(),
    modelPath: z.string().max(4_096).nullable(),
  })
  .strict()

const localServerResultSchema = z
  .object({
    status: z
      .object({
        state: z.enum([
          'stopped',
          'starting',
          'running',
          'stopping',
          'failed',
          'unavailable',
        ]),
        reason: z.string().max(2_048).optional(),
        host: z.string().max(512).optional(),
        port: z.number().int().min(1).max(65_535).optional(),
        url: z.string().max(4_096).optional(),
        pid: z.number().int().positive().safe().optional(),
        modelId: z.string().max(512).optional(),
        modelPath: z.string().max(4_096).optional(),
        error: z.string().max(8_192).optional(),
        stderrTail: z.string().max(64 * 1_024).optional(),
      })
      .strict(),
  })
  .strict()

const eligibilitySchema = z
  .object({
    state: z.enum(['checking', 'allowed', 'blocked-cn', 'unavailable']),
    countryCode: z.string().max(8).nullable(),
    policyVersion: z.string().max(512).nullable(),
    checkedAt: z.string().max(128).nullable(),
    expiresAt: z.string().max(128).nullable(),
    reasonCode: z.string().max(512).nullable(),
  })
  .strict()

const connectorSchema = z
  .object({
    connectorId: safeId,
    displayName: z.string().max(240),
    authMode: z.enum(['browser', 'device-code']),
    enabled: z.boolean(),
    disabledReasonCode: z.string().max(512).nullable(),
    termsStatus: z.enum(['signed-off', 'blocked']),
  })
  .strict()

const runtimeSchema = z
  .object({
    state: z.enum([
      'stopped',
      'starting',
      'ready',
      'stopping',
      'failed',
      'blocked',
    ]),
    componentVersion: z.string().max(512).nullable(),
    protocolVersion: z.number().int().nonnegative().safe().nullable(),
    lastErrorCode: z.string().max(512).nullable(),
  })
  .strict()

const accountSchema = z
  .object({
    accountId: safeId,
    connectorId: safeId,
    displayLabel: z.string().max(240),
    status: z.enum([
      'ready',
      'reauthorization-required',
      'cooldown',
      'quota-exhausted',
      'disabled',
    ]),
    connectedAt: z.string().max(128),
    lastUsedAt: z.string().max(128).nullable(),
    cooldownUntil: z.string().max(128).nullable(),
  })
  .strict()

const capabilityFields = {
  chatRuntimeSupported: z.boolean().nullable(),
  supportsTools: z.boolean().nullable(),
  supportsThinking: z.boolean().nullable(),
  supportsAdaptiveThinking: z.boolean().nullable(),
  supportsEffort: z.boolean().nullable(),
  supportsMaxEffort: z.boolean().nullable(),
  supportsVision: z.boolean().nullable(),
  supportsJsonMode: z.boolean().nullable(),
  supportedThinkingModes: z
    .array(z.enum(['auto', 'off', 'standard', 'deep']))
    .max(16),
  defaultThinkingMode: z
    .enum(['auto', 'off', 'standard', 'deep'])
    .nullable(),
  contextWindow: z.number().int().positive().safe().nullable(),
  maxOutputTokens: z.number().int().positive().safe().nullable(),
}

const accountRouteSchema = z
  .object({
    routeId: safeId,
    accountId: safeId,
    connectorId: safeId,
    modelId: safeId,
    displayName: z.string().max(240).nullable(),
    connectorLabel: z.string().max(240),
    accountLabel: z.string().max(240),
    ...capabilityFields,
    modelReference: z.string().min(1).max(1_024),
  })
  .strict()

const usageWindowSchema = z
  .object({
    label: z.string().max(240),
    limit: z.number().finite().nullable(),
    used: z.number().finite().nullable(),
    remainingPercent: z.number().finite().nullable(),
    resetsAt: z.string().max(128).nullable(),
  })
  .strict()

const usageSnapshotSchema = z
  .object({
    routeId: safeId,
    accountId: safeId,
    state: z.enum(['available', 'cooldown', 'exhausted', 'unknown']),
    remainingPercent: z.number().finite().nullable(),
    limitingWindowLabel: z.string().max(240).nullable(),
    resetsAt: z.string().max(128).nullable(),
    windows: z.array(usageWindowSchema).max(64),
    observedAt: z.string().max(128),
  })
  .strict()

const accountSnapshotSchema = z
  .object({
    eligibility: eligibilitySchema,
    runtime: runtimeSchema.nullable(),
    connectors: z.array(connectorSchema).max(128),
    accounts: z.array(accountSchema).max(10_000),
    routes: z.array(accountRouteSchema).max(50_000),
    usage: z.array(usageSnapshotSchema).max(50_000),
  })
  .strict()

export const directTuiModelManagementActionSchema = z.discriminatedUnion(
  'kind',
  [
    z.object({ kind: z.literal('model.custom.list') }).strict(),
    z
      .object({ kind: z.literal('model.custom.add'), input: customModelInputSchema })
      .strict(),
    z
      .object({
        kind: z.literal('model.custom.update'),
        id: safeId,
        input: customModelInputSchema,
      })
      .strict(),
    z.object({ kind: z.literal('model.custom.remove'), id: safeId }).strict(),
    z
      .object({
        kind: z.literal('model.custom.toggle'),
        id: safeId,
        enabled: z.boolean(),
      })
      .strict(),
    z
      .object({ kind: z.literal('model.custom.test_saved'), id: safeId })
      .strict(),
    z
      .object({
        kind: z.literal('model.custom.test_draft'),
        baseUrl: safeUrl,
        protocol: z.enum(['anthropic-compatible', 'openai-compatible']),
        modelId: safeId,
        apiKey: safeSecret,
      })
      .strict(),
    z.object({ kind: z.literal('model.local.snapshot') }).strict(),
    z.object({ kind: z.literal('model.local.download_start'), modelId: safeId }).strict(),
    z
      .object({
        kind: z.literal('model.local.download_progress'),
        downloadId: optionalNullableId,
        modelId: optionalNullableId,
      })
      .strict(),
    z
      .object({
        kind: z.literal('model.local.download_cancel'),
        downloadId: optionalNullableId,
        modelId: optionalNullableId,
      })
      .strict(),
    z
      .object({
        kind: z.literal('model.local.install_remove'),
        modelId: optionalNullableId,
        modelPath: optionalNullablePath,
        removeFiles: z.boolean(),
      })
      .strict(),
    z
      .object({
        kind: z.literal('model.local.server_start'),
        modelId: optionalNullableId,
        modelPath: optionalNullablePath,
        runtime: z.enum(['llama-server', 'ds4']).nullish(),
        port: z.number().int().min(1).max(65_535).nullish(),
        contextSize: positiveInteger.nullish(),
        gpuLayers: z.number().int().nonnegative().safe().nullish(),
      })
      .strict(),
    z
      .object({
        kind: z.literal('model.local.server_stop'),
        modelId: optionalNullableId,
        modelPath: optionalNullablePath,
      })
      .strict(),
    z.object({ kind: z.literal('model.local.server_status') }).strict(),
    z
      .object({
        kind: z.literal('model.local.byo_add'),
        ggufPath: safePath,
        displayName: z.string().min(1).max(240).nullish(),
      })
      .strict(),
    z.object({ kind: z.literal('model.local.byo_remove'), id: safeId }).strict(),
    z
      .object({
        kind: z.literal('model.account.snapshot'),
        forceRefresh: z.boolean(),
      })
      .strict(),
    z
      .object({
        kind: z.literal('model.account.consent'),
        granted: z.boolean(),
      })
      .strict(),
    z.object({ kind: z.literal('model.account.runtime_ensure') }).strict(),
    z.object({ kind: z.literal('model.account.runtime_stop') }).strict(),
    z
      .object({ kind: z.literal('model.account.login_start'), connectorId: safeId })
      .strict(),
    z
      .object({ kind: z.literal('model.account.login_poll'), sessionId: safeId })
      .strict(),
    z
      .object({ kind: z.literal('model.account.login_cancel'), sessionId: safeId })
      .strict(),
    z
      .object({ kind: z.literal('model.account.remove'), accountId: safeId })
      .strict(),
  ],
)

const customListResultSchema = z
  .object({
    kind: z.literal('model.custom.list'),
    entries: z.array(customModelViewSchema).max(10_000),
    version: z.number().int().nonnegative().safe().optional(),
  })
  .strict()

export const directTuiModelManagementResultSchema = z.discriminatedUnion(
  'kind',
  [
    customListResultSchema,
    z
      .object({ kind: z.literal('model.custom.test'), result: customTestResultSchema })
      .strict(),
    z
      .object({
        kind: z.literal('model.local.snapshot'),
        catalog: localCatalogSchema,
        profile: localProfileSchema,
        server: localServerResultSchema,
      })
      .strict(),
    z
      .object({ kind: z.literal('model.local.download'), result: localDownloadResultSchema })
      .strict(),
    z
      .object({
        kind: z.literal('model.local.install_remove'),
        result: localInstallRemoveResultSchema,
      })
      .strict(),
    z
      .object({ kind: z.literal('model.local.server'), result: localServerResultSchema })
      .strict(),
    z
      .object({ kind: z.literal('model.local.byo_add'), entry: localCatalogEntrySchema })
      .strict(),
    z
      .object({ kind: z.literal('model.local.byo_remove'), removed: z.boolean() })
      .strict(),
    z.object({ kind: z.literal('model.account.snapshot'), snapshot: accountSnapshotSchema }).strict(),
    z.object({ kind: z.literal('model.account.consent'), granted: z.boolean() }).strict(),
    z.object({ kind: z.literal('model.account.runtime'), runtime: runtimeSchema }).strict(),
    z
      .object({
        kind: z.literal('model.account.login_start'),
        session: z
          .object({
            sessionId: safeId,
            authMode: z.enum(['browser', 'device-code']),
            authorizationUrl: z.string().max(4_096).nullable(),
            userCode: z.string().max(128).nullable(),
            verificationUrl: z.string().max(4_096).nullable(),
            expiresAt: z.string().max(128).nullable(),
          })
          .strict(),
      })
      .strict(),
    z
      .object({
        kind: z.literal('model.account.login_poll'),
        state: z.enum([
          'pending',
          'succeeded',
          'failed',
          'cancelled',
          'expired',
          'session-lost',
        ]),
        accountId: z.string().max(512).nullable(),
        errorCode: z.string().max(512).nullable(),
      })
      .strict(),
    z.object({ kind: z.literal('model.account.login_cancel'), cancelled: z.boolean() }).strict(),
    z.object({ kind: z.literal('model.account.remove'), removed: z.boolean() }).strict(),
  ],
)

export type DirectTuiModelManagementAction = z.infer<
  typeof directTuiModelManagementActionSchema
>
export type DirectTuiModelManagementResult = z.infer<
  typeof directTuiModelManagementResultSchema
>

export interface DirectTuiModelManagementDeps {
  custom: CustomModelDirectClient
  local: LocalModelDirectClient
  account: Pick<
    AccountBridgeManager,
    | 'eligibilityRead'
    | 'status'
    | 'ensure'
    | 'stop'
    | 'connectorList'
    | 'accountList'
    | 'modelList'
    | 'usageRead'
    | 'loginStart'
    | 'loginPoll'
    | 'loginCancel'
    | 'accountRemove'
  >
  persistAccountConsent(granted: boolean): void
}

let productionDepsPromise: Promise<DirectTuiModelManagementDeps> | undefined

async function productionDeps(): Promise<DirectTuiModelManagementDeps> {
  productionDepsPromise ??= Promise.all([
    import('../services/customModel/directClient.js'),
    import('../services/localModel/directClient.js'),
    import('../services/accountBridge/runtimeManager.js'),
    import('../utils/settings/settings.js'),
  ]).then(([custom, local, account, settings]) => ({
    custom: custom.createDefaultCustomModelDirectClient(),
    local: local.createDefaultLocalModelDirectClient(),
    account: account.getAccountBridgeManager(),
    persistAccountConsent(granted: boolean): void {
      const result = settings.updateSettingsForSource('userSettings', {
        accountBridge: {
          eligibilityConsent: granted
            ? { grantedAtIso: new Date().toISOString(), noticeVersion: 1 }
            : null,
        },
      })
      if (result.error) throw result.error
    },
  }))
  return productionDepsPromise
}

function customView(entry: CustomModelEntry): z.infer<typeof customModelViewSchema> {
  return {
    id: entry.id,
    brand: entry.brand,
    protocol: entry.protocol,
    baseUrl: entry.baseUrl,
    modelId: entry.modelId,
    ...(entry.displayName === undefined ? {} : { displayName: entry.displayName }),
    contextWindow: entry.contextWindow,
    maxOutputTokens: entry.maxOutputTokens,
    ...(entry.supportsThinking === undefined
      ? {}
      : { supportsThinking: entry.supportsThinking }),
    ...(entry.supportsTools === undefined ? {} : { supportsTools: entry.supportsTools }),
    ...(entry.supportsJsonMode === undefined
      ? {}
      : { supportsJsonMode: entry.supportsJsonMode }),
    ...(entry.supportsVision === undefined
      ? {}
      : { supportsVision: entry.supportsVision }),
    enabled: entry.enabled,
    ...(entry.isDefault === undefined ? {} : { isDefault: entry.isDefault }),
    hasStoredCredential:
      typeof entry.apiKeyHandle === 'string' && entry.apiKeyHandle.length > 0,
    modelReference: renderCustomModelReference(entry.id),
  }
}

function localEntryWithReference(entry: {
  id: string
  displayName: string
  description: string | null
  runtime: 'llama-server' | 'ds4'
  protocol: 'openai-compatible'
  format: 'gguf'
  source: 'curated' | 'user-local-path'
  license: string | null
  sizeBytes: number | null
  sha256: string | null
  installed: boolean
  status: string
  modelPath: string | null
  reason: string | null
}): z.infer<typeof localCatalogEntrySchema> {
  return { ...entry, modelReference: renderLocalModelReference(entry.id) }
}

function accountRouteWithReference(
  route: AccountBridgeModelRouteView,
): z.infer<typeof accountRouteSchema> {
  return { ...route, modelReference: renderAccountBridgeReference(route.routeId) }
}

async function accountSnapshot(
  forceRefresh: boolean,
  account: DirectTuiModelManagementDeps['account'],
): Promise<z.infer<typeof accountSnapshotSchema>> {
  const eligibility = await account.eligibilityRead(forceRefresh)
  let connectors: AccountBridgeConnectorView[] = []
  if (eligibility.state === 'blocked-cn') {
    try {
      connectors = await account.connectorList()
    } catch {
      connectors = []
    }
  }
  if (!accountBridgeProtectedReadsPermitted(eligibility, connectors)) {
    return {
      eligibility,
      runtime: null,
      connectors,
      accounts: [],
      routes: [],
      usage: [],
    }
  }
  const [runtime, resolvedConnectors] = await Promise.all([
    Promise.resolve(account.status()),
    eligibility.state === 'blocked-cn'
      ? Promise.resolve(connectors)
      : account.connectorList(),
  ])
  if (runtime.state !== 'ready') {
    return {
      eligibility,
      runtime,
      connectors: resolvedConnectors,
      accounts: [],
      routes: [],
      usage: [],
    }
  }
  const [accounts, routes, usage] = await Promise.all([
    account.accountList(),
    account.modelList(),
    account.usageRead(null, forceRefresh),
  ])
  return {
    eligibility,
    runtime,
    connectors: resolvedConnectors,
    accounts,
    routes: routes.map(accountRouteWithReference),
    usage,
  }
}

export async function handleDirectTuiModelManagementAction(
  rawAction: unknown,
  deps?: DirectTuiModelManagementDeps,
): Promise<DirectTuiModelManagementResult> {
  const action = directTuiModelManagementActionSchema.parse(rawAction)
  const authority = deps ?? (await productionDeps())
  let result: DirectTuiModelManagementResult

  switch (action.kind) {
    case 'model.custom.list':
      result = {
        kind: 'model.custom.list',
        entries: (await authority.custom.list()).map(customView),
      }
      break
    case 'model.custom.add': {
      const mutation = await authority.custom.add(action.input)
      result = {
        kind: 'model.custom.list',
        entries: mutation.entries.map(customView),
        version: mutation.version,
      }
      break
    }
    case 'model.custom.update': {
      const mutation = await authority.custom.update(action.id, action.input)
      result = {
        kind: 'model.custom.list',
        entries: mutation.entries.map(customView),
        version: mutation.version,
      }
      break
    }
    case 'model.custom.remove': {
      const mutation = await authority.custom.remove(action.id)
      result = {
        kind: 'model.custom.list',
        entries: mutation.entries.map(customView),
        version: mutation.version,
      }
      break
    }
    case 'model.custom.toggle': {
      const mutation = await authority.custom.toggle(action.id, action.enabled)
      result = {
        kind: 'model.custom.list',
        entries: mutation.entries.map(customView),
        version: mutation.version,
      }
      break
    }
    case 'model.custom.test_saved': {
      const entry = (await authority.custom.list()).find(candidate => candidate.id === action.id)
      if (!entry) throw new Error('custom-model-entry-not-found')
      result = {
        kind: 'model.custom.test',
        result: await authority.custom.testConnection({
          baseUrl: entry.baseUrl,
          protocol: entry.protocol,
          modelId: entry.modelId,
          ...(entry.apiKeyHandle ? { apiKeyHandle: entry.apiKeyHandle } : {}),
        }),
      }
      break
    }
    case 'model.custom.test_draft':
      result = {
        kind: 'model.custom.test',
        result: await authority.custom.testConnection({
          baseUrl: action.baseUrl,
          protocol: action.protocol,
          modelId: action.modelId,
          apiKey: action.apiKey,
        }),
      }
      break
    case 'model.local.snapshot': {
      const [catalog, profile, server] = await Promise.all([
        authority.local.catalogRead(),
        authority.local.systemProfileRead(),
        authority.local.serverStatus(),
      ])
      result = {
        kind: 'model.local.snapshot',
        catalog: {
          ...catalog,
          data: catalog.data.map(localEntryWithReference),
        },
        profile,
        server,
      }
      break
    }
    case 'model.local.download_start':
      result = {
        kind: 'model.local.download',
        result: await authority.local.downloadStart({ modelId: action.modelId }),
      }
      break
    case 'model.local.download_progress':
      result = {
        kind: 'model.local.download',
        result: await authority.local.downloadProgress({
          downloadId: action.downloadId,
          modelId: action.modelId,
        }),
      }
      break
    case 'model.local.download_cancel':
      result = {
        kind: 'model.local.download',
        result: await authority.local.downloadCancel({
          downloadId: action.downloadId,
          modelId: action.modelId,
        }),
      }
      break
    case 'model.local.install_remove':
      result = {
        kind: 'model.local.install_remove',
        result: await authority.local.installRemove({
          modelId: action.modelId,
          modelPath: action.modelPath,
          removeFiles: action.removeFiles,
        }),
      }
      break
    case 'model.local.server_start':
      result = {
        kind: 'model.local.server',
        result: await authority.local.serverStart({
          modelId: action.modelId,
          modelPath: action.modelPath,
          runtime: action.runtime,
          port: action.port,
          contextSize: action.contextSize,
          gpuLayers: action.gpuLayers,
        }),
      }
      break
    case 'model.local.server_stop':
      result = {
        kind: 'model.local.server',
        result: await authority.local.serverStop({
          modelId: action.modelId,
          modelPath: action.modelPath,
        }),
      }
      break
    case 'model.local.server_status':
      result = {
        kind: 'model.local.server',
        result: await authority.local.serverStatus(),
      }
      break
    case 'model.local.byo_add': {
      const response = await authority.local.byoAdd({
        ggufPath: action.ggufPath,
        displayName: action.displayName,
      })
      result = {
        kind: 'model.local.byo_add',
        entry: localEntryWithReference(response.entry),
      }
      break
    }
    case 'model.local.byo_remove': {
      const response = await authority.local.byoRemove({ id: action.id })
      result = { kind: 'model.local.byo_remove', removed: response.removed }
      break
    }
    case 'model.account.snapshot':
      result = {
        kind: 'model.account.snapshot',
        snapshot: await accountSnapshot(action.forceRefresh, authority.account),
      }
      break
    case 'model.account.consent':
      authority.persistAccountConsent(action.granted)
      result = { kind: 'model.account.consent', granted: action.granted }
      break
    case 'model.account.runtime_ensure':
      result = {
        kind: 'model.account.runtime',
        runtime: await authority.account.ensure(true),
      }
      break
    case 'model.account.runtime_stop':
      result = {
        kind: 'model.account.runtime',
        runtime: await authority.account.stop(),
      }
      break
    case 'model.account.login_start':
      result = {
        kind: 'model.account.login_start',
        session: await authority.account.loginStart(action.connectorId),
      }
      break
    case 'model.account.login_poll': {
      const response = await authority.account.loginPoll(action.sessionId)
      result = { kind: 'model.account.login_poll', ...response }
      break
    }
    case 'model.account.login_cancel': {
      const response = await authority.account.loginCancel(action.sessionId)
      result = { kind: 'model.account.login_cancel', cancelled: response.cancelled }
      break
    }
    case 'model.account.remove': {
      const response = await authority.account.accountRemove(action.accountId)
      result = { kind: 'model.account.remove', removed: response.removed }
      break
    }
  }

  return directTuiModelManagementResultSchema.parse(result)
}
