/**
 * Zip Cache Adapters
 *
 * I/O helpers for the plugin zip cache. These functions handle reading/writing
 * zip-cache-local metadata files, extracting ZIPs to session directories,
 * and creating ZIPs for newly installed plugins.
 *
 * The zip cache stores data on a mounted volume (e.g., Filestore) that persists
 * across ephemeral container lifetimes. The session cache is a local temp dir
 * for extracted plugins used during a single session.
 */

import { realpath, stat } from 'fs/promises'
import { dirname, join } from 'path'
import { logForDebugging } from '../debug.js'
import { jsonParse, jsonStringify } from '../slowOperations.js'
import { loadKnownMarketplacesConfigSafe } from './marketplaceManager.js'
import { getPluginsDirectory } from './pluginDirectories.js'
import {
  readCanonicalPluginTextFile,
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from './pluginPathSecurity.js'
import {
  isLocalMarketplaceSource,
  type KnownMarketplacesFile,
  KnownMarketplacesFileSchema,
  type PluginMarketplace,
  PluginMarketplaceSchema,
  type MarketplaceSource,
} from './schemas.js'
import {
  atomicWriteToZipCache,
  getMarketplaceJsonRelativePath,
  getPluginZipCachePath,
  getZipCacheKnownMarketplacesPath,
} from './zipCache.js'

// ── Metadata I/O ──

/**
 * Read known_marketplaces.json from the zip cache.
 * Returns empty object if file doesn't exist, can't be parsed, or fails schema
 * validation (data comes from a shared mounted volume — other containers may write).
 */
export async function readZipCacheKnownMarketplaces(): Promise<KnownMarketplacesFile> {
  try {
    const cacheRoot = getPluginZipCachePath()
    if (!cacheRoot) return {}
    const knownPath = await resolveInternalPluginPath(
      cacheRoot,
      getZipCacheKnownMarketplacesPath(),
      {
        rejectSymlinks: true,
        component: 'ZIP cache known marketplaces',
      },
    )
    const content = await readCanonicalPluginTextFile(
      cacheRoot,
      knownPath,
      'ZIP cache known marketplaces',
    )
    const parsed = KnownMarketplacesFileSchema().safeParse(jsonParse(content))
    if (!parsed.success) {
      logForDebugging(
        `Invalid known_marketplaces.json in zip cache: ${parsed.error.message}`,
        { level: 'error' },
      )
      return {}
    }
    return parsed.data
  } catch {
    return {}
  }
}

/**
 * Write known_marketplaces.json to the zip cache atomically.
 */
export async function writeZipCacheKnownMarketplaces(
  data: KnownMarketplacesFile,
): Promise<void> {
  await atomicWriteToZipCache(
    getZipCacheKnownMarketplacesPath(),
    jsonStringify(data, null, 2),
  )
}

// ── Marketplace JSON ──

/**
 * Read a marketplace JSON file from the zip cache.
 */
export async function readMarketplaceJson(
  marketplaceName: string,
): Promise<PluginMarketplace | null> {
  const zipCachePath = getPluginZipCachePath()
  if (!zipCachePath) {
    return null
  }
  const relPath = getMarketplaceJsonRelativePath(marketplaceName)
  try {
    const fullPath = await resolveInternalPluginPath(
      zipCachePath,
      join(zipCachePath, relPath),
      { rejectSymlinks: true, component: 'ZIP cache marketplace JSON' },
    )
    const content = await readCanonicalPluginTextFile(
      zipCachePath,
      fullPath,
      'ZIP cache marketplace JSON',
    )
    const parsed = jsonParse(content)
    const result = PluginMarketplaceSchema().safeParse(parsed)
    if (result.success) {
      return result.data
    }
    logForDebugging(
      `Invalid marketplace JSON for ${marketplaceName}: ${result.error}`,
    )
    return null
  } catch {
    return null
  }
}

/**
 * Save a marketplace JSON to the zip cache from its install location.
 */
export async function saveMarketplaceJsonToZipCache(
  marketplaceName: string,
  installLocation: string,
  source: MarketplaceSource,
): Promise<void> {
  const zipCachePath = getPluginZipCachePath()
  if (!zipCachePath) {
    return
  }
  const content = await readMarketplaceJsonContent(installLocation, source)
  if (content !== null) {
    const relPath = getMarketplaceJsonRelativePath(marketplaceName)
    await atomicWriteToZipCache(join(zipCachePath, relPath), content)
  }
}

/**
 * Read marketplace.json content from a cloned marketplace directory or file.
 * For directory sources: checks .crabcode-plugin/marketplace.json, marketplace.json
 * For URL sources: the installLocation IS the marketplace JSON file itself.
 */
async function readMarketplaceJsonContent(
  installLocation: string,
  source: MarketplaceSource,
): Promise<string | null> {
  let safeInstallLocation: string
  if (isLocalMarketplaceSource(source)) {
    safeInstallLocation = await realpath(installLocation)
  } else {
    const marketplaceRoot = await resolvePluginComponentPath(
      getPluginsDirectory(),
      'marketplaces',
      { rejectSymlinks: true, component: 'marketplace cache root' },
    )
    safeInstallLocation = await resolveInternalPluginPath(
      marketplaceRoot,
      installLocation,
      {
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'marketplace install location',
      },
    )
  }

  if ((await stat(safeInstallLocation)).isFile()) {
    return readCanonicalPluginTextFile(
      dirname(safeInstallLocation),
      safeInstallLocation,
      'marketplace JSON for ZIP cache',
    )
  }

  for (const relativePath of [
    '.crabcode-plugin/marketplace.json',
    'marketplace.json',
  ]) {
    try {
      const candidate = await resolvePluginComponentPath(
        safeInstallLocation,
        relativePath,
        { component: 'marketplace JSON for ZIP cache' },
      )
      return await readCanonicalPluginTextFile(
        safeInstallLocation,
        candidate,
        'marketplace JSON for ZIP cache',
      )
    } catch {
      // ENOENT (doesn't exist) or EISDIR (directory) — try next
    }
  }
  return null
}

/**
 * Sync marketplace data to zip cache for offline access.
 * Saves marketplace JSONs and merges with previously cached data
 * so ephemeral containers can access marketplaces without re-cloning.
 */
export async function syncMarketplacesToZipCache(): Promise<void> {
  // Read-only iteration — Safe variant so a corrupted config doesn't throw.
  // This runs during startup paths; a throw here cascades to the same
  // try-block that catches loadAllPlugins failures.
  const knownMarketplaces = await loadKnownMarketplacesConfigSafe()

  // Save marketplace JSONs to zip cache
  for (const [name, entry] of Object.entries(knownMarketplaces)) {
    if (!entry.installLocation) continue
    try {
      await saveMarketplaceJsonToZipCache(
        name,
        entry.installLocation,
        entry.source,
      )
    } catch (error) {
      logForDebugging(`Failed to save marketplace JSON for ${name}: ${error}`)
    }
  }

  // Merge with previously cached data (ephemeral containers lose global config)
  const zipCacheKnownMarketplaces = await readZipCacheKnownMarketplaces()
  const mergedKnownMarketplaces: KnownMarketplacesFile = {
    ...zipCacheKnownMarketplaces,
    ...knownMarketplaces,
  }
  await writeZipCacheKnownMarketplaces(mergedKnownMarketplaces)
}
