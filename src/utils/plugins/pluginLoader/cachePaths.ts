/**
 * Cache path computation and probing utilities.
 *
 * Handles versioned, legacy, seed, and ZIP cache path resolution
 * for the plugin system.
 */

import { readdir } from 'fs/promises'
import { dirname, join } from 'path'
import { getFsImplementation } from '../../fsOperations.js'
import { parsePluginIdentifier } from '../pluginIdentifier.js'
import { getPluginSeedDirs, getPluginsDirectory } from '../pluginDirectories.js'
import {
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from '../pluginPathSecurity.js'

async function resolveCachePathIn(
  baseDir: string,
  candidatePath: string,
  mustExist = true,
): Promise<string> {
  const cacheRoot = await resolvePluginComponentPath(baseDir, 'cache', {
    mustExist,
    rejectSymlinks: true,
    component: 'plugin cache root',
  })
  return resolveInternalPluginPath(cacheRoot, candidatePath, {
    mustExist,
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'plugin cache path',
  })
}

/**
 * Get the path where plugin cache is stored
 */
export function getPluginCachePath(): string {
  return join(getPluginsDirectory(), 'cache')
}

/**
 * Compute the versioned cache path under a specific base plugins directory.
 * Used to probe both primary and seed caches.
 *
 * @param baseDir - Base plugins directory (e.g. getPluginsDirectory() or seed dir)
 * @param pluginId - Plugin identifier in format "name@marketplace"
 * @param version - Version string (semver, git SHA, etc.)
 * @returns Absolute path to versioned plugin directory under baseDir
 */
export function getVersionedCachePathIn(
  baseDir: string,
  pluginId: string,
  version: string,
): string {
  const { name: pluginName, marketplace } = parsePluginIdentifier(pluginId)
  const sanitizedMarketplace =
    (marketplace || 'unknown').replace(/[^a-zA-Z0-9\-_]/g, '-') ||
    'unknown'
  const sanitizedPlugin =
    (pluginName || pluginId).replace(/[^a-zA-Z0-9\-_]/g, '-') ||
    'unknown'
  // Sanitize version to prevent path traversal attacks
  const sanitizedVersion =
    version.replace(/[^a-zA-Z0-9\-_.]/g, '-') || 'unknown'
  return join(
    baseDir,
    'cache',
    sanitizedMarketplace,
    sanitizedPlugin,
    sanitizedVersion,
  )
}

/**
 * Get versioned cache path for a plugin under the primary plugins directory.
 * Format: ~/.crabcode/plugins/cache/{marketplace}/{plugin}/{version}/
 *
 * @param pluginId - Plugin identifier in format "name@marketplace"
 * @param version - Version string (semver, git SHA, etc.)
 * @returns Absolute path to versioned plugin directory
 */
export function getVersionedCachePath(
  pluginId: string,
  version: string,
): string {
  return getVersionedCachePathIn(getPluginsDirectory(), pluginId, version)
}

/**
 * Get versioned ZIP cache path for a plugin.
 * This is the zip cache variant of getVersionedCachePath.
 */
export function getVersionedZipCachePath(
  pluginId: string,
  version: string,
): string {
  return `${getVersionedCachePath(pluginId, version)}.zip`
}

/**
 * Probe seed directories for a populated cache at this plugin version.
 * Seeds are checked in precedence order; first hit wins. Returns null if no
 * seed is configured or none contains a populated directory at this version.
 */
export async function probeSeedCache(
  pluginId: string,
  version: string,
): Promise<string | null> {
  for (const seedDir of getPluginSeedDirs()) {
    const candidatePath = getVersionedCachePathIn(seedDir, pluginId, version)
    try {
      const seedPath = await resolveCachePathIn(seedDir, candidatePath)
      const entries = await readdir(seedPath)
      if (entries.length > 0) return seedPath
    } catch {
      // Try next seed
    }
  }
  return null
}

/**
 * When the computed version is 'unknown', probe seed/cache/<m>/<p>/ for an
 * actual version dir. Handles the first-boot chicken-and-egg where the
 * version can only be known after cloning, but seed already has the clone.
 *
 * Per seed, only matches when exactly one version exists (typical BYOC case).
 * Multiple versions within a single seed -> ambiguous -> try next seed.
 * Seeds are checked in precedence order; first match wins.
 */
export async function probeSeedCacheAnyVersion(
  pluginId: string,
): Promise<string | null> {
  for (const seedDir of getPluginSeedDirs()) {
    // The parent of the version dir -- computed the same way as
    // getVersionedCachePathIn, just without the version component.
    const candidatePluginDir = dirname(
      getVersionedCachePathIn(seedDir, pluginId, '_'),
    )
    try {
      const pluginDir = await resolveCachePathIn(seedDir, candidatePluginDir)
      const versions = await readdir(pluginDir)
      if (versions.length !== 1) continue
      const versionDir = await resolvePluginComponentPath(
        pluginDir,
        versions[0]!,
        { component: 'seed plugin cache version' },
      )
      const entries = await readdir(versionDir)
      if (entries.length > 0) return versionDir
    } catch {
      // Try next seed
    }
  }
  return null
}

/**
 * Get legacy (non-versioned) cache path for a plugin.
 * Format: ~/.crabcode/plugins/cache/{plugin-name}/
 *
 * Used for backward compatibility with existing installations.
 *
 * @param pluginName - Plugin name (without marketplace suffix)
 * @returns Absolute path to legacy plugin directory
 */
export function getLegacyCachePath(pluginName: string): string {
  const cachePath = getPluginCachePath()
  const safeName =
    pluginName.replace(/[^a-zA-Z0-9\-_]/g, '-') || 'unknown'
  return join(cachePath, safeName)
}

/**
 * Resolve plugin path with fallback to legacy location.
 *
 * Always:
 * 1. Try versioned path first if version is provided
 * 2. Fall back to legacy path for existing installations
 * 3. Return versioned path for new installations
 *
 * @param pluginId - Plugin identifier in format "name@marketplace"
 * @param version - Optional version string
 * @returns Absolute path to plugin directory
 */
export async function resolvePluginPath(
  pluginId: string,
  version?: string,
): Promise<string> {
  // Try versioned path first
  if (version) {
    try {
      return await resolveCachePathIn(
        getPluginsDirectory(),
        getVersionedCachePath(pluginId, version),
      )
    } catch {
      // Fall through to the legacy cache path.
    }
  }

  // Fall back to legacy path for existing installations
  const pluginName = parsePluginIdentifier(pluginId).name || pluginId
  const legacyPath = getLegacyCachePath(pluginName)
  try {
    return await resolveCachePathIn(getPluginsDirectory(), legacyPath)
  } catch {
    // Return the canonical creation target below.
  }

  // Return versioned path for new installations
  const candidate = version ? getVersionedCachePath(pluginId, version) : legacyPath
  const pluginsRoot = getPluginsDirectory()
  await getFsImplementation().mkdir(pluginsRoot)
  const cacheRoot = await resolvePluginComponentPath(pluginsRoot, 'cache', {
    mustExist: false,
    rejectSymlinks: true,
    component: 'plugin cache root',
  })
  await getFsImplementation().mkdir(cacheRoot)
  return resolveInternalPluginPath(cacheRoot, candidate, {
    mustExist: false,
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'plugin cache creation target',
  })
}
