/**
 * Plugin Loader Module — barrel re-export.
 *
 * This module re-exports all public API from the pluginLoader/ subdirectory.
 * Consumer import paths remain unchanged:
 *   import { loadAllPlugins } from './pluginLoader.js'
 */

// --- cachePaths ---
export {
  getPluginCachePath,
  getVersionedCachePathIn,
  getVersionedCachePath,
  getVersionedZipCachePath,
  probeSeedCacheAnyVersion,
  getLegacyCachePath,
  resolvePluginPath,
} from './pluginLoader/cachePaths.js'

// --- copyAndCache ---
export { copyDir } from './pluginLoader/copyAndCache.js'

// --- installSources ---
export {
  installFromNpm,
  gitClone,
  installFromGitSubdir,
  generateTemporaryCacheNameForPlugin,
  cachePlugin,
} from './pluginLoader/installSources.js'

// --- createPlugin ---
export {
  loadPluginManifest,
  createPluginFromPath,
} from './pluginLoader/createPlugin.js'

// --- pluginAssembly ---
export {
  mergePluginSources,
  loadAllPlugins,
  loadAllPluginsCacheOnly,
  loadAllPluginsStrictCacheOnly,
  clearPluginCache,
  cachePluginSettings,
} from './pluginLoader/pluginAssembly.js'
