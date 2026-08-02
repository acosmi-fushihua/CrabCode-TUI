import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import {
  directTuiModelManagementActionSchema,
  directTuiModelManagementResultSchema,
  handleDirectTuiModelManagementAction,
  type DirectTuiModelManagementDeps,
} from '../../src/cli/directTuiModelManagementActions.js'
import type { CustomModelEntry } from '../../src/services/customModel/directClient.js'

const routeId = 'a'.repeat(43)

function customEntry(overrides: Partial<CustomModelEntry> = {}): CustomModelEntry {
  return {
    id: 'custom-entry-1',
    brand: 'custom',
    protocol: 'openai-compatible',
    baseUrl: 'https://models.example.test/v1',
    apiKeyHandle: 'credential-handle-never-project',
    modelId: 'private-model',
    displayName: 'Private model',
    contextWindow: 32_768,
    maxOutputTokens: 4_096,
    supportsThinking: true,
    supportsTools: true,
    supportsJsonMode: true,
    supportsVision: false,
    enabled: true,
    ...overrides,
  }
}

function localEntry() {
  return {
    id: 'local-one',
    displayName: 'Local One',
    description: 'local model',
    runtime: 'llama-server' as const,
    protocol: 'openai-compatible' as const,
    format: 'gguf' as const,
    source: 'curated' as const,
    license: 'Apache-2.0',
    sizeBytes: 1024,
    sha256: 'a'.repeat(64),
    installed: true,
    status: 'installed',
    modelPath: '/tmp/local-one.gguf',
    reason: null,
  }
}

function deps(): DirectTuiModelManagementDeps & {
  calls: Array<{ method: string; value?: unknown }>
} {
  const calls: Array<{ method: string; value?: unknown }> = []
  const entry = customEntry()
  return {
    calls,
    custom: {
      async list() {
        calls.push({ method: 'custom.list' })
        return [entry]
      },
      async add(input) {
        calls.push({ method: 'custom.add', value: input })
        return {
          applied: { type: 'addCustomModel', ...input },
          version: 2,
          entries: [entry],
        }
      },
      async update(id, input) {
        calls.push({ method: 'custom.update', value: { id, input } })
        return {
          applied: { type: 'updateCustomModel', id, ...input },
          version: 3,
          entries: [entry],
        }
      },
      async remove(id) {
        calls.push({ method: 'custom.remove', value: id })
        return {
          applied: { type: 'removeCustomModel', id },
          version: 4,
          entries: [],
        }
      },
      async toggle(id, enabled) {
        calls.push({ method: 'custom.toggle', value: { id, enabled } })
        return {
          applied: { type: 'toggleCustomModel', id, enabled },
          version: 5,
          entries: [entry],
        }
      },
      async testConnection(params) {
        calls.push({ method: 'custom.test', value: params })
        return { ok: true, httpStatus: 200, latencyMs: 12 }
      },
    },
    local: {
      async catalogRead() {
        calls.push({ method: 'local.catalogRead' })
        return {
          data: [localEntry()],
          source: 'curated',
          manifestStatus: 'awaiting-license-signoff',
          manifestVersion: 1,
        }
      },
      async systemProfileRead() {
        calls.push({ method: 'local.systemProfileRead' })
        return {
          platform: 'darwin',
          arch: 'arm64',
          memoryBytes: 64 * 1024 * 1024 * 1024,
          recommendedRuntime: 'llama-server',
          supportedRuntimes: [
            {
              id: 'llama-server',
              displayName: 'llama-server',
              supported: true,
              acceleration: 'metal',
              reason: 'supported',
            },
          ],
        }
      },
      async downloadStart(value) {
        calls.push({ method: 'local.downloadStart', value })
        return {
          status: {
            state: 'queued',
            reason: null,
            downloadId: 'download-1',
            modelId: value.modelId,
            bytesReceived: 0,
            totalBytes: 1024,
            percentage: 0,
            error: null,
          },
        }
      },
      async downloadProgress(value) {
        calls.push({ method: 'local.downloadProgress', value })
        return {
          status: {
            state: 'downloading',
            reason: null,
            downloadId: value.downloadId ?? null,
            modelId: value.modelId ?? null,
            bytesReceived: 512,
            totalBytes: 1024,
            percentage: 50,
            error: null,
          },
        }
      },
      async downloadCancel(value) {
        calls.push({ method: 'local.downloadCancel', value })
        return {
          status: {
            state: 'cancelled',
            reason: null,
            downloadId: value.downloadId ?? null,
            modelId: value.modelId ?? null,
            bytesReceived: 512,
            totalBytes: 1024,
            percentage: 50,
            error: null,
          },
        }
      },
      async installRemove(value) {
        calls.push({ method: 'local.installRemove', value })
        return {
          state: 'removed',
          reason: null,
          modelId: value.modelId ?? null,
          modelPath: value.modelPath ?? null,
        }
      },
      async serverStart(value) {
        calls.push({ method: 'local.serverStart', value })
        return {
          status: {
            state: 'running',
            host: '127.0.0.1',
            port: value.port ?? 38080,
            url: 'http://127.0.0.1:38080',
            pid: 1234,
            modelId: value.modelId ?? undefined,
            modelPath: value.modelPath ?? undefined,
          },
        }
      },
      async serverStop(value) {
        calls.push({ method: 'local.serverStop', value })
        return { status: { state: 'stopped' } }
      },
      async serverStatus() {
        calls.push({ method: 'local.serverStatus' })
        return { status: { state: 'stopped' } }
      },
      async byoAdd(value) {
        calls.push({ method: 'local.byoAdd', value })
        return {
          entry: {
            ...localEntry(),
            id: 'byo-one',
            source: 'user-local-path',
            modelPath: value.ggufPath,
          },
        }
      },
      async byoRemove(value) {
        calls.push({ method: 'local.byoRemove', value })
        return { removed: true }
      },
    },
    account: {
      status() {
        calls.push({ method: 'account.status' })
        return {
          state: 'ready',
          componentVersion: '1.0.0',
          protocolVersion: 1,
          lastErrorCode: null,
        }
      },
      async eligibilityRead(forceRefresh) {
        calls.push({ method: 'account.eligibilityRead', value: forceRefresh })
        return {
          state: 'allowed',
          countryCode: 'US',
          policyVersion: '1',
          checkedAt: '2026-08-01T00:00:00.000Z',
          expiresAt: '2026-08-01T00:05:00.000Z',
          reasonCode: null,
        }
      },
      async ensure(explicitRetry) {
        calls.push({ method: 'account.ensure', value: explicitRetry })
        return this.status()
      },
      async stop() {
        calls.push({ method: 'account.stop' })
        return {
          state: 'stopped',
          componentVersion: '1.0.0',
          protocolVersion: 1,
          lastErrorCode: null,
        }
      },
      async connectorList() {
        calls.push({ method: 'account.connectorList' })
        return [
          {
            connectorId: 'connector-one',
            displayName: 'Connector One',
            authMode: 'browser',
            enabled: true,
            disabledReasonCode: null,
            termsStatus: 'signed-off',
          },
        ]
      },
      async accountList() {
        calls.push({ method: 'account.accountList' })
        return [
          {
            accountId: 'account-one',
            connectorId: 'connector-one',
            displayLabel: 'Account One',
            status: 'ready',
            connectedAt: '2026-08-01T00:00:00.000Z',
            lastUsedAt: null,
            cooldownUntil: null,
          },
        ]
      },
      async modelList() {
        calls.push({ method: 'account.modelList' })
        return [
          {
            routeId,
            accountId: 'account-one',
            connectorId: 'connector-one',
            modelId: 'provider-model',
            displayName: 'Provider Model',
            connectorLabel: 'Connector One',
            accountLabel: 'Account One',
            chatRuntimeSupported: true,
            supportsTools: true,
            supportsThinking: true,
            supportsAdaptiveThinking: false,
            supportsEffort: true,
            supportsMaxEffort: false,
            supportsVision: true,
            supportsJsonMode: true,
            supportedThinkingModes: ['auto', 'standard'],
            defaultThinkingMode: 'auto',
            contextWindow: 32_768,
            maxOutputTokens: 4_096,
          },
        ]
      },
      async usageRead(routeIds, forceRefresh) {
        calls.push({ method: 'account.usageRead', value: { routeIds, forceRefresh } })
        return [
          {
            routeId,
            accountId: 'account-one',
            state: 'available',
            remainingPercent: 80,
            limitingWindowLabel: 'weekly',
            resetsAt: null,
            windows: [],
            observedAt: '2026-08-01T00:00:00.000Z',
          },
        ]
      },
      async loginStart(connectorId) {
        calls.push({ method: 'account.loginStart', value: connectorId })
        return {
          sessionId: 'login-session',
          authMode: 'browser',
          authorizationUrl: 'https://auth.example.test/start',
          userCode: null,
          verificationUrl: null,
          expiresAt: null,
        }
      },
      async loginPoll(sessionId) {
        calls.push({ method: 'account.loginPoll', value: sessionId })
        return { state: 'pending', accountId: null, errorCode: null }
      },
      async loginCancel(sessionId) {
        calls.push({ method: 'account.loginCancel', value: sessionId })
        return { cancelled: true }
      },
      async accountRemove(accountId) {
        calls.push({ method: 'account.accountRemove', value: accountId })
        return { removed: true }
      },
    },
    persistAccountConsent(granted) {
      calls.push({ method: 'account.consent', value: granted })
    },
  }
}

describe('direct TUI model-management actions', () => {
  test('schemas are closed and reject unknown actions or extra fields', () => {
    expect(() =>
      directTuiModelManagementActionSchema.parse({
        kind: 'model.custom.list',
        arbitraryMethod: 'config/read',
      }),
    ).toThrow()
    expect(() =>
      directTuiModelManagementActionSchema.parse({ kind: 'future.action' }),
    ).toThrow()
    expect(() =>
      directTuiModelManagementResultSchema.parse({
        kind: 'model.account.consent',
        granted: true,
        inferenceKey: 'must-not-pass',
      }),
    ).toThrow()
  })

  test('custom list and mutations never project API keys or credential handles', async () => {
    const authority = deps()
    const listed = await handleDirectTuiModelManagementAction(
      { kind: 'model.custom.list' },
      authority,
    )
    const added = await handleDirectTuiModelManagementAction(
      {
        kind: 'model.custom.add',
        input: {
          brand: 'custom',
          protocol: 'openai-compatible',
          baseUrl: 'https://models.example.test/v1',
          modelId: 'private-model',
          apiKey: 'raw-secret-never-return',
          contextWindow: 32_768,
          maxOutputTokens: 4_096,
          enabled: true,
        },
      },
      authority,
    )

    expect(listed).toMatchObject({
      kind: 'model.custom.list',
      entries: [
        {
          id: 'custom-entry-1',
          hasStoredCredential: true,
          modelReference: 'custom:custom-entry-1',
        },
      ],
    })
    expect(JSON.stringify([listed, added])).not.toContain('raw-secret-never-return')
    expect(JSON.stringify([listed, added])).not.toContain(
      'credential-handle-never-project',
    )
  })

  test('saved connection test resolves the credential handle inside the authority process', async () => {
    const authority = deps()
    const result = await handleDirectTuiModelManagementAction(
      { kind: 'model.custom.test_saved', id: 'custom-entry-1' },
      authority,
    )
    expect(result).toEqual({
      kind: 'model.custom.test',
      result: { ok: true, httpStatus: 200, latencyMs: 12 },
    })
    expect(authority.calls).toContainEqual({
      method: 'custom.test',
      value: {
        baseUrl: 'https://models.example.test/v1',
        protocol: 'openai-compatible',
        modelId: 'private-model',
        apiKeyHandle: 'credential-handle-never-project',
      },
    })
    expect(JSON.stringify(result)).not.toContain('credential-handle-never-project')
  })

  test('local snapshot and lifecycle call the existing direct client without a second executor', async () => {
    const authority = deps()
    const snapshot = await handleDirectTuiModelManagementAction(
      { kind: 'model.local.snapshot' },
      authority,
    )
    const started = await handleDirectTuiModelManagementAction(
      {
        kind: 'model.local.server_start',
        modelId: 'local-one',
        modelPath: null,
        runtime: 'llama-server',
        port: 38_080,
        contextSize: 8_192,
        gpuLayers: 32,
      },
      authority,
    )
    const cancelled = await handleDirectTuiModelManagementAction(
      {
        kind: 'model.local.download_cancel',
        downloadId: 'download-1',
        modelId: 'local-one',
      },
      authority,
    )

    expect(snapshot).toMatchObject({
      kind: 'model.local.snapshot',
      catalog: { data: [{ modelReference: 'local:local-one' }] },
    })
    expect(started).toMatchObject({
      kind: 'model.local.server',
      result: { status: { state: 'running', port: 38_080 } },
    })
    expect(cancelled).toMatchObject({
      kind: 'model.local.download',
      result: { status: { state: 'cancelled' } },
    })
    expect(authority.calls).toContainEqual({
      method: 'local.serverStart',
      value: {
        modelId: 'local-one',
        modelPath: null,
        runtime: 'llama-server',
        port: 38_080,
        contextSize: 8_192,
        gpuLayers: 32,
      },
    })
  })

  test('account snapshot follows the existing consent/protected-read gate and returns opaque references', async () => {
    const authority = deps()
    const snapshot = await handleDirectTuiModelManagementAction(
      { kind: 'model.account.snapshot', forceRefresh: true },
      authority,
    )
    expect(snapshot).toMatchObject({
      kind: 'model.account.snapshot',
      snapshot: {
        eligibility: { state: 'allowed' },
        runtime: { state: 'ready' },
        routes: [{ routeId, modelReference: `account:${routeId}` }],
      },
    })
    expect(authority.calls).toContainEqual({
      method: 'account.usageRead',
      value: { routeIds: null, forceRefresh: true },
    })

    const locked = deps()
    locked.account.eligibilityRead = async () => ({
      state: 'unavailable',
      countryCode: null,
      policyVersion: null,
      checkedAt: null,
      expiresAt: null,
      reasonCode: 'consent-required',
    })
    const lockedSnapshot = await handleDirectTuiModelManagementAction(
      { kind: 'model.account.snapshot', forceRefresh: false },
      locked,
    )
    expect(lockedSnapshot).toEqual({
      kind: 'model.account.snapshot',
      snapshot: {
        eligibility: {
          state: 'unavailable',
          countryCode: null,
          policyVersion: null,
          checkedAt: null,
          expiresAt: null,
          reasonCode: 'consent-required',
        },
        runtime: null,
        connectors: [],
        accounts: [],
        routes: [],
        usage: [],
      },
    })
    expect(locked.calls.some(call => call.method === 'account.status')).toBe(false)
    expect(locked.calls.some(call => call.method === 'account.accountList')).toBe(false)
  })

  test('consent/runtime/login/remove map only to the existing account manager methods', async () => {
    const authority = deps()
    await handleDirectTuiModelManagementAction(
      { kind: 'model.account.consent', granted: true },
      authority,
    )
    await handleDirectTuiModelManagementAction(
      { kind: 'model.account.runtime_ensure' },
      authority,
    )
    await handleDirectTuiModelManagementAction(
      { kind: 'model.account.login_start', connectorId: 'connector-one' },
      authority,
    )
    await handleDirectTuiModelManagementAction(
      { kind: 'model.account.login_poll', sessionId: 'login-session' },
      authority,
    )
    await handleDirectTuiModelManagementAction(
      { kind: 'model.account.login_cancel', sessionId: 'login-session' },
      authority,
    )
    await handleDirectTuiModelManagementAction(
      { kind: 'model.account.remove', accountId: 'account-one' },
      authority,
    )

    expect(authority.calls.map(call => call.method)).toEqual([
      'account.consent',
      'account.ensure',
      'account.status',
      'account.loginStart',
      'account.loginPoll',
      'account.loginCancel',
      'account.accountRemove',
    ])
  })

  test('source boundary is direct-only and does not import AppServer or public schemas', () => {
    const source = readFileSync(
      resolve(process.cwd(), 'src/cli/directTuiModelManagementActions.ts'),
      'utf8',
    )
    expect(source).toContain("../services/customModel/directClient.js")
    expect(source).toContain("../services/localModel/directClient.js")
    expect(source).toContain("../services/accountBridge/runtimeManager.js")
    expect(source).not.toContain('appServer')
    expect(source).not.toContain('control_request')
    expect(source).not.toContain('jsonrpc')
  })
})
