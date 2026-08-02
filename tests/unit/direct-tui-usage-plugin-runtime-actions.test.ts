import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import {
  handleUsagePluginRuntimeAction,
  parseUsagePluginRuntimeAction,
  USAGE_PLUGIN_RUNTIME_ACTION_KINDS,
  USAGE_PLUGIN_RUNTIME_RESULT_KINDS,
  UsagePluginRuntimeActionSchema,
  type UsagePluginRuntimeDependencies,
  UsagePluginRuntimeResultSchema,
} from '../../src/cli/directTuiUsagePluginRuntimeActions.js'

type Dependencies = UsagePluginRuntimeDependencies

function createDependencies(
  overrides: Partial<Dependencies> = {},
): { dependencies: Dependencies; reported: unknown[] } {
  const reported: unknown[] = []
  const dependencies: Dependencies = {
    async fetchUtilization() {
      return null
    },
    async fetchEntitlementBalance() {
      return null
    },
    async setFiveHourContinuePreference(enabled) {
      return enabled
    },
    async readPluginInventory() {
      return {
        installed: {},
        loadedEnabled: [],
        loadedDisabled: [],
        loadErrors: [],
        configuredScopes: new Map(),
        blockedPluginIds: new Set(),
      }
    },
    async readMarketplaceInventory() {
      return {
        marketplaces: [],
        failedMarketplaceNames: new Set(),
        loadedPlugins: [],
        emptyReason: 'no-marketplaces-configured',
        autoUpdateFor: () => false,
      }
    },
    async readMarketplaceCatalog(marketplaceName) {
      return {
        marketplaceName,
        marketplace: { name: marketplaceName, plugins: [] },
        installed: {},
        globallyInstalledPluginIds: new Set(),
        enabledPluginIds: new Set(),
        blockedPluginIds: new Set(),
        installCounts: null,
      }
    },
    async installPlugin(pluginId, scope) {
      return {
        success: true,
        message: 'installed',
        pluginId,
        pluginName: pluginId.split('@')[0],
        scope,
      }
    },
    async uninstallPlugin(pluginId, scope) {
      return {
        success: true,
        message: 'uninstalled',
        pluginId,
        pluginName: pluginId.split('@')[0],
        scope,
      }
    },
    async setPluginEnabled(pluginId, _enabled, scope) {
      return {
        success: true,
        message: 'updated',
        pluginId,
        pluginName: pluginId.split('@')[0],
        scope: scope ?? 'user',
      }
    },
    async updatePlugin(pluginId, scope) {
      return {
        success: true,
        message: 'updated',
        pluginId,
        scope,
        oldVersion: '1.0.0',
        newVersion: '1.1.0',
      }
    },
    async parseMarketplaceInput() {
      return { source: 'github', repo: 'acosmi/example-marketplace' }
    },
    async addMarketplace(source) {
      return {
        name: 'example-marketplace',
        alreadyMaterialized: false,
        resolvedSource: source,
      }
    },
    saveMarketplaceDeclaration() {},
    async removeMarketplace() {},
    async refreshMarketplace() {},
    async updatePluginsForMarketplaces() {
      return { updatedPluginIds: [], failures: [] }
    },
    async setMarketplaceAutoUpdate() {},
    clearPluginCaches() {},
    async validatePlugin(path) {
      return [
        {
          success: true,
          errors: [],
          warnings: [],
          filePath: path,
          fileType: 'plugin',
        },
      ]
    },
    reportError(error) {
      reported.push(error)
    },
    ...overrides,
  }
  return { dependencies, reported }
}

describe('direct TUI usage/plugin action value boundary', () => {
  test('pins the closed action and result kind sets', () => {
    expect(USAGE_PLUGIN_RUNTIME_ACTION_KINDS).toEqual([
      'usage_read',
      'usage_set_five_hour_continue',
      'plugin_inventory_read',
      'plugin_marketplace_inventory_read',
      'plugin_marketplace_catalog_read',
      'plugin_install',
      'plugin_uninstall',
      'plugin_set_enabled',
      'plugin_update',
      'plugin_marketplace_add',
      'plugin_marketplace_remove',
      'plugin_marketplace_update',
      'plugin_marketplace_set_auto_update',
      'plugin_validate',
    ])
    expect(USAGE_PLUGIN_RUNTIME_RESULT_KINDS).toEqual([
      'usage_snapshot',
      'usage_five_hour_continue_updated',
      'plugin_inventory_snapshot',
      'plugin_marketplace_inventory_snapshot',
      'plugin_marketplace_catalog_snapshot',
      'plugin_installed',
      'plugin_uninstalled',
      'plugin_enabled_state_updated',
      'plugin_updated',
      'plugin_marketplace_added',
      'plugin_marketplace_removed',
      'plugin_marketplace_updated',
      'plugin_marketplace_auto_update_updated',
      'plugin_validation_result',
      'usage_plugin_error',
    ])
    expect(new Set(USAGE_PLUGIN_RUNTIME_RESULT_KINDS).size).toBe(
      USAGE_PLUGIN_RUNTIME_RESULT_KINDS.length,
    )
  })

  test('rejects unknown, widened, malformed, and unbounded actions', () => {
    expect(parseUsagePluginRuntimeAction({ kind: 'plugin_magic' })).toBeNull()
    expect(
      parseUsagePluginRuntimeAction({ kind: 'usage_read', extra: true }),
    ).toBeNull()
    expect(
      parseUsagePluginRuntimeAction({
        kind: 'plugin_install',
        plugin_id: '../escape',
        scope: 'user',
      }),
    ).toBeNull()
    expect(
      parseUsagePluginRuntimeAction({
        kind: 'plugin_marketplace_remove',
        marketplace_name: 'name/with/path',
      }),
    ).toBeNull()
    expect(
      parseUsagePluginRuntimeAction({
        kind: 'plugin_marketplace_add',
        source_input: `source\nsecond-line`,
      }),
    ).toBeNull()
    expect(
      parseUsagePluginRuntimeAction({
        kind: 'plugin_validate',
        path: 'x'.repeat(4097),
      }),
    ).toBeNull()
    expect(
      parseUsagePluginRuntimeAction({
        kind: 'plugin_set_enabled',
        plugin_id: 'demo@market',
        enabled: true,
      }),
    ).toBeNull()
  })

  test('normalizes bounded strings and preserves explicit null scope', () => {
    expect(
      UsagePluginRuntimeActionSchema.parse({
        kind: 'plugin_set_enabled',
        plugin_id: ' demo@market ',
        enabled: false,
        scope: null,
      }),
    ).toEqual({
      kind: 'plugin_set_enabled',
      plugin_id: 'demo@market',
      enabled: false,
      scope: null,
    })
  })

  test('keeps result objects and nested rows strict', () => {
    expect(
      UsagePluginRuntimeResultSchema.safeParse({
        kind: 'usage_five_hour_continue_updated',
        enabled: true,
        extra: true,
      }).success,
    ).toBe(false)
    expect(
      UsagePluginRuntimeResultSchema.safeParse({
        kind: 'plugin_inventory_snapshot',
        plugins: [
          {
            id: 'demo@market',
            name: 'demo',
            marketplace: 'market',
            description: null,
            version: null,
            is_builtin: false,
            loaded: false,
            enabled: false,
            configured_scope: null,
            installations: [],
            secret: 'must-not-pass',
          },
        ],
        load_diagnostics: [],
        truncated: false,
      }).success,
    ).toBe(false)
  })
})

describe('direct TUI usage authority adapter', () => {
  test('preserves partial success and maps every absent field to null', async () => {
    const secret = 'balance-secret-must-not-cross-wire'
    const { dependencies, reported } = createDependencies({
      async fetchUtilization() {
        return {
          five_hour: {
            utilization: 37.5,
            resets_at: '2026-08-02T00:00:00Z',
            overridable: true,
          },
        }
      },
      async fetchEntitlementBalance() {
        throw new Error(secret)
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'usage_read' },
      dependencies,
    )

    expect(result).toEqual({
      kind: 'usage_snapshot',
      utilization: {
        five_hour: {
          utilization: 37.5,
          resets_at: '2026-08-02T00:00:00Z',
          overridable: true,
        },
        seven_day: null,
        extra_usage: null,
        five_hour_continue_enabled: null,
      },
      entitlement_balance: null,
    })
    expect(JSON.stringify(result)).not.toContain(secret)
    expect(reported).toEqual([])
  })

  test('maps the complete entitlement authority shape without inventing fields', async () => {
    const { dependencies } = createDependencies({
      async fetchEntitlementBalance() {
        return {
          totalTokenQuota: 100,
          totalTokenUsed: 25,
          totalTokenRemaining: 75,
          totalCallQuota: 20,
          totalCallUsed: 5,
          totalCallRemaining: 15,
          activeEntitlements: 2,
        }
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'usage_read' },
      dependencies,
    )
    expect(result).toEqual({
      kind: 'usage_snapshot',
      utilization: null,
      entitlement_balance: {
        total_token_quota: 100,
        total_token_used: 25,
        total_token_remaining: 75,
        total_call_quota: 20,
        total_call_used: 5,
        total_call_remaining: 15,
        active_entitlements: 2,
      },
    })
  })

  test('preserves the entitlement authority negative call-quota sentinel', async () => {
    const { dependencies, reported } = createDependencies({
      async fetchEntitlementBalance() {
        return {
          totalTokenQuota: 100,
          totalTokenUsed: 25,
          totalTokenRemaining: 75,
          totalCallQuota: -1,
          totalCallUsed: 0,
          totalCallRemaining: -1,
          activeEntitlements: 1,
        }
      },
    })

    await expect(
      handleUsagePluginRuntimeAction({ kind: 'usage_read' }, dependencies),
    ).resolves.toEqual({
      kind: 'usage_snapshot',
      utilization: null,
      entitlement_balance: {
        total_token_quota: 100,
        total_token_used: 25,
        total_token_remaining: 75,
        total_call_quota: -1,
        total_call_used: 0,
        total_call_remaining: -1,
        active_entitlements: 1,
      },
    })
    expect(reported).toEqual([])
  })

  test('returns a typed safe error when both authorities fail', async () => {
    const secret = 'oauth-token-should-never-be-rendered'
    const { dependencies, reported } = createDependencies({
      async fetchUtilization() {
        throw new Error(secret)
      },
      async fetchEntitlementBalance() {
        throw new Error(`second-${secret}`)
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'usage_read' },
      dependencies,
    )
    expect(result).toEqual({
      kind: 'usage_plugin_error',
      action_kind: 'usage_read',
      code: 'usage_unavailable',
      message: 'Usage data is unavailable.',
    })
    expect(JSON.stringify(result)).not.toContain(secret)
    expect(reported).toHaveLength(2)
  })

  test('delegates the five-hour preference and returns the authority echo', async () => {
    const calls: boolean[] = []
    const { dependencies } = createDependencies({
      async setFiveHourContinuePreference(enabled) {
        calls.push(enabled)
        return false
      },
    })
    await expect(
      handleUsagePluginRuntimeAction(
        { kind: 'usage_set_five_hour_continue', enabled: true },
        dependencies,
      ),
    ).resolves.toEqual({
      kind: 'usage_five_hour_continue_updated',
      enabled: false,
    })
    expect(calls).toEqual([true])
  })
})

describe('direct TUI plugin read adapters', () => {
  test('merges installed and loaded inventory and omits blocked plugins and raw diagnostics', async () => {
    const { dependencies } = createDependencies({
      async readPluginInventory() {
        return {
          installed: {
            'installed@community': [
              {
                scope: 'project',
                version: '1.0.0',
                installedAt: 'today',
                lastUpdated: 'today',
              },
            ],
            'blocked@community': [{ scope: 'user' }],
          },
          loadedEnabled: [
            {
              name: 'enabled',
              source: 'plugin@community',
              manifest: {
                description: '\u001b[31mEnabled plugin\u001b[0m',
                version: '2.0.0',
              },
            },
          ],
          loadedDisabled: [
            {
              name: 'builtin',
              source: 'plugin@builtin',
              isBuiltin: true,
              manifest: {},
            },
          ],
          loadErrors: [
            {
              type: 'network-error',
              plugin: 'safe-name',
              message: 'credential=must-not-cross-wire',
            } as never,
          ],
          configuredScopes: new Map([
            ['enabled@community', 'local' as const],
          ]),
          blockedPluginIds: new Set(['blocked@community']),
        }
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'plugin_inventory_read' },
      dependencies,
    )
    expect(result.kind).toBe('plugin_inventory_snapshot')
    if (result.kind !== 'plugin_inventory_snapshot') return
    expect(result.plugins.map(plugin => plugin.id)).toEqual([
      'builtin@builtin',
      'enabled@community',
      'installed@community',
    ])
    expect(result.plugins[1]).toMatchObject({
      description: 'Enabled plugin',
      enabled: true,
      loaded: true,
      configured_scope: 'local',
    })
    expect(result.plugins[2]?.installations).toEqual([
      {
        scope: 'project',
        version: '1.0.0',
        installed_at: 'today',
        last_updated: 'today',
      },
    ])
    expect(result.load_diagnostics).toEqual([
      { type: 'network-error', plugin_name: 'safe-name' },
    ])
    expect(JSON.stringify(result)).not.toContain('credential=')
  })

  test('returns marketplace metadata without returning source URLs or paths', async () => {
    const secretSource = 'https://token@example.test/private.git'
    const { dependencies } = createDependencies({
      async readMarketplaceInventory() {
        return {
          marketplaces: [
            {
              name: 'community',
              config: {
                source: { source: 'git', url: secretSource },
                lastUpdated: '2026-08-01',
                autoUpdate: true,
              } as never,
              data: {
                name: 'community',
                plugins: [{ name: 'one' }, { name: 'two' }],
              },
            },
          ],
          failedMarketplaceNames: new Set(['community']),
          loadedPlugins: [
            {
              name: 'one',
              source: 'plugin@community',
              manifest: {},
            },
          ],
          emptyReason: 'all-marketplaces-failed',
          autoUpdateFor: (_name, entry) => entry.autoUpdate === true,
        }
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'plugin_marketplace_inventory_read' },
      dependencies,
    )
    expect(result).toEqual({
      kind: 'plugin_marketplace_inventory_snapshot',
      marketplaces: [
        {
          name: 'community',
          source_kind: 'git',
          last_updated: '2026-08-01',
          plugin_count: 2,
          installed_plugin_count: 1,
          auto_update: true,
          load_failed: true,
        },
      ],
      empty_reason: 'all-marketplaces-failed',
      truncated: false,
    })
    expect(JSON.stringify(result)).not.toContain(secretSource)
  })

  test('filters policy-blocked catalog entries and sorts by install authority count', async () => {
    const { dependencies } = createDependencies({
      async readMarketplaceCatalog(marketplaceName) {
        return {
          marketplaceName,
          marketplace: {
            name: marketplaceName,
            plugins: [
              { name: 'alpha', description: 'alpha description' },
              { name: 'blocked', description: 'must not appear' },
              { name: 'popular', displayName: 'Popular' },
            ],
          },
          installed: {
            'alpha@community': [{ scope: 'local', version: '1.0.0' }],
          },
          globallyInstalledPluginIds: new Set(['alpha@community']),
          enabledPluginIds: new Set(['popular@community']),
          blockedPluginIds: new Set(['blocked@community']),
          installCounts: new Map([
            ['alpha@community', 2],
            ['popular@community', 20],
          ]),
        }
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_marketplace_catalog_read',
        marketplace_name: 'community',
      },
      dependencies,
    )
    expect(result.kind).toBe('plugin_marketplace_catalog_snapshot')
    if (result.kind !== 'plugin_marketplace_catalog_snapshot') return
    expect(result.plugins.map(plugin => plugin.id)).toEqual([
      'popular@community',
      'alpha@community',
    ])
    expect(result.plugins[0]).toMatchObject({
      display_name: 'Popular',
      enabled: true,
      globally_installed: false,
      install_count: 20,
    })
    expect(result.plugins[1]).toMatchObject({
      enabled: false,
      globally_installed: true,
      install_count: 2,
    })
  })

  test('surfaces policy denial as a typed result', async () => {
    const { dependencies } = createDependencies({
      async readMarketplaceCatalog() {
        return 'blocked'
      },
    })
    await expect(
      handleUsagePluginRuntimeAction(
        {
          kind: 'plugin_marketplace_catalog_read',
          marketplace_name: 'community',
        },
        dependencies,
      ),
    ).resolves.toMatchObject({
      kind: 'usage_plugin_error',
      code: 'marketplace_blocked_by_policy',
    })
  })
})

describe('direct TUI plugin mutation adapters', () => {
  test('passes explicit install, uninstall, update, and inferred enable scopes exactly', async () => {
    const calls: unknown[] = []
    const { dependencies } = createDependencies({
      async installPlugin(pluginId, scope) {
        calls.push(['install', pluginId, scope])
        return { success: true, message: 'ok', pluginId, scope }
      },
      async uninstallPlugin(pluginId, scope, deleteData) {
        calls.push(['uninstall', pluginId, scope, deleteData])
        return {
          success: true,
          message: 'ok',
          pluginId,
          scope,
          reverseDependents: ['dependent@community'],
        }
      },
      async setPluginEnabled(pluginId, enabled, scope) {
        calls.push(['set-enabled', pluginId, enabled, scope])
        return {
          success: true,
          message: 'ok',
          pluginId,
          scope: 'project',
        }
      },
      async updatePlugin(pluginId, scope) {
        calls.push(['update', pluginId, scope])
        return {
          success: true,
          message: 'ok',
          pluginId,
          scope,
          alreadyUpToDate: true,
        }
      },
    })

    await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_install',
        plugin_id: 'demo@community',
        scope: 'local',
      },
      dependencies,
    )
    await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_uninstall',
        plugin_id: 'demo@community',
        scope: 'project',
        delete_data: true,
      },
      dependencies,
    )
    const enabled = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_set_enabled',
        plugin_id: 'demo@community',
        enabled: true,
        scope: null,
      },
      dependencies,
    )
    await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_update',
        plugin_id: 'demo@community',
        scope: 'managed',
      },
      dependencies,
    )

    expect(calls).toEqual([
      ['install', 'demo@community', 'local'],
      ['uninstall', 'demo@community', 'project', true],
      ['set-enabled', 'demo@community', true, undefined],
      ['update', 'demo@community', 'managed'],
    ])
    expect(enabled).toMatchObject({
      kind: 'plugin_enabled_state_updated',
      scope: 'project',
    })
  })

  test('does not guess a scope when the enable authority omits it', async () => {
    const { dependencies, reported } = createDependencies({
      async setPluginEnabled(pluginId) {
        return { success: true, message: 'looks successful', pluginId }
      },
    })
    const result = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_set_enabled',
        plugin_id: 'demo@community',
        enabled: true,
        scope: null,
      },
      dependencies,
    )
    expect(result).toMatchObject({
      kind: 'usage_plugin_error',
      action_kind: 'plugin_set_enabled',
      code: 'plugin_operation_rejected',
    })
    expect(reported).toHaveLength(1)
  })

  test('never returns an authority failure message or throws it into the session', async () => {
    const secret = 'api-key-in-authority-message'
    const { dependencies, reported } = createDependencies({
      async installPlugin() {
        return { success: false, message: secret }
      },
    })
    const result = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_install',
        plugin_id: 'demo@community',
        scope: 'user',
      },
      dependencies,
    )
    expect(result).toMatchObject({
      kind: 'usage_plugin_error',
      code: 'plugin_operation_rejected',
    })
    expect(JSON.stringify(result)).not.toContain(secret)
    expect(reported).toHaveLength(1)
  })
})

describe('direct TUI marketplace and validation mutation adapters', () => {
  test('commits marketplace add in the historical authority order without echoing source credentials', async () => {
    const calls: string[] = []
    const secretSource = 'https://user:secret@example.test/catalog.git'
    const source = { source: 'git', url: secretSource } as const
    const { dependencies } = createDependencies({
      async parseMarketplaceInput(input) {
        calls.push(`parse:${input}`)
        return source
      },
      async addMarketplace(value) {
        calls.push(`add:${value.source}`)
        return {
          name: 'private-market',
          alreadyMaterialized: false,
          resolvedSource: value,
        }
      },
      saveMarketplaceDeclaration(name, value) {
        calls.push(`save:${name}:${value.source}`)
      },
      clearPluginCaches() {
        calls.push('clear')
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'plugin_marketplace_add', source_input: secretSource },
      dependencies,
    )
    expect(calls).toEqual([
      `parse:${secretSource}`,
      'add:git',
      'save:private-market:git',
      'clear',
    ])
    expect(result).toEqual({
      kind: 'plugin_marketplace_added',
      marketplace_name: 'private-market',
      source_kind: 'git',
      already_materialized: false,
    })
    expect(JSON.stringify(result)).not.toContain(secretSource)
  })

  test('stops invalid marketplace input before any mutation', async () => {
    let mutations = 0
    const { dependencies } = createDependencies({
      async parseMarketplaceInput() {
        return { error: 'contains-private-parser-detail' }
      },
      async addMarketplace(source) {
        mutations += 1
        return {
          name: 'unreachable',
          alreadyMaterialized: false,
          resolvedSource: source,
        }
      },
    })
    const result = await handleUsagePluginRuntimeAction(
      { kind: 'plugin_marketplace_add', source_input: 'invalid-source' },
      dependencies,
    )
    expect(result).toMatchObject({
      kind: 'usage_plugin_error',
      code: 'invalid_marketplace_source',
    })
    expect(JSON.stringify(result)).not.toContain('private-parser-detail')
    expect(mutations).toBe(0)
  })

  test('delegates remove, update, and auto-update with request-scoped completion', async () => {
    const calls: unknown[] = []
    const { dependencies } = createDependencies({
      async removeMarketplace(name) {
        calls.push(['remove', name])
      },
      async refreshMarketplace(name) {
        calls.push(['refresh', name])
      },
      async updatePluginsForMarketplaces(names) {
        calls.push(['update-plugins', [...names]])
        return {
          updatedPluginIds: ['demo@community'],
          failures: [{ hidden: 'raw failure detail' }],
        }
      },
      async setMarketplaceAutoUpdate(name, enabled) {
        calls.push(['auto-update', name, enabled])
      },
      clearPluginCaches() {
        calls.push(['clear'])
      },
    })

    const removed = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_marketplace_remove',
        marketplace_name: 'community',
      },
      dependencies,
    )
    const updated = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_marketplace_update',
        marketplace_name: 'Community',
      },
      dependencies,
    )
    const autoUpdate = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_marketplace_set_auto_update',
        marketplace_name: 'community',
        enabled: true,
      },
      dependencies,
    )

    expect(calls).toEqual([
      ['remove', 'community'],
      ['clear'],
      ['refresh', 'Community'],
      ['update-plugins', ['community']],
      ['clear'],
      ['auto-update', 'community', true],
    ])
    expect(removed.kind).toBe('plugin_marketplace_removed')
    expect(updated).toEqual({
      kind: 'plugin_marketplace_updated',
      marketplace_name: 'Community',
      updated_plugin_ids: ['demo@community'],
      plugin_update_failure_count: 1,
    })
    expect(JSON.stringify(updated)).not.toContain('raw failure detail')
    expect(autoUpdate.kind).toBe(
      'plugin_marketplace_auto_update_updated',
    )
  })

  test('returns bounded validation facts without file paths, messages, or contents', async () => {
    const secret = '/private/path/token-secret/plugin.json'
    const { dependencies } = createDependencies({
      async validatePlugin() {
        return [
          {
            success: false,
            errors: [
              {
                path: 'manifest.name',
                message: `raw message ${secret}`,
                code: 'invalid-name',
              },
            ],
            warnings: [],
            filePath: secret,
            fileType: 'plugin',
          },
          {
            success: true,
            errors: [],
            warnings: [
              {
                path: 'commands[0]',
                message: `file body ${secret}`,
              },
            ],
            filePath: `${secret}/SKILL.md`,
            fileType: 'skill',
          },
        ]
      },
    })

    const result = await handleUsagePluginRuntimeAction(
      { kind: 'plugin_validate', path: secret },
      dependencies,
    )
    expect(result).toEqual({
      kind: 'plugin_validation_result',
      success: false,
      file_type: 'plugin',
      errors: [{ path: 'manifest.name', code: 'invalid-name' }],
      warnings: [{ path: 'commands[0]', code: null }],
      related_result_count: 1,
      truncated: false,
    })
    expect(JSON.stringify(result)).not.toContain(secret)
    expect(JSON.stringify(result)).not.toContain('raw message')
    expect(JSON.stringify(result)).not.toContain('file body')
  })

  test('normalizes thrown authority failures into typed non-fatal results', async () => {
    const secret = 'private-stack-or-credential'
    const { dependencies, reported } = createDependencies({
      async removeMarketplace() {
        throw new Error(secret)
      },
    })
    const result = await handleUsagePluginRuntimeAction(
      {
        kind: 'plugin_marketplace_remove',
        marketplace_name: 'community',
      },
      dependencies,
    )
    expect(result).toEqual({
      kind: 'usage_plugin_error',
      action_kind: 'plugin_marketplace_remove',
      code: 'marketplace_operation_rejected',
      message: 'The marketplace operation was rejected.',
    })
    expect(JSON.stringify(result)).not.toContain(secret)
    expect(reported).toHaveLength(1)
  })
})

describe('direct TUI usage/plugin authority and process boundary', () => {
  test('imports only the existing direct business authorities', () => {
    const source = readFileSync(
      join(
        import.meta.dir,
        '..',
        '..',
        'src/cli/directTuiUsagePluginRuntimeActions.ts',
      ),
      'utf8',
    )
    for (const authority of [
      '../services/api/usage.js',
      '../services/acosmi/index.js',
      '../services/plugins/pluginOperations.js',
      '../utils/plugins/marketplaceManager.js',
      '../utils/plugins/marketplaceHelpers.js',
      '../utils/plugins/pluginLoader.js',
      '../utils/plugins/installedPluginsManager.js',
      '../utils/plugins/pluginPolicy.js',
      '../utils/plugins/parseMarketplaceInput.js',
      '../utils/plugins/pluginAutoupdate.js',
      '../utils/plugins/validatePlugin.js',
    ]) {
      expect(source).toContain(authority)
    }
    expect(source).not.toMatch(/app[-_]?server/i)
    expect(source).not.toContain('apps/agent-worker')
    expect(source).not.toContain('process.exit')
    expect(source).not.toContain('structuredIO')
  })
})
