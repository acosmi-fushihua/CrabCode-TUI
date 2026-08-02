import { basename, dirname } from 'node:path'
import stripAnsi from 'strip-ansi'
import { z } from 'zod/v4'
import type { MarketplaceSource } from '../utils/plugins/schemas.js'

/**
 * Process-private actions used by the native renderer for the historical
 * `/usage` and `/plugin` panels. This module is deliberately a closed value
 * adapter over the existing direct authorities: it does not own model,
 * entitlement, plugin, marketplace, or validation semantics.
 */

const NO_CONTROL_CHARACTERS = /^[^\u0000-\u001f\u007f]*$/
const PLUGIN_SELECTOR =
  /^[a-z0-9][-a-z0-9._]*(?:@[a-z0-9][-a-z0-9._]*)?$/i
const MARKETPLACE_NAME = /^[a-z0-9][-a-z0-9._]*$/i

const SafeInputLineSchema = z
  .string()
  .trim()
  .min(1)
  .max(4096)
  .regex(NO_CONTROL_CHARACTERS)
const PluginSelectorSchema = z
  .string()
  .trim()
  .min(1)
  .max(512)
  .regex(PLUGIN_SELECTOR)
const MarketplaceNameSchema = z
  .string()
  .trim()
  .min(1)
  .max(256)
  .regex(MARKETPLACE_NAME)
const SafeWireTextSchema = z
  .string()
  .max(1024)
  .regex(NO_CONTROL_CHARACTERS)
const PluginInstallScopeSchema = z.enum(['user', 'project', 'local'])
const PluginScopeSchema = z.enum(['user', 'project', 'local', 'managed'])
const PluginConfiguredScopeSchema = z.enum([
  'user',
  'project',
  'local',
  'managed',
  'flag',
  'builtin',
])
const PluginDiscoverEmptyReasonSchema = z.enum([
  'git-not-installed',
  'all-blocked-by-policy',
  'policy-restricts-sources',
  'all-marketplaces-failed',
  'no-marketplaces-configured',
  'all-plugins-installed',
])

export const USAGE_PLUGIN_RUNTIME_ACTION_KINDS = [
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
] as const

export const UsagePluginRuntimeActionSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('usage_read') }).strict(),
  z
    .object({
      kind: z.literal('usage_set_five_hour_continue'),
      enabled: z.boolean(),
    })
    .strict(),
  z.object({ kind: z.literal('plugin_inventory_read') }).strict(),
  z.object({ kind: z.literal('plugin_marketplace_inventory_read') }).strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_catalog_read'),
      marketplace_name: MarketplaceNameSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_install'),
      plugin_id: PluginSelectorSchema,
      scope: PluginInstallScopeSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_uninstall'),
      plugin_id: PluginSelectorSchema,
      scope: PluginInstallScopeSchema,
      delete_data: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_set_enabled'),
      plugin_id: PluginSelectorSchema,
      enabled: z.boolean(),
      scope: PluginInstallScopeSchema.nullable(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_update'),
      plugin_id: PluginSelectorSchema,
      scope: PluginScopeSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_add'),
      source_input: SafeInputLineSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_remove'),
      marketplace_name: MarketplaceNameSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_update'),
      marketplace_name: MarketplaceNameSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_set_auto_update'),
      marketplace_name: MarketplaceNameSchema,
      enabled: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_validate'),
      path: SafeInputLineSchema,
    })
    .strict(),
])

export type UsagePluginRuntimeAction = z.infer<
  typeof UsagePluginRuntimeActionSchema
>

export function parseUsagePluginRuntimeAction(
  value: unknown,
): UsagePluginRuntimeAction | null {
  const parsed = UsagePluginRuntimeActionSchema.safeParse(value)
  return parsed.success ? parsed.data : null
}

const RateLimitSnapshotSchema = z
  .object({
    utilization: z.number().finite().min(0).max(100).nullable(),
    resets_at: SafeWireTextSchema.nullable(),
    overridable: z.boolean().nullable(),
  })
  .strict()

const ExtraUsageSnapshotSchema = z
  .object({
    is_enabled: z.boolean(),
    monthly_limit: z.number().finite().nonnegative().nullable(),
    used_credits: z.number().finite().nonnegative().nullable(),
    utilization: z.number().finite().min(0).max(100).nullable(),
  })
  .strict()

const UtilizationSnapshotSchema = z
  .object({
    five_hour: RateLimitSnapshotSchema.nullable(),
    seven_day: RateLimitSnapshotSchema.nullable(),
    extra_usage: ExtraUsageSnapshotSchema.nullable(),
    five_hour_continue_enabled: z.boolean().nullable(),
  })
  .strict()

const EntitlementBalanceSnapshotSchema = z
  .object({
    total_token_quota: z.number().finite().nonnegative(),
    total_token_used: z.number().finite().nonnegative(),
    total_token_remaining: z.number().finite().nonnegative(),
    // The entitlement authority uses a negative finite value when call
    // accounting is not applicable. Preserve that backend sentinel: the
    // renderer already omits the call row unless the quota is positive.
    total_call_quota: z.number().finite(),
    total_call_used: z.number().finite().nonnegative(),
    total_call_remaining: z.number().finite(),
    active_entitlements: z.number().int().nonnegative(),
  })
  .strict()

const PluginInstallationSnapshotSchema = z
  .object({
    scope: PluginScopeSchema,
    version: SafeWireTextSchema.nullable(),
    installed_at: SafeWireTextSchema.nullable(),
    last_updated: SafeWireTextSchema.nullable(),
  })
  .strict()

const PluginInventoryEntrySchema = z
  .object({
    id: PluginSelectorSchema,
    name: SafeWireTextSchema,
    marketplace: SafeWireTextSchema,
    description: SafeWireTextSchema.nullable(),
    version: SafeWireTextSchema.nullable(),
    is_builtin: z.boolean(),
    loaded: z.boolean(),
    enabled: z.boolean(),
    configured_scope: PluginConfiguredScopeSchema.nullable(),
    installations: z.array(PluginInstallationSnapshotSchema).max(16),
  })
  .strict()

export const PLUGIN_LOAD_ERROR_TYPES = [
  'path-not-found',
  'git-auth-failed',
  'git-timeout',
  'network-error',
  'manifest-parse-error',
  'manifest-validation-error',
  'plugin-not-found',
  'marketplace-not-found',
  'marketplace-load-failed',
  'mcp-config-invalid',
  'mcp-server-suppressed-duplicate',
  'lsp-config-invalid',
  'hook-load-failed',
  'component-load-failed',
  'mcpb-download-failed',
  'mcpb-extract-failed',
  'mcpb-invalid-manifest',
  'lsp-server-start-failed',
  'lsp-server-crashed',
  'lsp-request-timeout',
  'lsp-request-failed',
  'marketplace-blocked-by-policy',
  'dependency-unsatisfied',
  'plugin-cache-miss',
  'generic-error',
] as const

const PluginLoadDiagnosticSchema = z
  .object({
    type: z.enum(PLUGIN_LOAD_ERROR_TYPES),
    plugin_name: SafeWireTextSchema.nullable(),
  })
  .strict()

export const MARKETPLACE_SOURCE_KINDS = [
  'url',
  'github',
  'git',
  'npm',
  'file',
  'directory',
  'hostPattern',
  'pathPattern',
  'settings',
] as const

const MarketplaceInventoryEntrySchema = z
  .object({
    name: SafeWireTextSchema,
    source_kind: z.enum(MARKETPLACE_SOURCE_KINDS),
    last_updated: SafeWireTextSchema.nullable(),
    plugin_count: z.number().int().nonnegative().nullable(),
    installed_plugin_count: z.number().int().nonnegative(),
    auto_update: z.boolean(),
    load_failed: z.boolean(),
  })
  .strict()

const MarketplaceCatalogPluginSchema = z
  .object({
    id: PluginSelectorSchema,
    name: SafeWireTextSchema,
    display_name: SafeWireTextSchema,
    description: SafeWireTextSchema.nullable(),
    version: SafeWireTextSchema.nullable(),
    category: SafeWireTextSchema.nullable(),
    tags: z.array(SafeWireTextSchema).max(32),
    globally_installed: z.boolean(),
    enabled: z.boolean(),
    install_count: z.number().int().nonnegative().nullable(),
    installations: z.array(PluginInstallationSnapshotSchema).max(16),
  })
  .strict()

const PluginValidationDiagnosticSchema = z
  .object({
    path: SafeWireTextSchema,
    code: SafeWireTextSchema.nullable(),
  })
  .strict()

export const USAGE_PLUGIN_RUNTIME_ERROR_CODES = [
  'usage_unavailable',
  'usage_write_rejected',
  'plugin_inventory_unavailable',
  'marketplace_inventory_unavailable',
  'marketplace_catalog_unavailable',
  'marketplace_blocked_by_policy',
  'invalid_marketplace_source',
  'plugin_operation_rejected',
  'marketplace_operation_rejected',
  'validation_unavailable',
  'authority_failure',
] as const

const UsagePluginRuntimeErrorSchema = z
  .object({
    kind: z.literal('usage_plugin_error'),
    action_kind: z.enum(USAGE_PLUGIN_RUNTIME_ACTION_KINDS),
    code: z.enum(USAGE_PLUGIN_RUNTIME_ERROR_CODES),
    message: SafeWireTextSchema,
  })
  .strict()

export const USAGE_PLUGIN_RUNTIME_RESULT_KINDS = [
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
] as const

export const UsagePluginRuntimeResultSchema = z.discriminatedUnion('kind', [
  z
    .object({
      kind: z.literal('usage_snapshot'),
      utilization: UtilizationSnapshotSchema.nullable(),
      entitlement_balance: EntitlementBalanceSnapshotSchema.nullable(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('usage_five_hour_continue_updated'),
      enabled: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_inventory_snapshot'),
      plugins: z.array(PluginInventoryEntrySchema).max(512),
      load_diagnostics: z.array(PluginLoadDiagnosticSchema).max(256),
      truncated: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_inventory_snapshot'),
      marketplaces: z.array(MarketplaceInventoryEntrySchema).max(128),
      empty_reason: PluginDiscoverEmptyReasonSchema,
      truncated: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_catalog_snapshot'),
      marketplace_name: SafeWireTextSchema,
      plugins: z.array(MarketplaceCatalogPluginSchema).max(512),
      truncated: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_installed'),
      plugin_id: PluginSelectorSchema,
      plugin_name: SafeWireTextSchema,
      scope: PluginInstallScopeSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_uninstalled'),
      plugin_id: PluginSelectorSchema,
      plugin_name: SafeWireTextSchema,
      scope: PluginInstallScopeSchema,
      reverse_dependents: z.array(PluginSelectorSchema).max(128),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_enabled_state_updated'),
      plugin_id: PluginSelectorSchema,
      plugin_name: SafeWireTextSchema,
      enabled: z.boolean(),
      scope: PluginInstallScopeSchema,
      reverse_dependents: z.array(PluginSelectorSchema).max(128),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_updated'),
      plugin_id: PluginSelectorSchema,
      scope: PluginScopeSchema,
      old_version: SafeWireTextSchema.nullable(),
      new_version: SafeWireTextSchema.nullable(),
      already_up_to_date: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_added'),
      marketplace_name: SafeWireTextSchema,
      source_kind: z.enum(MARKETPLACE_SOURCE_KINDS),
      already_materialized: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_removed'),
      marketplace_name: SafeWireTextSchema,
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_updated'),
      marketplace_name: SafeWireTextSchema,
      updated_plugin_ids: z.array(PluginSelectorSchema).max(512),
      plugin_update_failure_count: z.number().int().nonnegative(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_marketplace_auto_update_updated'),
      marketplace_name: SafeWireTextSchema,
      enabled: z.boolean(),
    })
    .strict(),
  z
    .object({
      kind: z.literal('plugin_validation_result'),
      success: z.boolean(),
      file_type: z.enum([
        'plugin',
        'marketplace',
        'skill',
        'agent',
        'command',
        'hooks',
      ]),
      errors: z.array(PluginValidationDiagnosticSchema).max(256),
      warnings: z.array(PluginValidationDiagnosticSchema).max(256),
      related_result_count: z.number().int().nonnegative(),
      truncated: z.boolean(),
    })
    .strict(),
  UsagePluginRuntimeErrorSchema,
])

export type UsagePluginRuntimeResult = z.infer<
  typeof UsagePluginRuntimeResultSchema
>

type AuthorityRateLimit = {
  utilization: number | null
  resets_at: string | null
  overridable?: boolean
}

type AuthorityUtilization = {
  five_hour?: AuthorityRateLimit | null
  seven_day?: AuthorityRateLimit | null
  extra_usage?: {
    is_enabled: boolean
    monthly_limit: number | null
    used_credits: number | null
    utilization: number | null
  } | null
  five_hour_continue_enabled?: boolean
}

type AuthorityEntitlementBalance = {
  totalTokenQuota: number
  totalTokenUsed: number
  totalTokenRemaining: number
  totalCallQuota: number
  totalCallUsed: number
  totalCallRemaining: number
  activeEntitlements: number
}

type AuthorityPluginInstallation = {
  scope: 'user' | 'project' | 'local' | 'managed'
  version?: string
  installedAt?: string
  lastUpdated?: string
}

type AuthorityLoadedPlugin = {
  name: string
  source: string
  isBuiltin?: boolean
  manifest: {
    description?: string
    version?: string
  }
}

type AuthorityPluginLoadError = {
  type: (typeof PLUGIN_LOAD_ERROR_TYPES)[number]
  plugin?: string
}

export type PluginInventoryAuthoritySnapshot = {
  installed: Record<string, AuthorityPluginInstallation[]>
  loadedEnabled: AuthorityLoadedPlugin[]
  loadedDisabled: AuthorityLoadedPlugin[]
  loadErrors: AuthorityPluginLoadError[]
  configuredScopes: ReadonlyMap<
    string,
    'user' | 'project' | 'local' | 'managed' | 'flag'
  >
  blockedPluginIds: ReadonlySet<string>
}

type AuthorityMarketplaceSource = {
  source: (typeof MARKETPLACE_SOURCE_KINDS)[number]
}

type AuthorityMarketplaceEntry = {
  source: AuthorityMarketplaceSource
  lastUpdated: string
  autoUpdate?: boolean
}

type AuthorityMarketplaceCatalogEntry = {
  name: string
  displayName?: string
  shortDescription?: string
  description?: string
  version?: string
  category?: string
  tags?: string[]
}

type AuthorityMarketplace = {
  name: string
  plugins: AuthorityMarketplaceCatalogEntry[]
}

export type MarketplaceInventoryAuthoritySnapshot = {
  marketplaces: Array<{
    name: string
    config: AuthorityMarketplaceEntry
    data: AuthorityMarketplace | null
  }>
  failedMarketplaceNames: ReadonlySet<string>
  loadedPlugins: AuthorityLoadedPlugin[]
  emptyReason: z.infer<typeof PluginDiscoverEmptyReasonSchema>
  autoUpdateFor: (
    marketplaceName: string,
    entry: AuthorityMarketplaceEntry,
  ) => boolean
}

export type MarketplaceCatalogAuthoritySnapshot = {
  marketplaceName: string
  marketplace: AuthorityMarketplace
  installed: Record<string, AuthorityPluginInstallation[]>
  globallyInstalledPluginIds: ReadonlySet<string>
  enabledPluginIds: ReadonlySet<string>
  blockedPluginIds: ReadonlySet<string>
  installCounts: ReadonlyMap<string, number> | null
}

type AuthorityPluginOperationResult = {
  success: boolean
  message: string
  pluginId?: string
  pluginName?: string
  scope?: 'user' | 'project' | 'local' | 'managed'
  reverseDependents?: string[]
}

type AuthorityPluginUpdateResult = {
  success: boolean
  message: string
  pluginId?: string
  oldVersion?: string
  newVersion?: string
  alreadyUpToDate?: boolean
  scope?: 'user' | 'project' | 'local' | 'managed'
}

type AuthorityMarketplaceSourceValue = MarketplaceSource

type AuthorityMarketplaceParseResult =
  | AuthorityMarketplaceSourceValue
  | { error: string }
  | null

type AuthorityMarketplaceAddResult = {
  name: string
  alreadyMaterialized: boolean
  resolvedSource: AuthorityMarketplaceSourceValue
}

type AuthorityMarketplacePluginUpdateReport = {
  updatedPluginIds: string[]
  failures: unknown[]
}

type AuthorityValidationDiagnostic = {
  path: string
  message: string
  code?: string
}

type AuthorityValidationResult = {
  success: boolean
  errors: AuthorityValidationDiagnostic[]
  warnings: AuthorityValidationDiagnostic[]
  filePath: string
  fileType: 'plugin' | 'marketplace' | 'skill' | 'agent' | 'command' | 'hooks'
}

export interface UsagePluginRuntimeDependencies {
  fetchUtilization(): Promise<AuthorityUtilization | null>
  fetchEntitlementBalance(): Promise<AuthorityEntitlementBalance | null>
  setFiveHourContinuePreference(enabled: boolean): Promise<boolean>
  readPluginInventory(): Promise<PluginInventoryAuthoritySnapshot>
  readMarketplaceInventory(): Promise<MarketplaceInventoryAuthoritySnapshot>
  readMarketplaceCatalog(
    marketplaceName: string,
  ): Promise<MarketplaceCatalogAuthoritySnapshot | 'blocked'>
  installPlugin(
    pluginId: string,
    scope: 'user' | 'project' | 'local',
  ): Promise<AuthorityPluginOperationResult>
  uninstallPlugin(
    pluginId: string,
    scope: 'user' | 'project' | 'local',
    deleteData: boolean,
  ): Promise<AuthorityPluginOperationResult>
  setPluginEnabled(
    pluginId: string,
    enabled: boolean,
    scope?: 'user' | 'project' | 'local',
  ): Promise<AuthorityPluginOperationResult>
  updatePlugin(
    pluginId: string,
    scope: 'user' | 'project' | 'local' | 'managed',
  ): Promise<AuthorityPluginUpdateResult>
  parseMarketplaceInput(
    sourceInput: string,
  ): Promise<AuthorityMarketplaceParseResult>
  addMarketplace(
    source: AuthorityMarketplaceSourceValue,
  ): Promise<AuthorityMarketplaceAddResult>
  saveMarketplaceDeclaration(
    marketplaceName: string,
    source: AuthorityMarketplaceSourceValue,
  ): void | Promise<void>
  removeMarketplace(marketplaceName: string): Promise<void>
  refreshMarketplace(marketplaceName: string): Promise<void>
  updatePluginsForMarketplaces(
    marketplaceNames: ReadonlySet<string>,
  ): Promise<AuthorityMarketplacePluginUpdateReport>
  setMarketplaceAutoUpdate(
    marketplaceName: string,
    enabled: boolean,
  ): Promise<void>
  clearPluginCaches(): void | Promise<void>
  validatePlugin(path: string): Promise<AuthorityValidationResult[]>
  reportError(error: unknown): void | Promise<void>
}

async function readDefaultPluginInventory(): Promise<PluginInventoryAuthoritySnapshot> {
  const [loader, installedManager, startupCheck, policy] = await Promise.all([
    import('../utils/plugins/pluginLoader.js'),
    import('../utils/plugins/installedPluginsManager.js'),
    import('../utils/plugins/pluginStartupCheck.js'),
    import('../utils/plugins/pluginPolicy.js'),
  ])
  const installed = installedManager.loadInstalledPluginsV2()
  const registryHealth = installedManager.getInstalledPluginsRegistryHealth()
  if (!registryHealth.ok) {
    throw new Error('installed plugin registry is unavailable')
  }
  const loaded = await loader.loadAllPlugins()
  const configuredScopes = startupCheck.getPluginEditableScopes()
  const allPluginIds = new Set<string>(Object.keys(installed.plugins))
  for (const plugin of [...loaded.enabled, ...loaded.disabled]) {
    allPluginIds.add(canonicalLoadedPluginId(plugin))
  }
  const blockedPluginIds = new Set(
    [...allPluginIds].filter(pluginId => policy.isPluginBlockedByPolicy(pluginId)),
  )
  return {
    installed: installed.plugins,
    loadedEnabled: loaded.enabled,
    loadedDisabled: loaded.disabled,
    loadErrors: loaded.errors,
    configuredScopes,
    blockedPluginIds,
  }
}

async function readDefaultMarketplaceInventory(): Promise<MarketplaceInventoryAuthoritySnapshot> {
  const [manager, helpers, loader, schemas] = await Promise.all([
    import('../utils/plugins/marketplaceManager.js'),
    import('../utils/plugins/marketplaceHelpers.js'),
    import('../utils/plugins/pluginLoader.js'),
    import('../utils/plugins/schemas.js'),
  ])
  const config = await manager.loadKnownMarketplacesConfig()
  const [loaded, marketplaceLoad] = await Promise.all([
    loader.loadAllPlugins(),
    helpers.loadMarketplacesWithGracefulDegradation(config),
  ])
  const emptyReason = await helpers.detectEmptyMarketplaceReason({
    configuredMarketplaceCount: Object.keys(config).length,
    failedMarketplaceCount: marketplaceLoad.failures.length,
  })
  return {
    marketplaces: marketplaceLoad.marketplaces,
    failedMarketplaceNames: new Set(
      marketplaceLoad.failures.map(failure => failure.name),
    ),
    loadedPlugins: [...loaded.enabled, ...loaded.disabled],
    emptyReason,
    autoUpdateFor: schemas.isMarketplaceAutoUpdate,
  }
}

async function readDefaultMarketplaceCatalog(
  marketplaceName: string,
): Promise<MarketplaceCatalogAuthoritySnapshot | 'blocked'> {
  const [manager, helpers, installedManager, startupCheck, policy, counts] =
    await Promise.all([
      import('../utils/plugins/marketplaceManager.js'),
      import('../utils/plugins/marketplaceHelpers.js'),
      import('../utils/plugins/installedPluginsManager.js'),
      import('../utils/plugins/pluginStartupCheck.js'),
      import('../utils/plugins/pluginPolicy.js'),
      import('../utils/plugins/installCounts.js'),
    ])
  const config = await manager.loadKnownMarketplacesConfig()
  const entry = config[marketplaceName]
  if (!entry) throw new Error('marketplace is not configured')
  if (!helpers.isSourceAllowedByPolicy(entry.source)) return 'blocked'

  const installed = installedManager.loadInstalledPluginsV2()
  const registryHealth = installedManager.getInstalledPluginsRegistryHealth()
  if (!registryHealth.ok) {
    throw new Error('installed plugin registry is unavailable')
  }
  const [marketplace, installCounts, enabledPluginIds] = await Promise.all([
    manager.getMarketplace(marketplaceName),
    counts.getInstallCounts(),
    startupCheck.checkEnabledPlugins().then(pluginIds => new Set(pluginIds)),
  ])
  const globallyInstalledPluginIds = new Set<string>()
  const blockedPluginIds = new Set<string>()
  for (const plugin of marketplace.plugins) {
    const pluginId = `${plugin.name}@${marketplaceName}`
    if (installedManager.isPluginGloballyInstalled(pluginId)) {
      globallyInstalledPluginIds.add(pluginId)
    }
    if (policy.isPluginBlockedByPolicy(pluginId)) {
      blockedPluginIds.add(pluginId)
    }
  }
  return {
    marketplaceName,
    marketplace,
    installed: installed.plugins,
    globallyInstalledPluginIds,
    enabledPluginIds,
    blockedPluginIds,
    installCounts,
  }
}

async function validateDefaultPlugin(
  path: string,
): Promise<AuthorityValidationResult[]> {
  const validation = await import('../utils/plugins/validatePlugin.js')
  const manifest = await validation.validateManifest(path)
  const results: AuthorityValidationResult[] = [manifest]
  if (
    manifest.fileType === 'plugin' &&
    basename(dirname(manifest.filePath)) === '.crabcode-plugin'
  ) {
    results.push(...(await validation.validatePluginContents(dirname(dirname(manifest.filePath)))))
  }
  return results
}

export function createDefaultUsagePluginRuntimeDependencies(): UsagePluginRuntimeDependencies {
  return {
    async fetchUtilization() {
      const usage = await import('../services/api/usage.js')
      return usage.fetchUtilization()
    },
    async fetchEntitlementBalance() {
      const { getAcosmiClient } = await import('../services/acosmi/index.js')
      return (await getAcosmiClient()).getBalance()
    },
    async setFiveHourContinuePreference(enabled) {
      const usage = await import('../services/api/usage.js')
      return usage.setFiveHourContinuePreference(enabled)
    },
    readPluginInventory: readDefaultPluginInventory,
    readMarketplaceInventory: readDefaultMarketplaceInventory,
    readMarketplaceCatalog: readDefaultMarketplaceCatalog,
    async installPlugin(pluginId, scope) {
      const operations = await import('../services/plugins/pluginOperations.js')
      return operations.installPluginOp(pluginId, scope)
    },
    async uninstallPlugin(pluginId, scope, deleteData) {
      const operations = await import('../services/plugins/pluginOperations.js')
      return operations.uninstallPluginOp(pluginId, scope, deleteData)
    },
    async setPluginEnabled(pluginId, enabled, scope) {
      const operations = await import('../services/plugins/pluginOperations.js')
      return enabled
        ? operations.enablePluginOp(pluginId, scope)
        : operations.disablePluginOp(pluginId, scope)
    },
    async updatePlugin(pluginId, scope) {
      const operations = await import('../services/plugins/pluginOperations.js')
      return operations.updatePluginOp(pluginId, scope)
    },
    async parseMarketplaceInput(sourceInput) {
      const parser = await import('../utils/plugins/parseMarketplaceInput.js')
      return parser.parseMarketplaceInput(sourceInput)
    },
    async addMarketplace(source) {
      const manager = await import('../utils/plugins/marketplaceManager.js')
      return manager.addMarketplaceSource(source)
    },
    async saveMarketplaceDeclaration(marketplaceName, source) {
      const manager = await import('../utils/plugins/marketplaceManager.js')
      manager.saveMarketplaceToSettings(marketplaceName, {
        source,
      })
    },
    async removeMarketplace(marketplaceName) {
      const manager = await import('../utils/plugins/marketplaceManager.js')
      await manager.removeMarketplaceSource(marketplaceName)
    },
    async refreshMarketplace(marketplaceName) {
      const manager = await import('../utils/plugins/marketplaceManager.js')
      await manager.refreshMarketplace(marketplaceName)
    },
    async updatePluginsForMarketplaces(marketplaceNames) {
      const updater = await import('../utils/plugins/pluginAutoupdate.js')
      return updater.updatePluginsForMarketplacesStrict(
        new Set(marketplaceNames),
      )
    },
    async setMarketplaceAutoUpdate(marketplaceName, enabled) {
      const manager = await import('../utils/plugins/marketplaceManager.js')
      await manager.setMarketplaceAutoUpdate(marketplaceName, enabled)
    },
    async clearPluginCaches() {
      const cache = await import('../utils/plugins/cacheUtils.js')
      cache.clearAllCaches()
    },
    validatePlugin: validateDefaultPlugin,
    async reportError(error) {
      const { logError } = await import('../utils/log.js')
      logError(error)
    },
  }
}

const MAX_PLUGIN_ROWS = 512
const MAX_PLUGIN_DIAGNOSTICS = 256
const MAX_MARKETPLACE_ROWS = 128
const MAX_INSTALLATIONS_PER_PLUGIN = 16

function safeWireText(value: string, maxLength = 1024): string {
  return stripAnsi(value)
    .replace(/[\u0000-\u001f\u007f]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, maxLength)
}

function nullableSafeWireText(
  value: string | null | undefined,
  maxLength = 1024,
): string | null {
  if (value == null) return null
  const safe = safeWireText(value, maxLength)
  return safe.length > 0 ? safe : null
}

function canonicalLoadedPluginId(plugin: AuthorityLoadedPlugin): string {
  const marketplace = plugin.source.split('@')[1] || 'local'
  return `${plugin.name}@${marketplace}`
}

function splitPluginId(pluginId: string): {
  name: string
  marketplace: string
} {
  const [name = '', marketplace = 'local'] = pluginId.split('@')
  return { name, marketplace }
}

function mapInstallation(
  installation: AuthorityPluginInstallation,
): z.infer<typeof PluginInstallationSnapshotSchema> {
  return {
    scope: installation.scope,
    version: nullableSafeWireText(installation.version, 128),
    installed_at: nullableSafeWireText(installation.installedAt, 128),
    last_updated: nullableSafeWireText(installation.lastUpdated, 128),
  }
}

function mapInstallations(
  installations: readonly AuthorityPluginInstallation[] | undefined,
): z.infer<typeof PluginInstallationSnapshotSchema>[] {
  return [...(installations ?? [])]
    .sort((left, right) => left.scope.localeCompare(right.scope))
    .slice(0, MAX_INSTALLATIONS_PER_PLUGIN)
    .map(mapInstallation)
}

function singleInstallationVersion(
  installations: readonly AuthorityPluginInstallation[],
): string | null {
  const versions = new Set(
    installations
      .map(installation => installation.version)
      .filter((version): version is string => version !== undefined),
  )
  return versions.size === 1
    ? nullableSafeWireText([...versions][0], 128)
    : null
}

function mapRateLimit(
  rateLimit: AuthorityRateLimit | null | undefined,
): z.infer<typeof RateLimitSnapshotSchema> | null {
  if (!rateLimit) return null
  return {
    utilization: rateLimit.utilization,
    resets_at: nullableSafeWireText(rateLimit.resets_at, 128),
    overridable:
      typeof rateLimit.overridable === 'boolean'
        ? rateLimit.overridable
        : null,
  }
}

function mapUtilization(
  utilization: AuthorityUtilization | null,
): z.infer<typeof UtilizationSnapshotSchema> | null {
  if (utilization === null) return null
  return {
    five_hour: mapRateLimit(utilization.five_hour),
    seven_day: mapRateLimit(utilization.seven_day),
    extra_usage: utilization.extra_usage
      ? {
          is_enabled: utilization.extra_usage.is_enabled,
          monthly_limit: utilization.extra_usage.monthly_limit,
          used_credits: utilization.extra_usage.used_credits,
          utilization: utilization.extra_usage.utilization,
        }
      : null,
    five_hour_continue_enabled:
      typeof utilization.five_hour_continue_enabled === 'boolean'
        ? utilization.five_hour_continue_enabled
        : null,
  }
}

function mapEntitlementBalance(
  balance: AuthorityEntitlementBalance | null,
): z.infer<typeof EntitlementBalanceSnapshotSchema> | null {
  if (balance === null) return null
  return {
    total_token_quota: balance.totalTokenQuota,
    total_token_used: balance.totalTokenUsed,
    total_token_remaining: balance.totalTokenRemaining,
    total_call_quota: balance.totalCallQuota,
    total_call_used: balance.totalCallUsed,
    total_call_remaining: balance.totalCallRemaining,
    active_entitlements: balance.activeEntitlements,
  }
}

function buildPluginInventoryResult(
  snapshot: PluginInventoryAuthoritySnapshot,
): UsagePluginRuntimeResult {
  type MutablePluginRow = z.infer<typeof PluginInventoryEntrySchema>
  const rows = new Map<string, MutablePluginRow>()
  let truncated = false

  for (const [pluginId, allInstallations] of Object.entries(snapshot.installed)) {
    if (snapshot.blockedPluginIds.has(pluginId)) continue
    const { name, marketplace } = splitPluginId(pluginId)
    if (allInstallations.length > MAX_INSTALLATIONS_PER_PLUGIN) {
      truncated = true
    }
    rows.set(pluginId, {
      id: pluginId,
      name: safeWireText(name, 256),
      marketplace: safeWireText(marketplace, 256),
      description: null,
      version: singleInstallationVersion(allInstallations),
      is_builtin: false,
      loaded: false,
      enabled: false,
      configured_scope: snapshot.configuredScopes.get(pluginId) ?? null,
      installations: mapInstallations(allInstallations),
    })
  }

  const applyLoaded = (plugin: AuthorityLoadedPlugin, enabled: boolean): void => {
    const pluginId = canonicalLoadedPluginId(plugin)
    if (snapshot.blockedPluginIds.has(pluginId)) return
    const { marketplace } = splitPluginId(pluginId)
    const installations = snapshot.installed[pluginId] ?? []
    if (installations.length > MAX_INSTALLATIONS_PER_PLUGIN) truncated = true
    rows.set(pluginId, {
      id: pluginId,
      name: safeWireText(plugin.name, 256),
      marketplace: safeWireText(marketplace, 256),
      description: nullableSafeWireText(plugin.manifest.description),
      version:
        nullableSafeWireText(plugin.manifest.version, 128) ??
        singleInstallationVersion(installations),
      is_builtin: plugin.isBuiltin === true,
      loaded: true,
      enabled,
      configured_scope: plugin.isBuiltin
        ? 'builtin'
        : (snapshot.configuredScopes.get(pluginId) ?? null),
      installations: mapInstallations(installations),
    })
  }
  snapshot.loadedDisabled.forEach(plugin => applyLoaded(plugin, false))
  snapshot.loadedEnabled.forEach(plugin => applyLoaded(plugin, true))

  const plugins = [...rows.values()].sort((left, right) => {
    if (left.marketplace === 'crabcode-plugin-directory') return -1
    if (right.marketplace === 'crabcode-plugin-directory') return 1
    return left.id.localeCompare(right.id)
  })
  const diagnostics = snapshot.loadErrors.map(error => ({
    type: error.type,
    plugin_name: nullableSafeWireText(error.plugin, 256),
  }))
  if (
    plugins.length > MAX_PLUGIN_ROWS ||
    diagnostics.length > MAX_PLUGIN_DIAGNOSTICS
  ) {
    truncated = true
  }
  return UsagePluginRuntimeResultSchema.parse({
    kind: 'plugin_inventory_snapshot',
    plugins: plugins.slice(0, MAX_PLUGIN_ROWS),
    load_diagnostics: diagnostics.slice(0, MAX_PLUGIN_DIAGNOSTICS),
    truncated,
  })
}

function buildMarketplaceInventoryResult(
  snapshot: MarketplaceInventoryAuthoritySnapshot,
): UsagePluginRuntimeResult {
  const marketplaces = snapshot.marketplaces.map(item => ({
    name: safeWireText(item.name, 256),
    source_kind: item.config.source.source,
    last_updated: nullableSafeWireText(item.config.lastUpdated, 128),
    plugin_count: item.data?.plugins.length ?? null,
    installed_plugin_count: snapshot.loadedPlugins.filter(plugin =>
      plugin.source.endsWith(`@${item.name}`),
    ).length,
    auto_update: snapshot.autoUpdateFor(item.name, item.config),
    load_failed: snapshot.failedMarketplaceNames.has(item.name),
  }))
  marketplaces.sort((left, right) => {
    if (left.name === 'crabcode-plugin-directory') return -1
    if (right.name === 'crabcode-plugin-directory') return 1
    return left.name.localeCompare(right.name)
  })
  return UsagePluginRuntimeResultSchema.parse({
    kind: 'plugin_marketplace_inventory_snapshot',
    marketplaces: marketplaces.slice(0, MAX_MARKETPLACE_ROWS),
    empty_reason: snapshot.emptyReason,
    truncated: marketplaces.length > MAX_MARKETPLACE_ROWS,
  })
}

function buildMarketplaceCatalogResult(
  snapshot: MarketplaceCatalogAuthoritySnapshot,
): UsagePluginRuntimeResult {
  let truncated = false
  const plugins = snapshot.marketplace.plugins.flatMap(plugin => {
    const pluginId = `${plugin.name}@${snapshot.marketplaceName}`
    if (snapshot.blockedPluginIds.has(pluginId)) return []
    const installations = snapshot.installed[pluginId] ?? []
    if (installations.length > MAX_INSTALLATIONS_PER_PLUGIN) truncated = true
    return [
      {
        id: pluginId,
        name: safeWireText(plugin.name, 256),
        display_name: safeWireText(plugin.displayName ?? plugin.name, 256),
        description: nullableSafeWireText(
          plugin.shortDescription ?? plugin.description,
        ),
        version: nullableSafeWireText(plugin.version, 128),
        category: nullableSafeWireText(plugin.category, 128),
        tags: (plugin.tags ?? [])
          .slice(0, 32)
          .map(tag => safeWireText(tag, 128)),
        globally_installed:
          snapshot.globallyInstalledPluginIds.has(pluginId),
        enabled: snapshot.enabledPluginIds.has(pluginId),
        install_count: snapshot.installCounts?.get(pluginId) ?? null,
        installations: mapInstallations(installations),
      },
    ]
  })
  plugins.sort((left, right) => {
    if (snapshot.installCounts) {
      const countDifference =
        (right.install_count ?? 0) - (left.install_count ?? 0)
      if (countDifference !== 0) return countDifference
    }
    return left.name.localeCompare(right.name)
  })
  if (plugins.length > MAX_PLUGIN_ROWS) truncated = true
  return UsagePluginRuntimeResultSchema.parse({
    kind: 'plugin_marketplace_catalog_snapshot',
    marketplace_name: safeWireText(snapshot.marketplaceName, 256),
    plugins: plugins.slice(0, MAX_PLUGIN_ROWS),
    truncated,
  })
}

const ERROR_MESSAGES: Record<
  (typeof USAGE_PLUGIN_RUNTIME_ERROR_CODES)[number],
  string
> = {
  usage_unavailable: 'Usage data is unavailable.',
  usage_write_rejected: 'The usage preference could not be updated.',
  plugin_inventory_unavailable: 'Plugin inventory is unavailable.',
  marketplace_inventory_unavailable: 'Marketplace inventory is unavailable.',
  marketplace_catalog_unavailable: 'Marketplace catalog is unavailable.',
  marketplace_blocked_by_policy: 'The marketplace is blocked by policy.',
  invalid_marketplace_source: 'The marketplace source is invalid.',
  plugin_operation_rejected: 'The plugin operation was rejected.',
  marketplace_operation_rejected: 'The marketplace operation was rejected.',
  validation_unavailable: 'Plugin validation could not be completed.',
  authority_failure: 'The requested operation could not be completed.',
}

function runtimeError(
  actionKind: UsagePluginRuntimeAction['kind'],
  code: (typeof USAGE_PLUGIN_RUNTIME_ERROR_CODES)[number],
): UsagePluginRuntimeResult {
  return {
    kind: 'usage_plugin_error',
    action_kind: actionKind,
    code,
    message: ERROR_MESSAGES[code],
  }
}

async function reportErrorSafely(
  dependencies: UsagePluginRuntimeDependencies,
  error: unknown,
): Promise<void> {
  try {
    await dependencies.reportError(error)
  } catch {
    // A diagnostics sink must never turn a request-scoped failure into a
    // process failure.
  }
}

function errorCodeForAction(
  actionKind: UsagePluginRuntimeAction['kind'],
): (typeof USAGE_PLUGIN_RUNTIME_ERROR_CODES)[number] {
  switch (actionKind) {
    case 'usage_read':
      return 'usage_unavailable'
    case 'usage_set_five_hour_continue':
      return 'usage_write_rejected'
    case 'plugin_inventory_read':
      return 'plugin_inventory_unavailable'
    case 'plugin_marketplace_inventory_read':
      return 'marketplace_inventory_unavailable'
    case 'plugin_marketplace_catalog_read':
      return 'marketplace_catalog_unavailable'
    case 'plugin_install':
    case 'plugin_uninstall':
    case 'plugin_set_enabled':
    case 'plugin_update':
      return 'plugin_operation_rejected'
    case 'plugin_marketplace_add':
    case 'plugin_marketplace_remove':
    case 'plugin_marketplace_update':
    case 'plugin_marketplace_set_auto_update':
      return 'marketplace_operation_rejected'
    case 'plugin_validate':
      return 'validation_unavailable'
  }
}

async function rejectedOperation(
  dependencies: UsagePluginRuntimeDependencies,
  actionKind: UsagePluginRuntimeAction['kind'],
  code: (typeof USAGE_PLUGIN_RUNTIME_ERROR_CODES)[number],
): Promise<UsagePluginRuntimeResult> {
  await reportErrorSafely(
    dependencies,
    new Error(`${actionKind} authority rejected the request`),
  )
  return runtimeError(actionKind, code)
}

export async function handleUsagePluginRuntimeAction(
  action: UsagePluginRuntimeAction,
  dependencies: UsagePluginRuntimeDependencies =
    createDefaultUsagePluginRuntimeDependencies(),
): Promise<UsagePluginRuntimeResult> {
  try {
    switch (action.kind) {
      case 'usage_read': {
        const [utilizationResult, balanceResult] = await Promise.allSettled([
          dependencies.fetchUtilization(),
          dependencies.fetchEntitlementBalance(),
        ])
        const utilization =
          utilizationResult.status === 'fulfilled'
            ? utilizationResult.value
            : null
        const balance =
          balanceResult.status === 'fulfilled' ? balanceResult.value : null
        if (utilization === null && balance === null) {
          if (utilizationResult.status === 'rejected') {
            await reportErrorSafely(dependencies, utilizationResult.reason)
          }
          if (balanceResult.status === 'rejected') {
            await reportErrorSafely(dependencies, balanceResult.reason)
          }
          return runtimeError(action.kind, 'usage_unavailable')
        }
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'usage_snapshot',
          utilization: mapUtilization(utilization),
          entitlement_balance: mapEntitlementBalance(balance),
        })
      }

      case 'usage_set_five_hour_continue': {
        const enabled = await dependencies.setFiveHourContinuePreference(
          action.enabled,
        )
        if (typeof enabled !== 'boolean') {
          return rejectedOperation(
            dependencies,
            action.kind,
            'usage_write_rejected',
          )
        }
        return { kind: 'usage_five_hour_continue_updated', enabled }
      }

      case 'plugin_inventory_read':
        return buildPluginInventoryResult(
          await dependencies.readPluginInventory(),
        )

      case 'plugin_marketplace_inventory_read':
        return buildMarketplaceInventoryResult(
          await dependencies.readMarketplaceInventory(),
        )

      case 'plugin_marketplace_catalog_read': {
        const snapshot = await dependencies.readMarketplaceCatalog(
          action.marketplace_name,
        )
        if (snapshot === 'blocked') {
          return runtimeError(action.kind, 'marketplace_blocked_by_policy')
        }
        return buildMarketplaceCatalogResult(snapshot)
      }

      case 'plugin_install': {
        const result = await dependencies.installPlugin(
          action.plugin_id,
          action.scope,
        )
        if (!result.success) {
          return rejectedOperation(
            dependencies,
            action.kind,
            'plugin_operation_rejected',
          )
        }
        const pluginId = result.pluginId ?? action.plugin_id
        const pluginName = result.pluginName ?? splitPluginId(pluginId).name
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_installed',
          plugin_id: pluginId,
          plugin_name: safeWireText(pluginName, 256),
          scope: action.scope,
        })
      }

      case 'plugin_uninstall': {
        const result = await dependencies.uninstallPlugin(
          action.plugin_id,
          action.scope,
          action.delete_data,
        )
        if (!result.success) {
          return rejectedOperation(
            dependencies,
            action.kind,
            'plugin_operation_rejected',
          )
        }
        const pluginId = result.pluginId ?? action.plugin_id
        const pluginName = result.pluginName ?? splitPluginId(pluginId).name
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_uninstalled',
          plugin_id: pluginId,
          plugin_name: safeWireText(pluginName, 256),
          scope: action.scope,
          reverse_dependents: (result.reverseDependents ?? []).slice(0, 128),
        })
      }

      case 'plugin_set_enabled': {
        const result = await dependencies.setPluginEnabled(
          action.plugin_id,
          action.enabled,
          action.scope ?? undefined,
        )
        if (
          !result.success ||
          result.scope === undefined ||
          result.scope === 'managed'
        ) {
          return rejectedOperation(
            dependencies,
            action.kind,
            'plugin_operation_rejected',
          )
        }
        const pluginId = result.pluginId ?? action.plugin_id
        const pluginName = result.pluginName ?? splitPluginId(pluginId).name
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_enabled_state_updated',
          plugin_id: pluginId,
          plugin_name: safeWireText(pluginName, 256),
          enabled: action.enabled,
          scope: result.scope,
          reverse_dependents: (result.reverseDependents ?? []).slice(0, 128),
        })
      }

      case 'plugin_update': {
        const result = await dependencies.updatePlugin(
          action.plugin_id,
          action.scope,
        )
        if (!result.success) {
          return rejectedOperation(
            dependencies,
            action.kind,
            'plugin_operation_rejected',
          )
        }
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_updated',
          plugin_id: result.pluginId ?? action.plugin_id,
          scope: action.scope,
          old_version: nullableSafeWireText(result.oldVersion, 128),
          new_version: nullableSafeWireText(result.newVersion, 128),
          already_up_to_date: result.alreadyUpToDate === true,
        })
      }

      case 'plugin_marketplace_add': {
        const source = await dependencies.parseMarketplaceInput(
          action.source_input,
        )
        if (source === null || 'error' in source) {
          return runtimeError(action.kind, 'invalid_marketplace_source')
        }
        const result = await dependencies.addMarketplace(source)
        await dependencies.saveMarketplaceDeclaration(
          result.name,
          result.resolvedSource,
        )
        await dependencies.clearPluginCaches()
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_marketplace_added',
          marketplace_name: safeWireText(result.name, 256),
          source_kind: result.resolvedSource.source,
          already_materialized: result.alreadyMaterialized,
        })
      }

      case 'plugin_marketplace_remove':
        await dependencies.removeMarketplace(action.marketplace_name)
        await dependencies.clearPluginCaches()
        return {
          kind: 'plugin_marketplace_removed',
          marketplace_name: action.marketplace_name,
        }

      case 'plugin_marketplace_update': {
        await dependencies.refreshMarketplace(action.marketplace_name)
        const report = await dependencies.updatePluginsForMarketplaces(
          new Set([action.marketplace_name.toLowerCase()]),
        )
        await dependencies.clearPluginCaches()
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_marketplace_updated',
          marketplace_name: action.marketplace_name,
          updated_plugin_ids: report.updatedPluginIds.slice(0, 512),
          plugin_update_failure_count: report.failures.length,
        })
      }

      case 'plugin_marketplace_set_auto_update':
        await dependencies.setMarketplaceAutoUpdate(
          action.marketplace_name,
          action.enabled,
        )
        return {
          kind: 'plugin_marketplace_auto_update_updated',
          marketplace_name: action.marketplace_name,
          enabled: action.enabled,
        }

      case 'plugin_validate': {
        const results = await dependencies.validatePlugin(action.path)
        const primary = results[0]
        if (!primary) {
          return runtimeError(action.kind, 'validation_unavailable')
        }
        const errors = results.flatMap(result => result.errors)
        const warnings = results.flatMap(result => result.warnings)
        const mapDiagnostic = (diagnostic: AuthorityValidationDiagnostic) => ({
          path: safeWireText(diagnostic.path, 512),
          code: nullableSafeWireText(diagnostic.code, 128),
        })
        return UsagePluginRuntimeResultSchema.parse({
          kind: 'plugin_validation_result',
          success: results.every(result => result.success),
          file_type: primary.fileType,
          errors: errors.slice(0, 256).map(mapDiagnostic),
          warnings: warnings.slice(0, 256).map(mapDiagnostic),
          related_result_count: Math.max(0, results.length - 1),
          truncated: errors.length > 256 || warnings.length > 256,
        })
      }
    }
  } catch (error) {
    await reportErrorSafely(dependencies, error)
    return runtimeError(action.kind, errorCodeForAction(action.kind))
  }
}
