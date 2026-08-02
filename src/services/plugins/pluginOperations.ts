/**
 * Core plugin operations (install, uninstall, enable, disable, update)
 *
 * This module provides pure library functions that can be used by both:
 * - CLI commands (`crabcode plugin install/uninstall/enable/disable/update`)
 * - Interactive UI (ManagePlugins.tsx)
 *
 * Functions in this module:
 * - Do NOT call process.exit()
 * - Do NOT write to console
 * - Return result objects indicating success/failure with messages
 * - Can throw errors for unexpected failures
 */
import { join, normalize } from 'path'
import { getOriginalCwd } from '../../bootstrap/state.js'
import { isBuiltinPluginId } from '../../plugins/builtinPlugins.js'
import { logForDebugging } from '../../utils/debug.js'
import { errorMessage, isENOENT, toError } from '../../utils/errors.js'
import { getFsImplementation } from '../../utils/fsOperations.js'
import { logError } from '../../utils/log.js'
import { markPluginVersionOrphaned } from '../../utils/plugins/cacheUtils.js'
import {
  findReverseDependents,
  formatReverseDependentsSuffix,
} from '../../utils/plugins/dependencyResolver.js'
import { pathsEqual } from '../../utils/file.js'
import {
  commitInstalledPluginsRegistryWithSibling,
  isInstalledPluginCachePathUsable,
  loadInstalledPluginsFromDisk,
  loadInstalledPluginsV2,
} from '../../utils/plugins/installedPluginsManager.js'
import {
  getMarketplace,
  getPluginById,
  loadKnownMarketplacesConfig,
  withPluginMarketplaceMembershipLock,
} from '../../utils/plugins/marketplaceManager.js'
import { deletePluginDataDir } from '../../utils/plugins/pluginDirectories.js'
import { bumpPluginLifecycleGeneration } from '../../utils/plugins/pluginLifecycle.js'
import {
  parsePluginIdentifier,
  scopeToSettingSource,
} from '../../utils/plugins/pluginIdentifier.js'
import {
  formatResolutionError,
  installResolvedPlugin,
  installResolvedPluginBatch,
  type ResolvedPluginBatchRoot,
  updateResolvedPluginInstallation,
} from '../../utils/plugins/pluginInstallationHelpers.js'
import { resolveInstalledPluginIdentityForUninstall } from './pluginExactIdentity.js'
import { loadAllPlugins } from '../../utils/plugins/pluginLoader.js'
import { deletePluginOptions } from '../../utils/plugins/pluginOptionsStorage.js'
import { isPluginBlockedByPolicy } from '../../utils/plugins/pluginPolicy.js'
import { getPluginEditableScopes } from '../../utils/plugins/pluginStartupCheck.js'
import type {
  PluginMarketplaceEntry,
  PluginScope,
} from '../../utils/plugins/schemas.js'
import {
  getSettingsForSource,
  updateSettingsForSource,
} from '../../utils/settings/settings.js'
import { plural } from '../../utils/stringUtils.js'

/** Valid installable scopes (excludes 'managed' which can only be installed from managed-settings.json) */
export const VALID_INSTALLABLE_SCOPES = ['user', 'project', 'local'] as const

/** Installation scope type derived from VALID_INSTALLABLE_SCOPES */
export type InstallableScope = (typeof VALID_INSTALLABLE_SCOPES)[number]

/** Valid scopes for update operations (includes 'managed' since managed plugins can be updated) */
export const VALID_UPDATE_SCOPES: readonly PluginScope[] = [
  'user',
  'project',
  'local',
  'managed',
] as const

/**
 * Assert that a scope is a valid installable scope at runtime
 * @param scope The scope to validate
 * @throws Error if scope is not a valid installable scope
 */
export function assertInstallableScope(
  scope: string,
): asserts scope is InstallableScope {
  if (!VALID_INSTALLABLE_SCOPES.includes(scope as InstallableScope)) {
    throw new Error(
      `Invalid scope "${scope}". Must be one of: ${VALID_INSTALLABLE_SCOPES.join(', ')}`,
    )
  }
}

/**
 * Type guard to check if a scope is an installable scope (not 'managed').
 * Use this for type narrowing in conditional blocks.
 */
export function isInstallableScope(
  scope: PluginScope,
): scope is InstallableScope {
  return VALID_INSTALLABLE_SCOPES.includes(scope as InstallableScope)
}

/**
 * Get the project path for scopes that are project-specific.
 * Returns the original cwd for 'project' and 'local' scopes, undefined otherwise.
 */
export function getProjectPathForScope(scope: PluginScope): string | undefined {
  return scope === 'project' || scope === 'local' ? getOriginalCwd() : undefined
}

/**
 * Is this plugin enabled (value === true) in .crabcode/settings.json?
 *
 * Distinct from V2 installed_plugins.json scope: that file tracks where a
 * plugin was *installed from*, but the same plugin can also be enabled at
 * project scope via settings. The uninstall UI needs to check THIS, because
 * a user-scope install with a project-scope enablement means "uninstall"
 * would succeed at removing the user install while leaving the project
 * enablement active — the plugin keeps running.
 */
export function isPluginEnabledAtProjectScope(pluginId: string): boolean {
  return (
    getSettingsForSource('projectSettings')?.enabledPlugins?.[pluginId] === true
  )
}

// ============================================================================
// Result Types
// ============================================================================

/**
 * Result of a plugin operation
 */
export type PluginOperationResult = {
  success: boolean
  message: string
  pluginId?: string
  pluginName?: string
  scope?: PluginScope
  /** Plugins that declare this plugin as a dependency (warning on uninstall/disable) */
  reverseDependents?: string[]
}

/**
 * Result of a plugin update operation
 */
export type PluginUpdateResult = {
  success: boolean
  message: string
  pluginId?: string
  newVersion?: string
  oldVersion?: string
  alreadyUpToDate?: boolean
  scope?: PluginScope
}

// ============================================================================
// Helper Functions
// ============================================================================

/**
 * Search all editable settings scopes for a plugin ID matching the given input.
 *
 * If `plugin` contains `@`, it's treated as a full pluginId and returned if
 * found in any scope. If `plugin` is a bare name, searches for any key
 * starting with `{plugin}@` in any scope.
 *
 * Returns the most specific scope where the plugin is mentioned (regardless
 * of enabled/disabled state) plus the resolved full pluginId.
 *
 * Precedence: local > project > user (most specific wins).
 */
function findPluginInSettings(plugin: string): {
  pluginId: string
  scope: InstallableScope
} | null {
  const hasMarketplace = plugin.includes('@')
  // Most specific first — first match wins
  const searchOrder: InstallableScope[] = ['local', 'project', 'user']

  for (const scope of searchOrder) {
    const enabledPlugins = getSettingsForSource(
      scopeToSettingSource(scope),
    )?.enabledPlugins
    if (!enabledPlugins) continue

    for (const key of Object.keys(enabledPlugins)) {
      if (hasMarketplace ? key === plugin : key.startsWith(`${plugin}@`)) {
        return { pluginId: key, scope }
      }
    }
  }
  return null
}

/**
 * Get the most relevant installation for a plugin from V2 data.
 * For project/local scoped plugins, prioritizes installations matching the current project.
 * Priority order: local (matching project) > project (matching project) > user > first available
 */
export function getPluginInstallationFromV2(pluginId: string): {
  scope: PluginScope
  projectPath?: string
} {
  const installedData = loadInstalledPluginsV2()
  const installations = installedData.plugins[pluginId]

  if (!installations || installations.length === 0) {
    return { scope: 'user' }
  }

  const currentProjectPath = getOriginalCwd()

  // Find installations by priority: local > project > user > managed
  const localInstall = installations.find(
    (inst) => inst.scope === 'local' && inst.projectPath === currentProjectPath,
  )
  if (localInstall) {
    return { scope: localInstall.scope, projectPath: localInstall.projectPath }
  }

  const projectInstall = installations.find(
    (inst) =>
      inst.scope === 'project' && inst.projectPath === currentProjectPath,
  )
  if (projectInstall) {
    return {
      scope: projectInstall.scope,
      projectPath: projectInstall.projectPath,
    }
  }

  const userInstall = installations.find((inst) => inst.scope === 'user')
  if (userInstall) {
    return { scope: userInstall.scope }
  }

  // Fall back to first installation (could be managed)
  return {
    scope: installations[0]!.scope,
    projectPath: installations[0]!.projectPath,
  }
}

// ============================================================================
// Core Operations
// ============================================================================

/**
 * Install a plugin (settings-first).
 *
 * Order of operations:
 *   1. Search materialized marketplaces for the plugin
 *   2. Write settings (THE ACTION — declares intent)
 *   3. Cache plugin + record version hint (materialization)
 *
 * Marketplace reconciliation is NOT this function's responsibility — startup
 * reconcile handles declared-but-not-materialized marketplaces. If the
 * marketplace isn't found, "not found" is the correct error.
 *
 * @param plugin Plugin identifier (name or plugin@marketplace)
 * @param scope Installation scope: user, project, or local (defaults to 'user')
 * @returns Result indicating success/failure
 */
export async function installPluginOp(
  plugin: string,
  scope: InstallableScope = 'user',
  expectedMarketplaceInstallLocation?: string,
): Promise<PluginOperationResult> {
  assertInstallableScope(scope)

  const { name: pluginName, marketplace: marketplaceName } =
    parsePluginIdentifier(plugin)

  // ── Search materialized marketplaces for the plugin ──
  let foundPlugin: PluginMarketplaceEntry | undefined
  let foundMarketplace: string | undefined
  let marketplaceInstallLocation: string | undefined
  let marketplaceGenerationId: string | undefined
  let marketplaceContentDigest: string | undefined

  if (marketplaceName) {
    const pluginInfo = await getPluginById(plugin)
    if (pluginInfo) {
      if (expectedMarketplaceInstallLocation !== undefined) {
        const fs = getFsImplementation()
        let actualLocation: string
        let expectedLocation: string
        try {
          actualLocation = normalize(
            fs.realpathSync(pluginInfo.marketplaceInstallLocation),
          )
          expectedLocation = normalize(
            fs.realpathSync(expectedMarketplaceInstallLocation),
          )
        } catch (error) {
          return {
            success: false,
            message: `Marketplace selector for "${marketplaceName}" could not be verified: ${errorMessage(error)}`,
          }
        }
        const pathIdentity = (value: string): string =>
          process.platform === 'win32' ? value.toLowerCase() : value
        if (pathIdentity(actualLocation) !== pathIdentity(expectedLocation)) {
          return {
            success: false,
            message: `Marketplace selector for "${marketplaceName}" changed before plugin installation`,
          }
        }
      }
      foundPlugin = pluginInfo.entry
      foundMarketplace = marketplaceName
      marketplaceInstallLocation = pluginInfo.marketplaceInstallLocation
      marketplaceGenerationId = pluginInfo.marketplaceGenerationId
      marketplaceContentDigest = pluginInfo.marketplaceContentDigest
    }
  } else if (expectedMarketplaceInstallLocation !== undefined) {
    return {
      success: false,
      message:
        'An expected marketplace path requires a fully-qualified plugin identifier',
    }
  } else {
    const marketplaces = await loadKnownMarketplacesConfig()
    for (const mktName of Object.keys(marketplaces)) {
      try {
        const pluginInfo = await getPluginById(`${pluginName}@${mktName}`)
        if (pluginInfo) {
          foundPlugin = pluginInfo.entry
          foundMarketplace = mktName
          marketplaceInstallLocation = pluginInfo.marketplaceInstallLocation
          marketplaceGenerationId = pluginInfo.marketplaceGenerationId
          marketplaceContentDigest = pluginInfo.marketplaceContentDigest
          break
        }
      } catch (error) {
        logError(toError(error))
        continue
      }
    }
  }

  if (!foundPlugin || !foundMarketplace) {
    const location = marketplaceName
      ? `marketplace "${marketplaceName}"`
      : 'any configured marketplace'
    return {
      success: false,
      message: `Plugin "${pluginName}" not found in ${location}`,
    }
  }

  const entry = foundPlugin
  const pluginId = `${entry.name}@${foundMarketplace}`

  const result = await installResolvedPlugin({
    pluginId,
    entry,
    scope,
    marketplaceInstallLocation,
    marketplaceGenerationId,
    marketplaceContentDigest,
  })

  if (!result.ok) {
    switch (result.reason) {
      case 'local-source-no-location':
        return {
          success: false,
          message: `Cannot install local plugin "${result.pluginName}" without marketplace install location`,
        }
      case 'settings-write-failed':
        return {
          success: false,
          message: `Failed to update settings: ${result.message}`,
        }
      case 'resolution-failed':
        return {
          success: false,
          message: formatResolutionError(result.resolution),
        }
      case 'blocked-by-policy':
        return {
          success: false,
          message: `Plugin "${result.pluginName}" is blocked by your organization's policy and cannot be installed`,
        }
      case 'dependency-blocked-by-policy':
        return {
          success: false,
          message: `Plugin "${result.pluginName}" depends on "${result.blockedDependency}", which is blocked by your organization's policy`,
        }
    }
  }

  return {
    success: true,
    message: `Successfully installed plugin: ${pluginId} (scope: ${scope})${result.depNote}`,
    pluginId,
    pluginName: entry.name,
    scope,
  }
}

/**
 * Result of a one-click batch install.
 */
export type BatchInstallResult = {
  /** pluginIds installed + enabled this batch (excludes already-installed). */
  installed: string[]
  /** Failure that aborted the atomic batch. `installed` is empty when set. */
  failed: { pluginId: string; message: string }[]
}

/**
 * Install **every not-yet-installed plugin** from a single cached
 * marketplace in one call.
 *
 *   * Resolves the install set backend-side from the cached marketplace
 *     catalog (`getMarketplace(name).plugins`) — the caller passes only the
 *     marketplace **name**, never an id list, so it does not have to wait
 *     for an async `marketplace/added` → stateSync refresh to learn which
 *     plugins exist (no immediate-read assumption on eventually-consistent
 *     global state).
 *   * Resolves every root and dependency before mutation, de-duplicates their
 *     closures, stages every cache generation, then performs exactly one
 *     installed-registry + settings sibling commit.
 *   * Atomic failure: policy/resolution/materialization/commit failure returns
 *     `installed: []`; staged bytes are cleaned and no peer is published.
 *   * Idempotent: already-installed plugins are skipped (not re-installed,
 *     not reported as failures), so re-running "install all" is a safe no-op
 *     for the plugins already present.
 *
 * Throws only if the marketplace itself can't be resolved (not cached) —
 * it surfaces as a fail-loud error rather than a fake success.
 *
 * @param marketplaceName The cached marketplace name (e.g. the official market).
 * @param scope Installation scope (defaults to 'user', matching plugin/install).
 */
export async function batchInstallMarketplaceOp(
  marketplaceName: string,
  scope: InstallableScope = 'user',
): Promise<BatchInstallResult> {
  assertInstallableScope(scope)

  // Resolve the install set from the cached catalog (no network — the
  // marketplace was already pulled by the preceding `marketplace/add`).
  const marketplace = await getMarketplace(marketplaceName)

  const roots: ResolvedPluginBatchRoot[] = []
  const seenPluginIds = new Set<string>()
  for (const entry of marketplace.plugins) {
    const pluginId = `${entry.name}@${marketplaceName}`
    if (seenPluginIds.has(pluginId)) {
      return {
        installed: [],
        failed: [
          {
            pluginId,
            message: `Marketplace "${marketplaceName}" contains duplicate plugin id "${pluginId}"`,
          },
        ],
      }
    }
    seenPluginIds.add(pluginId)

    const snapshot = await getPluginById(pluginId)
    if (!snapshot) {
      return {
        installed: [],
        failed: [
          {
            pluginId,
            message: `Plugin "${entry.name}" not found in marketplace "${marketplaceName}"`,
          },
        ],
      }
    }
    roots.push({ pluginId, snapshot })
  }

  const result = await installResolvedPluginBatch({ roots, scope })
  if (!result.ok) return { installed: [], failed: result.failures }
  return { installed: result.changedPluginIds, failed: [] }
}

/**
 * Uninstall a plugin
 *
 * @param plugin Plugin name or exact plugin@marketplace identifier. Bare names
 * resolve only when one installed marketplace key matches.
 * @param scope Uninstall from scope: user, project, or local (defaults to 'user')
 * @returns Result indicating success/failure
 */
export async function uninstallPluginOp(
  plugin: string,
  scope: InstallableScope = 'user',
  deleteDataDir = true,
): Promise<PluginOperationResult> {
  assertInstallableScope(scope)
  const projectPath = getProjectPathForScope(scope)
  const { enabled, disabled } = await loadAllPlugins()
  const allPlugins = [...enabled, ...disabled]
  const settingSource = scopeToSettingSource(scope)

  type LockedUninstallResult = {
    pluginId: string
    pluginName: string
    installPath: string
    isLastScope: boolean
    installPathUnreferenced: boolean
  }
  class LockedUninstallRejected extends Error {
    constructor(
      readonly reason: 'invalid' | 'ambiguous' | 'not-found' | 'scope-missing',
      readonly candidates: string[] = [],
      readonly actualScope?: PluginScope,
    ) {
      super(reason)
    }
  }

  let committed: LockedUninstallResult
  try {
    committed = await withPluginMarketplaceMembershipLock(async () =>
      commitInstalledPluginsRegistryWithSibling(
        (draft) => {
          // The lock-free caller state is at most a hint. Bare-name uniqueness
          // and exact existence are resolved again from this strict locked
          // draft; 0 or >1 matches abort before the sibling callback runs.
          const lockedResolution = resolveInstalledPluginIdentityForUninstall(
            plugin,
            draft.plugins,
          )
          if (lockedResolution.status === 'invalid') {
            throw new LockedUninstallRejected('invalid')
          }
          if (lockedResolution.status === 'ambiguous') {
            throw new LockedUninstallRejected(
              'ambiguous',
              lockedResolution.candidates,
            )
          }
          if (lockedResolution.status === 'not-found') {
            throw new LockedUninstallRejected('not-found')
          }

          const { pluginId, pluginName, installations } = lockedResolution
          const targetIndex = installations.findIndex(
            (installation) =>
              installation.scope === scope &&
              installation.projectPath === projectPath,
          )
          if (targetIndex < 0) {
            throw new LockedUninstallRejected(
              'scope-missing',
              [],
              installations[0]?.scope,
            )
          }
          const target = installations[targetIndex]!
          const remaining = installations.filter(
            (_installation, index) => index !== targetIndex,
          )
          if (remaining.length === 0) delete draft.plugins[pluginId]
          else draft.plugins[pluginId] = remaining

          const installPathUnreferenced = !Object.values(draft.plugins)
            .flat()
            .some(
              (installation) => installation.installPath === target.installPath,
            )
          return {
            changed: true,
            result: {
              pluginId,
              pluginName,
              installPath: target.installPath,
              isLastScope: remaining.length === 0,
              installPathUnreferenced,
            },
          }
        },
        (result) => {
          const settingsResult = updateSettingsForSource(settingSource, {
            enabledPlugins: { [result.pluginId]: undefined },
          })
          if (settingsResult.error) throw settingsResult.error
        },
      ),
    )
  } catch (error) {
    if (error instanceof LockedUninstallRejected) {
      if (error.reason === 'invalid') {
        return {
          success: false,
          message: `Plugin "${plugin}" must be a plugin name or fully-qualified plugin@marketplace identifier`,
        }
      }
      if (error.reason === 'ambiguous') {
        return {
          success: false,
          message: `Plugin name "${plugin}" is ambiguous across installed marketplaces (${error.candidates.join(', ')}). Use a fully-qualified plugin@marketplace identifier.`,
        }
      }
      if (error.reason === 'not-found') {
        return {
          success: false,
          message: `Plugin "${plugin}" not found in installed plugins`,
        }
      }
      if (error.actualScope === 'project') {
        return {
          success: false,
          message: `Plugin "${plugin}" is enabled at project scope (.crabcode/settings.json, shared with your team). To disable just for you: crabcode plugin disable ${plugin} --scope local`,
        }
      }
      return {
        success: false,
        message: error.actualScope
          ? `Plugin "${plugin}" is installed in ${error.actualScope} scope, not ${scope}. Use --scope ${error.actualScope} to uninstall.`
          : `Plugin "${plugin}" is not installed in ${scope} scope. Use --scope to specify the correct scope.`,
      }
    }
    return {
      success: false,
      message: `Failed to uninstall plugin "${plugin}": ${errorMessage(error)}`,
    }
  }

  // 前台生命周期事件（卸载）：registry+settings 事务已成功返回、锁已释放，
  // 在任何可失败的物理清理（markPluginVersionOrphaned / deletePluginOptions /
  // deletePluginDataDir）之前 bump——清理失败绝不能吞掉代次翻转。原
  // clearAllCaches() 由 bump 内部覆盖（clearAllCaches + 快照重建）。
  bumpPluginLifecycleGeneration()
  if (committed.installPathUnreferenced) {
    await markPluginVersionOrphaned(committed.installPath)
  }
  if (committed.isLastScope) {
    try {
      await deletePluginOptions(committed.pluginId)
      if (deleteDataDir) await deletePluginDataDir(committed.pluginId)
    } catch (error) {
      logForDebugging(
        `Plugin logical uninstall committed but optional data cleanup failed: ${errorMessage(error)}`,
        { level: 'warn' },
      )
    }
  }

  // Warn (don't block) if other enabled plugins depend on this one.
  // Blocking creates tombstones — can't tear down a graph with a delisted
  // plugin. Load-time verifyAndDemote catches the fallout.
  const reverseDependents = findReverseDependents(
    committed.pluginId,
    allPlugins,
  )
  const depWarn = formatReverseDependentsSuffix(reverseDependents)

  return {
    success: true,
    message: `Successfully uninstalled plugin: ${committed.pluginName} (scope: ${scope})${depWarn}`,
    pluginId: committed.pluginId,
    pluginName: committed.pluginName,
    scope,
    reverseDependents:
      reverseDependents.length > 0 ? reverseDependents : undefined,
  }
}

/**
 * Set plugin enabled/disabled status (settings-first).
 *
 * Resolves the plugin ID and scope from settings — does NOT pre-gate on
 * installed_plugins.json. Settings declares intent; if the plugin isn't
 * cached yet, the next load will cache it.
 *
 * @param plugin Plugin name or plugin@marketplace identifier
 * @param enabled true to enable, false to disable
 * @param scope Optional scope. If not provided, auto-detects the most specific
 *   scope where the plugin is mentioned in settings.
 * @returns Result indicating success/failure
 */
export async function setPluginEnabledOp(
  plugin: string,
  enabled: boolean,
  scope?: InstallableScope,
): Promise<PluginOperationResult> {
  const operation = enabled ? 'enable' : 'disable'

  // Built-in plugins: always use user-scope settings, bypass the normal
  // scope-resolution + installed_plugins lookup (they're not installed).
  if (isBuiltinPluginId(plugin)) {
    const { error } = updateSettingsForSource('userSettings', {
      enabledPlugins: {
        ...getSettingsForSource('userSettings')?.enabledPlugins,
        [plugin]: enabled,
      },
    })
    if (error) {
      return {
        success: false,
        message: `Failed to ${operation} built-in plugin: ${error.message}`,
      }
    }
    // 前台生命周期事件（builtin 启停同样算）：原 clearAllCaches() 替换为
    // bump（内部已做 clearAllCaches + 快照重建 + 跨进程代次翻转）。
    bumpPluginLifecycleGeneration()
    const { name: pluginName } = parsePluginIdentifier(plugin)
    return {
      success: true,
      message: `Successfully ${operation}d built-in plugin: ${pluginName}`,
      pluginId: plugin,
      pluginName,
      scope: 'user',
    }
  }

  if (scope) {
    assertInstallableScope(scope)
  }

  // ── Resolve pluginId and scope from settings ──
  // Search across editable scopes for any mention (enabled or disabled) of
  // this plugin. Does NOT pre-gate on installed_plugins.json.
  let pluginId: string
  let resolvedScope: InstallableScope

  const found = findPluginInSettings(plugin)

  if (scope) {
    // Explicit scope: use it. Resolve pluginId from settings if possible,
    // otherwise require a full plugin@marketplace identifier.
    resolvedScope = scope
    if (found) {
      pluginId = found.pluginId
    } else if (plugin.includes('@')) {
      pluginId = plugin
    } else {
      return {
        success: false,
        message: `Plugin "${plugin}" not found in settings. Use plugin@marketplace format.`,
      }
    }
  } else if (found) {
    // Auto-detect scope: use the most specific scope where the plugin is
    // mentioned in settings.
    pluginId = found.pluginId
    resolvedScope = found.scope
  } else if (plugin.includes('@')) {
    // Not in any settings scope, but full pluginId given — default to user
    // scope (matches install default). This allows enabling a plugin that
    // was cached but never declared.
    pluginId = plugin
    resolvedScope = 'user'
  } else {
    return {
      success: false,
      message: `Plugin "${plugin}" not found in any editable settings scope. Use plugin@marketplace format.`,
    }
  }

  // ── Policy guard ──
  // Org-blocked plugins cannot be enabled at any scope. Check after pluginId
  // is resolved so we catch both full identifiers and bare-name lookups.
  if (enabled && isPluginBlockedByPolicy(pluginId)) {
    return {
      success: false,
      message: `Plugin "${pluginId}" is blocked by your organization's policy and cannot be enabled`,
    }
  }

  const settingSource = scopeToSettingSource(resolvedScope)
  const scopeSettingsValue =
    getSettingsForSource(settingSource)?.enabledPlugins?.[pluginId]

  // ── Cross-scope hint: explicit scope given but plugin is elsewhere ──
  // If the plugin is absent from the requested scope but present at a
  // different scope, guide the user to the right --scope — UNLESS they're
  // writing to a higher-precedence scope to override a lower one
  // (e.g. `disable --scope local` to override a project-enabled plugin
  // without touching the shared .crabcode/settings.json).
  const SCOPE_PRECEDENCE: Record<InstallableScope, number> = {
    user: 0,
    project: 1,
    local: 2,
  }
  const isOverride =
    scope && found && SCOPE_PRECEDENCE[scope] > SCOPE_PRECEDENCE[found.scope]
  if (
    scope &&
    scopeSettingsValue === undefined &&
    found &&
    found.scope !== scope &&
    !isOverride
  ) {
    return {
      success: false,
      message: `Plugin "${plugin}" is installed at ${found.scope} scope, not ${scope}. Use --scope ${found.scope} or omit --scope to auto-detect.`,
    }
  }

  // ── Check current state (for idempotency messaging) ──
  // When explicit scope given: check that scope's settings value directly
  // (merged state can be wrong if plugin is enabled elsewhere but disabled here).
  // When auto-detected: use merged effective state.
  // When overriding a lower scope: check merged state — scopeSettingsValue is
  // undefined (plugin not in this scope yet), which would read as "already
  // disabled", but the whole point of the override is to write an explicit
  // `false` that masks the lower scope's `true`.
  const isCurrentlyEnabled =
    scope && !isOverride
      ? scopeSettingsValue === true
      : getPluginEditableScopes().has(pluginId)
  if (enabled === isCurrentlyEnabled) {
    return {
      success: false,
      message: `Plugin "${plugin}" is already ${enabled ? 'enabled' : 'disabled'}${scope ? ` at ${scope} scope` : ''}`,
    }
  }

  // On disable: capture reverse dependents from the PRE-disable snapshot,
  // before we write settings and clear the memoized plugin cache.
  let reverseDependents: string[] | undefined
  if (!enabled) {
    const { enabled: loadedEnabled, disabled } = await loadAllPlugins()
    const rdeps = findReverseDependents(pluginId, [
      ...loadedEnabled,
      ...disabled,
    ])
    if (rdeps.length > 0) reverseDependents = rdeps
  }

  // ── Enable must materialize ──
  // Startup migration no longer materializes `false` keys ("known but
  // disabled" is not an install intent), so a legacy installed-then-disabled
  // plugin can have a settings key with NO registry record / cache bytes.
  // Flipping such a plugin to true must install it through the coordinated
  // transaction — writing `true` alone would produce an "enabled but nothing
  // to load" dead state. Disable and enable-with-usable-bytes are unchanged.
  if (enabled) {
    const installations = loadInstalledPluginsV2().plugins[pluginId] ?? []
    const expectedProjectPath = getProjectPathForScope(resolvedScope)
    let hasUsableInstallation = false
    for (const entry of installations) {
      const scopeMatches =
        entry.scope === 'user' ||
        entry.scope === 'managed' ||
        (entry.scope === resolvedScope &&
          entry.projectPath !== undefined &&
          expectedProjectPath !== undefined &&
          pathsEqual(entry.projectPath, expectedProjectPath))
      if (!scopeMatches) continue
      if (await isInstalledPluginCachePathUsable(entry.installPath)) {
        hasUsableInstallation = true
        break
      }
    }

    if (!hasUsableInstallation) {
      const pluginInfo = await getPluginById(pluginId)
      if (!pluginInfo) {
        return {
          success: false,
          message: `Plugin "${pluginId}" has no usable installation and was not found in any configured marketplace. Install the plugin before enabling it.`,
        }
      }

      // installResolvedPlugin commits the registry generation AND writes
      // `enabledPlugins[pluginId] = true` for this scope as its settings
      // sibling (plus the lifecycle bump — do NOT bump again on this path),
      // so on success we skip the duplicate settings write below. On failure
      // nothing is written — no dead state.
      const result = await installResolvedPlugin({
        pluginId,
        entry: pluginInfo.entry,
        scope: resolvedScope,
        marketplaceInstallLocation: pluginInfo.marketplaceInstallLocation,
        marketplaceGenerationId: pluginInfo.marketplaceGenerationId,
        marketplaceContentDigest: pluginInfo.marketplaceContentDigest,
      })

      if (!result.ok) {
        switch (result.reason) {
          case 'local-source-no-location':
            return {
              success: false,
              message: `Cannot install local plugin "${result.pluginName}" without marketplace install location`,
            }
          case 'settings-write-failed':
            return {
              success: false,
              message: `Failed to update settings: ${result.message}`,
            }
          case 'resolution-failed':
            return {
              success: false,
              message: formatResolutionError(result.resolution),
            }
          case 'blocked-by-policy':
            return {
              success: false,
              message: `Plugin "${result.pluginName}" is blocked by your organization's policy and cannot be installed`,
            }
          case 'dependency-blocked-by-policy':
            return {
              success: false,
              message: `Plugin "${result.pluginName}" depends on "${result.blockedDependency}", which is blocked by your organization's policy`,
            }
        }
      }

      const { name: materializedName } = parsePluginIdentifier(pluginId)
      return {
        success: true,
        message: `Successfully installed and enabled plugin: ${materializedName} (scope: ${resolvedScope})${result.depNote}`,
        pluginId,
        pluginName: materializedName,
        scope: resolvedScope,
      }
    }
  }

  // ── ACTION: write settings ──
  const { error } = updateSettingsForSource(settingSource, {
    enabledPlugins: {
      ...getSettingsForSource(settingSource)?.enabledPlugins,
      [pluginId]: enabled,
    },
  })
  if (error) {
    return {
      success: false,
      message: `Failed to ${operation} plugin: ${error.message}`,
    }
  }

  // 前台生命周期事件（纯 settings 启停写）：原 clearAllCaches() 替换为
  // bump（内部已做 clearAllCaches + 快照重建 + 跨进程代次翻转）。
  bumpPluginLifecycleGeneration()

  const { name: pluginName } = parsePluginIdentifier(pluginId)
  const depWarn = formatReverseDependentsSuffix(reverseDependents)
  return {
    success: true,
    message: `Successfully ${operation}d plugin: ${pluginName} (scope: ${resolvedScope})${depWarn}`,
    pluginId,
    pluginName,
    scope: resolvedScope,
    reverseDependents,
  }
}

/**
 * Enable a plugin
 *
 * @param plugin Plugin name or plugin@marketplace identifier
 * @param scope Optional scope. If not provided, finds the most specific scope for the current project.
 * @returns Result indicating success/failure
 */
export async function enablePluginOp(
  plugin: string,
  scope?: InstallableScope,
): Promise<PluginOperationResult> {
  return setPluginEnabledOp(plugin, true, scope)
}

/**
 * Disable a plugin
 *
 * @param plugin Plugin name or plugin@marketplace identifier
 * @param scope Optional scope. If not provided, finds the most specific scope for the current project.
 * @returns Result indicating success/failure
 */
export async function disablePluginOp(
  plugin: string,
  scope?: InstallableScope,
): Promise<PluginOperationResult> {
  return setPluginEnabledOp(plugin, false, scope)
}

/**
 * Disable all enabled plugins
 *
 * @returns Result indicating success/failure with count of disabled plugins
 */
export async function disableAllPluginsOp(): Promise<PluginOperationResult> {
  const enabledPlugins = getPluginEditableScopes()

  if (enabledPlugins.size === 0) {
    return { success: true, message: 'No enabled plugins to disable' }
  }

  const disabled: string[] = []
  const errors: string[] = []

  for (const [pluginId] of enabledPlugins) {
    const result = await setPluginEnabledOp(pluginId, false)
    if (result.success) {
      disabled.push(pluginId)
    } else {
      errors.push(`${pluginId}: ${result.message}`)
    }
  }

  if (errors.length > 0) {
    return {
      success: false,
      message: `Disabled ${disabled.length} ${plural(disabled.length, 'plugin')}, ${errors.length} failed:\n${errors.join('\n')}`,
    }
  }

  return {
    success: true,
    message: `Disabled ${disabled.length} ${plural(disabled.length, 'plugin')}`,
  }
}

/**
 * Update a plugin to the latest version.
 *
 * This function performs a generation-scoped NON-INPLACE update: it prepares
 * unpublished bytes, validates the exact marketplace generation under lock,
 * publishes a unique cache path, and replaces only the still-current installed
 * scope through the coordinated registry transaction. A concurrent uninstall
 * or reinstall therefore fails the update instead of being overwritten.
 *
 * @param plugin Plugin name or plugin@marketplace identifier
 * @param scope Scope to update. Unlike install/uninstall/enable/disable, managed scope IS allowed.
 * @returns Result indicating success/failure with version info
 */
export async function updatePluginOp(
  plugin: string,
  scope: PluginScope,
): Promise<PluginUpdateResult> {
  const diskData = loadInstalledPluginsFromDisk()
  const identity = resolveInstalledPluginIdentityForUninstall(
    plugin,
    diskData.plugins,
  )
  const fallbackName = parsePluginIdentifier(plugin).name || plugin
  if (identity.status === 'invalid') {
    return {
      success: false,
      message: `Invalid plugin identifier "${plugin}"`,
      pluginId: plugin,
      scope,
    }
  }
  if (identity.status === 'ambiguous') {
    return {
      success: false,
      message: `Plugin name "${plugin}" is ambiguous across installed marketplaces (${identity.candidates.join(', ')}). Use a fully-qualified plugin@marketplace identifier.`,
      pluginId: plugin,
      scope,
    }
  }
  if (identity.status === 'not-found') {
    return {
      success: false,
      message: `Plugin "${fallbackName}" is not installed`,
      pluginId: plugin,
      scope,
    }
  }

  const { pluginId, pluginName, installations } = identity

  // Get plugin info from marketplace
  const pluginInfo = await getPluginById(pluginId)
  if (!pluginInfo) {
    return {
      success: false,
      message: `Plugin "${pluginName}" not found`,
      pluginId,
      scope,
    }
  }

  const {
    entry,
    marketplaceInstallLocation,
    marketplaceGenerationId,
    marketplaceContentDigest,
  } = pluginInfo

  // Determine projectPath based on scope
  const projectPath = getProjectPathForScope(scope)

  // Find the installation for this scope
  const installation = installations.find(
    (inst) => inst.scope === scope && inst.projectPath === projectPath,
  )
  if (!installation) {
    const scopeDesc = projectPath ? `${scope} (${projectPath})` : scope
    return {
      success: false,
      message: `Plugin "${pluginName}" is not installed at scope ${scopeDesc}`,
      pluginId,
      scope,
    }
  }

  return performPluginUpdate({
    pluginId,
    pluginName,
    entry,
    marketplaceInstallLocation,
    marketplaceGenerationId,
    marketplaceContentDigest,
    installation,
    scope,
    projectPath,
  })
}

/**
 * Perform the coordinated generation-scoped update selected by updatePluginOp.
 */
async function performPluginUpdate({
  pluginId,
  pluginName,
  entry,
  marketplaceInstallLocation,
  marketplaceGenerationId,
  marketplaceContentDigest,
  installation,
  scope,
  projectPath,
}: {
  pluginId: string
  pluginName: string
  entry: PluginMarketplaceEntry
  marketplaceInstallLocation: string
  marketplaceGenerationId: string
  marketplaceContentDigest: string
  installation: { version?: string; installPath: string }
  scope: PluginScope
  projectPath: string | undefined
}): Promise<PluginUpdateResult> {
  const oldVersion = installation.version
  const update = await updateResolvedPluginInstallation({
    pluginId,
    snapshot: {
      entry,
      marketplaceInstallLocation,
      marketplaceGenerationId,
      marketplaceContentDigest,
    },
    scope,
    projectPath,
    expectedInstallPath: installation.installPath,
    expectedVersion: oldVersion,
  })
  if (update.alreadyUpToDate) {
    return {
      success: true,
      message: `${pluginName} is already at the latest version (${update.version}).`,
      pluginId,
      newVersion: update.version,
      oldVersion,
      alreadyUpToDate: true,
      scope,
    }
  }
  const updatedScopeDesc = projectPath ? `${scope} (${projectPath})` : scope
  return {
    success: true,
    message: `Plugin "${pluginName}" updated from ${oldVersion || 'unknown'} to ${update.version} for scope ${updatedScopeDesc}. Restart to apply changes.`,
    pluginId,
    newVersion: update.version,
    oldVersion,
    scope,
  }

}
