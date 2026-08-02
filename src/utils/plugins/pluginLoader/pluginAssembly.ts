/**
 * Top-level plugin assembly, session loading, merge, and cache management.
 *
 * This module provides the main entry points (loadAllPlugins,
 * loadAllPluginsCacheOnly) and orchestrates the full load pipeline.
 */

import memoize from 'lodash-es/memoize.js'
import { basename, resolve } from 'path'
import { getInlinePlugins } from '../../../bootstrap/state.js'
import {
  getBuiltinPlugins,
} from '../../../plugins/builtinPlugins.js'
import type {
  LoadedPlugin,
  PluginError,
  PluginLoadResult,
} from '../../../types/plugin.js'
import { logForDebugging } from '../../debug.js'
import { isEnvTruthy } from '../../envUtils.js'
import { errorMessage } from '../../errors.js'
import { logError } from '../../log.js'
import {
  clearPluginSettingsBase,
  getPluginSettingsBase,
  resetSettingsCache,
  setPluginSettingsBase,
} from '../../settings/settingsCache.js'
import { verifyAndDemote } from '../dependencyResolver.js'
import { getManagedPluginNames } from '../managedPlugins.js'
import { PluginPathSecurityError } from '../pluginPathSecurity.js'
import {
  createPluginFromPath,
  recordPluginComponentPathFailure,
} from './createPlugin.js'
import { loadPluginsFromMarketplaces } from './marketplaceLoader.js'

/**
 * Load session-only plugins from --plugin-dir CLI flag.
 *
 * These plugins are loaded directly without going through the marketplace system.
 * They appear with source='plugin-name@inline' and are always enabled for the current session.
 */
async function loadSessionOnlyPlugins(
  sessionPluginPaths: Array<string>,
): Promise<{ plugins: LoadedPlugin[]; errors: PluginError[] }> {
  if (sessionPluginPaths.length === 0) {
    return { plugins: [], errors: [] }
  }

  const plugins: LoadedPlugin[] = []
  const errors: PluginError[] = []

  for (const [index, pluginPath] of sessionPluginPaths.entries()) {
    const resolvedPath = resolve(pluginPath)
    try {
      const dirName = basename(resolvedPath)
      const { plugin, errors: pluginErrors } = await createPluginFromPath(
        resolvedPath,
        `${dirName}@inline`,
        true,
        dirName,
      )

      // Update source to use the actual plugin name from manifest
      plugin.source = `${plugin.name}@inline`
      plugin.repository = `${plugin.name}@inline`

      plugins.push(plugin)
      errors.push(...pluginErrors)

      logForDebugging(`Loaded inline plugin from path: ${plugin.name}`)
    } catch (error) {
      if (error instanceof PluginPathSecurityError) {
        recordPluginComponentPathFailure(errors, error, {
          pluginPath: resolvedPath,
          requestedPath: '.',
          pluginName: basename(resolvedPath),
          source: `inline[${index}]`,
          component: 'plugin-root',
        })
        continue
      }
      const errorMsg = errorMessage(error)
      logForDebugging(
        `Failed to load session plugin from ${pluginPath}: ${errorMsg}`,
        { level: 'warn' },
      )
      errors.push({
        type: 'generic-error',
        source: `inline[${index}]`,
        error: `Failed to load plugin: ${errorMsg}`,
      })
    }
  }

  if (plugins.length > 0) {
    logForDebugging(
      `Loaded ${plugins.length} session-only plugins from --plugin-dir`,
    )
  }

  return { plugins, errors }
}

export { loadSessionOnlyPlugins as __loadSessionOnlyPluginsForTest }

/**
 * Merge plugins from session (--plugin-dir), marketplace (installed), and
 * builtin sources. Session plugins override marketplace plugins with the
 * same name.
 *
 * Exception: marketplace plugins locked by managed settings (policySettings)
 * cannot be overridden.
 */
export function mergePluginSources(sources: {
  session: LoadedPlugin[]
  marketplace: LoadedPlugin[]
  builtin: LoadedPlugin[]
  managedNames?: Set<string> | null
}): { plugins: LoadedPlugin[]; errors: PluginError[] } {
  const errors: PluginError[] = []
  const managed = sources.managedNames

  const sessionPlugins = sources.session.filter(p => {
    if (managed?.has(p.name)) {
      logForDebugging(
        `Plugin "${p.name}" from --plugin-dir is blocked by managed settings`,
        { level: 'warn' },
      )
      errors.push({
        type: 'generic-error',
        source: p.source,
        plugin: p.name,
        error: `--plugin-dir copy of "${p.name}" ignored: plugin is locked by managed settings`,
      })
      return false
    }
    return true
  })

  const sessionNames = new Set(sessionPlugins.map(p => p.name))
  const marketplacePlugins = sources.marketplace.filter(p => {
    if (sessionNames.has(p.name)) {
      logForDebugging(
        `Plugin "${p.name}" from --plugin-dir overrides installed version`,
      )
      return false
    }
    return true
  })
  return {
    plugins: [...sessionPlugins, ...marketplacePlugins, ...sources.builtin],
    errors,
  }
}

/**
 * Main plugin loading function that discovers and loads all plugins.
 *
 * This function is memoized to avoid repeated filesystem scanning and is
 * the primary entry point for the plugin system.
 */
export const loadAllPlugins = memoize(async (): Promise<PluginLoadResult> => {
  const result = await assemblePluginLoadResult(() =>
    loadPluginsFromMarketplaces({ cacheOnly: false }),
  )
  // Warm the cache-only memoize so downstream consumers see just-cloned
  // plugins. Wave D: the cache-only memoize is keyed by targetCwd with
  // '@process' as the no-arg sentinel — warm that key, not `undefined`.
  loadAllPluginsCacheOnly.cache?.set('@process', Promise.resolve(result))
  return result
})

/**
 * Cache-only variant of loadAllPlugins.
 *
 * Same merge/dependency/settings logic, but the marketplace loader never
 * hits the network. Reads from installed_plugins.json's installPath.
 *
 * CRABCODE_SYNC_PLUGIN_INSTALL=1 delegates to the full loader.
 *
 * Wave D (R2 根修)：可选 `targetCwd` 决定 project/local 安装记录按哪个 cwd
 * 判相关性（透传 marketplace loader）。memoize 按 targetCwd 键化，零参 =
 * '@process'（进程 cwd 语义，既有调用点全部零参 → 行为不变）；
 * `clearPluginCache` 整表 clear，天然覆盖所有键。
 */
export const loadAllPluginsCacheOnly = memoize(
  async (targetCwd?: string): Promise<PluginLoadResult> => {
    if (isEnvTruthy(process.env.CRABCODE_SYNC_PLUGIN_INSTALL)) {
      return loadAllPlugins()
    }
    return assemblePluginLoadResult(() =>
      loadPluginsFromMarketplaces({ cacheOnly: true, targetCwd }),
    )
  },
  (targetCwd?: string) => targetCwd ?? '@process',
)

/**
 * Security-boundary cache-only loader. Unlike `loadAllPluginsCacheOnly`, this
 * entry point can never delegate to the network-enabled loader through
 * `CRABCODE_SYNC_PLUGIN_INSTALL`; callers use it while authorizing a local
 * configuration write against the already-installed plugin set.
 */
export async function loadAllPluginsStrictCacheOnly(): Promise<PluginLoadResult> {
  return assemblePluginLoadResult(() =>
    loadPluginsFromMarketplaces({ cacheOnly: true }),
  )
}

/**
 * Shared body of loadAllPlugins and loadAllPluginsCacheOnly.
 */
async function assemblePluginLoadResult(
  marketplaceLoader: () => Promise<{
    plugins: LoadedPlugin[]
    errors: PluginError[]
  }>,
): Promise<PluginLoadResult> {
  const inlinePlugins = getInlinePlugins()
  const [marketplaceResult, sessionResult] = await Promise.all([
    marketplaceLoader(),
    inlinePlugins.length > 0
      ? loadSessionOnlyPlugins(inlinePlugins)
      : Promise.resolve({ plugins: [], errors: [] }),
  ])
  const builtinResult = getBuiltinPlugins()

  const { plugins: allPlugins, errors: mergeErrors } = mergePluginSources({
    session: sessionResult.plugins,
    marketplace: marketplaceResult.plugins,
    builtin: [...builtinResult.enabled, ...builtinResult.disabled],
    managedNames: getManagedPluginNames(),
  })
  const allErrors = [
    ...marketplaceResult.errors,
    ...sessionResult.errors,
    ...mergeErrors,
  ]

  // Verify dependencies
  const { demoted, errors: depErrors } = verifyAndDemote(allPlugins)
  for (const p of allPlugins) {
    if (demoted.has(p.source)) p.enabled = false
  }
  allErrors.push(...depErrors)

  const enabledPlugins = allPlugins.filter(p => p.enabled)
  logForDebugging(
    `Found ${allPlugins.length} plugins (${enabledPlugins.length} enabled, ${allPlugins.length - enabledPlugins.length} disabled)`,
  )

  // Cache plugin settings for synchronous access by the settings cascade
  cachePluginSettings(enabledPlugins)

  return {
    enabled: enabledPlugins,
    disabled: allPlugins.filter(p => !p.enabled),
    errors: allErrors,
  }
}

/**
 * Clears the memoized plugin cache.
 *
 * Call this when plugins are installed, removed, or settings change
 * to force a fresh scan on the next loadAllPlugins call.
 */
export function clearPluginCache(reason?: string): void {
  if (reason) {
    logForDebugging(
      `clearPluginCache: invalidating loadAllPlugins cache (${reason})`,
    )
  }
  loadAllPlugins.cache?.clear?.()
  loadAllPluginsCacheOnly.cache?.clear?.()
  if (getPluginSettingsBase() !== undefined) {
    resetSettingsCache()
  }
  clearPluginSettingsBase()
}

/**
 * Merge settings from all enabled plugins into a single record.
 * Later plugins override earlier ones for the same key.
 */
function mergePluginSettings(
  plugins: LoadedPlugin[],
): Record<string, unknown> | undefined {
  let merged: Record<string, unknown> | undefined

  for (const plugin of plugins) {
    if (!plugin.settings) {
      continue
    }

    if (!merged) {
      merged = {}
    }

    for (const [key, value] of Object.entries(plugin.settings)) {
      if (key in merged) {
        logForDebugging(
          `Plugin "${plugin.name}" overrides setting "${key}" (previously set by another plugin)`,
        )
      }
      merged[key] = value
    }
  }

  return merged
}

/**
 * Store merged plugin settings in the synchronous cache.
 * Called after loadAllPlugins resolves.
 */
export function cachePluginSettings(plugins: LoadedPlugin[]): void {
  const settings = mergePluginSettings(plugins)
  setPluginSettingsBase(settings)
  if (settings && Object.keys(settings).length > 0) {
    resetSettingsCache()
    logForDebugging(
      `Cached plugin settings with keys: ${Object.keys(settings).join(', ')}`,
    )
  }
}
