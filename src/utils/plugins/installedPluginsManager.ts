/**
 * Manages plugin installation metadata stored in installed_plugins.json
 *
 * This module separates plugin installation state (global) from enabled/disabled
 * state (per-repository). The installed_plugins.json file tracks:
 * - Which plugins are installed globally
 * - Installation metadata (version, timestamps, paths)
 *
 * The enabled/disabled state remains in .crabcode/settings.json for per-repo control.
 *
 * Rationale: Installation is global (a plugin is either on disk or not), while
 * enabled/disabled state is per-repository (different projects may want different
 * plugins active).
 */

import { homedir } from 'os'
import { dirname, isAbsolute, join, relative, resolve, sep } from 'path'
import { withCrossProcessResourceLockSync } from '../crossProcessResourceLock.js'
import { logForDebugging } from '../debug.js'
import { errorMessage, isENOENT, toError } from '../errors.js'
import { pathsEqual, writeFileSyncAtomicNoFallback } from '../file.js'
import { getFsImplementation } from '../fsOperations.js'
import { logError } from '../log.js'
import { jsonParse, jsonStringify } from '../slowOperations.js'
import { getPluginSeedDirs, getPluginsDirectory } from './pluginDirectories.js'
import {
  type InstalledPlugin,
  InstalledPluginsFileSchemaV1,
  InstalledPluginsFileSchemaV2,
  type InstalledPluginsFileV1,
  type InstalledPluginsFileV2,
  type PluginInstallationEntry,
  type PluginMarketplaceEntry,
  type PluginScope,
} from './schemas.js'

// Type alias for V2 plugins map
type InstalledPluginsMapV2 = Record<string, PluginInstallationEntry[]>

// Type for persistable scopes (excludes 'flag' which is session-only)
export type PersistableScope = Exclude<PluginScope, never> // All scopes are persistable in the schema

import { getOriginalCwd } from '../../bootstrap/state.js'
import { logEvent } from '../../services/analytics/index.js'
import type { EditableSettingSource } from '../settings/constants.js'
import {
  getSettings_DEPRECATED,
  getSettingsFilePathForSource,
  getSettingsForSource,
  updateSettingsForSource,
} from '../settings/settings.js'
import { getPluginById } from './marketplaceManager.js'
import {
  parsePluginIdentifier,
  settingSourceToScope,
} from './pluginIdentifier.js'
import {
  getPluginCachePath,
  getVersionedCachePath,
} from './pluginLoader/cachePaths.js'
import {
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from './pluginPathSecurity.js'

// Migration state to prevent running migration multiple times per session
let migrationCompleted = false

/**
 * Memoized cache of installed plugins data (V2 format)
 * Cleared by clearInstalledPluginsCache() when file is modified.
 * Prevents repeated filesystem reads within a single CLI session.
 */
let installedPluginsCacheV2: InstalledPluginsFileV2 | null = null

/**
 * Session-level snapshot of installed plugins at startup.
 * This is what the running session uses - it's NOT updated by background operations.
 * Background updates modify the disk file only.
 */
let inMemoryInstalledPlugins: InstalledPluginsFileV2 | null = null

export type InstalledPluginsRegistryHealth =
  | { ok: true }
  | { ok: false; message: string }

/**
 * Health of the most recent registry read (loadInstalledPluginsV2).
 *
 * `ok: false` means installed_plugins.json exists on disk but could not be
 * read/parsed/validated — single-point corruption that must NOT masquerade
 * as "nothing installed". Consumers (worker plugin/list, skills/list) use
 * this to fail loud instead of presenting a clean empty state.
 *
 * A missing file (ENOENT) is a legitimate empty state and stays ok: true.
 */
let installedPluginsRegistryHealth: InstalledPluginsRegistryHealth = {
  ok: true,
}

export function getInstalledPluginsRegistryHealth(): InstalledPluginsRegistryHealth {
  return installedPluginsRegistryHealth
}

/**
 * Resolve an installed-registry path inside a trusted primary or seed cache.
 * Marketplace loading and startup health checks share this boundary so a
 * legacy registry entry cannot redirect loading back to mutable source bytes.
 */
export async function resolveInstalledPluginCachePath(
  candidatePath: string,
): Promise<string> {
  const baseRoots = [getPluginsDirectory(), ...getPluginSeedDirs()]
  let lastError: unknown
  for (const baseRoot of baseRoots) {
    try {
      const cacheRoot = await resolvePluginComponentPath(baseRoot, 'cache', {
        rejectSymlinks: true,
        component: 'plugin cache root',
      })
      return await resolveInternalPluginPath(cacheRoot, candidatePath, {
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'installed plugin cache path',
      })
    } catch (error) {
      lastError = error
    }
  }
  throw (
    lastError ?? new Error(`Plugin cache path is not trusted: ${candidatePath}`)
  )
}

export async function isInstalledPluginCachePathUsable(
  candidatePath: string,
): Promise<boolean> {
  try {
    const resolvedPath = await resolveInstalledPluginCachePath(candidatePath)
    const stats = await getFsImplementation().stat(resolvedPath)
    return stats.isDirectory() || (stats.isFile() && resolvedPath.endsWith('.zip'))
  } catch {
    return false
  }
}

/**
 * Get the path to the installed_plugins.json file
 */
export function getInstalledPluginsFilePath(): string {
  return join(getPluginsDirectory(), 'installed_plugins.json')
}

/**
 * Get the path to the legacy installed_plugins_v2.json file.
 * Used only during migration to consolidate into single file.
 */
export function getInstalledPluginsV2FilePath(): string {
  return join(getPluginsDirectory(), 'installed_plugins_v2.json')
}

/**
 * Clear the installed plugins cache
 * Call this when the file is modified to force a reload
 *
 * Note: This also clears the in-memory session state (inMemoryInstalledPlugins).
 * In most cases, this is only called during initialization or testing.
 */
export function clearInstalledPluginsCache(): void {
  installedPluginsCacheV2 = null
  inMemoryInstalledPlugins = null
  // Health is re-judged by the next read; a cleared cache must not keep
  // reporting a stale unhealthy (or healthy) verdict.
  installedPluginsRegistryHealth = { ok: true }
  logForDebugging('Cleared installed plugins cache')
}

/**
 * Migrate to single plugin file format.
 *
 * This consolidates the V1/V2 dual-file system into a single file:
 * 1. If installed_plugins_v2.json exists: copy to installed_plugins.json (version=2), delete V2 file
 * 2. If only installed_plugins.json exists with version=1: convert to version=2 in-place
 * 3. Clean up legacy non-versioned cache directories
 *
 * This migration runs once per session at startup.
 */
export function migrateToSinglePluginFile(): void {
  if (migrationCompleted) {
    return
  }

  const fs = getFsImplementation()
  const mainFilePath = getInstalledPluginsFilePath()
  const v2FilePath = getInstalledPluginsV2FilePath()

  try {
    const migratedData = withInstalledPluginsLock(() => {
      // Case 1: Validate the legacy V2 file before renaming it over the main
      // registry. Validation and rename share the transaction lock, so a
      // corrupt legacy generation cannot destroy a valid main generation and
      // another CrabCode process cannot mutate it between read and commit.
      let legacyContent: string | null = null
      try {
        legacyContent = fs.readFileSync(v2FilePath, {
          encoding: 'utf-8',
        })
      } catch (e) {
        if (!isENOENT(e)) throw e
      }

      if (legacyContent !== null) {
        const legacyRaw = jsonParse(legacyContent)
        const legacyVersion =
          typeof legacyRaw?.version === 'number' ? legacyRaw.version : 1
        const legacyData =
          legacyVersion === 2
            ? InstalledPluginsFileSchemaV2().parse(legacyRaw)
            : migrateV1ToV2(InstalledPluginsFileSchemaV1().parse(legacyRaw))

        fs.renameSync(v2FilePath, mainFilePath)
        if (legacyVersion !== 2) {
          saveInstalledPluginsV2WhileLocked(legacyData)
        } else {
          installedPluginsCacheV2 = legacyData
        }
        logForDebugging(
          `Renamed installed_plugins_v2.json to installed_plugins.json`,
        )
        return legacyData
      }

      // Case 2: v2 absent — try reading main; ENOENT = neither exists (case 3)
      let mainContent: string
      try {
        mainContent = fs.readFileSync(mainFilePath, { encoding: 'utf-8' })
      } catch (e) {
        if (!isENOENT(e)) throw e
        return null
      }

      const mainData = jsonParse(mainContent)
      const version =
        typeof mainData?.version === 'number' ? mainData.version : 1

      if (version === 1) {
        // Convert V1 to V2 format in-place using atomic replacement. Never
        // truncate the authoritative file before the new snapshot is durable.
        const v1Data = InstalledPluginsFileSchemaV1().parse(mainData)
        const v2Data = migrateV1ToV2(v1Data)
        saveInstalledPluginsV2WhileLocked(v2Data)
        logForDebugging(
          `Converted installed_plugins.json from V1 to V2 format (${Object.keys(v1Data.plugins).length} plugins)`,
        )
        return v2Data
      }

      if (version === 2) {
        // Validate existing V2 state, but preserve the prior migration
        // contract: legacy cache cleanup only follows an actual rename or
        // V1→V2 conversion, not every startup with an already-current file.
        InstalledPluginsFileSchemaV2().parse(mainData)
        return null
      }

      return null
    })

    if (migratedData) {
      cleanupLegacyCache(migratedData)
    }
    migrationCompleted = true
  } catch (error) {
    const errorMsg = errorMessage(error)
    logForDebugging(`Failed to migrate plugin files: ${errorMsg}`, {
      level: 'error',
    })
    logError(toError(error))
    // Fail closed: initialization must not synthesize a partial main registry
    // after a lock/read/validation/write failure. Keep the migration retryable
    // for the next initialization attempt and surface this failure to stop the
    // current sync pipeline.
    migrationCompleted = false
    throw error
  }
}

/**
 * Clean up legacy non-versioned cache directories.
 *
 * Legacy cache structure: ~/.crabcode/plugins/cache/{plugin-name}/
 * Versioned cache structure: ~/.crabcode/plugins/cache/{marketplace}/{plugin}/{version}/
 *
 * This function removes legacy directories that are not referenced by any installation.
 */
function cleanupLegacyCache(v2Data: InstalledPluginsFileV2): void {
  const fs = getFsImplementation()
  const cachePath = getPluginCachePath()
  try {
    // Collect all install paths that are referenced
    const referencedPaths = new Set<string>()
    for (const installations of Object.values(v2Data.plugins)) {
      for (const entry of installations) {
        referencedPaths.add(entry.installPath)
      }
    }

    // List top-level directories in cache
    const entries = fs.readdirSync(cachePath)

    for (const dirent of entries) {
      if (!dirent.isDirectory()) {
        continue
      }

      const entry = dirent.name
      const entryPath = join(cachePath, entry)

      // Check if this is a versioned cache (marketplace dir with plugin/version subdirs)
      // or a legacy cache (flat plugin directory)
      const subEntries = fs.readdirSync(entryPath)
      const hasVersionedStructure = subEntries.some((subDirent) => {
        if (!subDirent.isDirectory()) return false
        const subPath = join(entryPath, subDirent.name)
        // Check if subdir contains version directories (semver-like or hash)
        const versionEntries = fs.readdirSync(subPath)
        return versionEntries.some((vDirent) => vDirent.isDirectory())
      })

      if (hasVersionedStructure) {
        // This is a marketplace directory with versioned structure - skip
        continue
      }

      // This is a legacy flat cache directory
      // Check if it's referenced by any installation
      if (!referencedPaths.has(entryPath)) {
        // Not referenced - safe to delete
        fs.rmSync(entryPath, { recursive: true, force: true })
        logForDebugging(`Cleaned up legacy cache directory: ${entry}`)
      }
    }
  } catch (error) {
    const errorMsg = errorMessage(error)
    logForDebugging(`Failed to clean up legacy cache: ${errorMsg}`, {
      level: 'warn',
    })
  }
}

/**
 * Reset migration state (for testing)
 */
export function resetMigrationState(): void {
  migrationCompleted = false
}

/**
 * Read raw file data from installed_plugins.json
 * Returns null if file doesn't exist.
 * Throws error if file exists but can't be parsed.
 */
function readInstalledPluginsFileRaw(): {
  version: number
  data: unknown
} | null {
  const fs = getFsImplementation()
  const filePath = getInstalledPluginsFilePath()

  let fileContent: string
  try {
    fileContent = fs.readFileSync(filePath, { encoding: 'utf-8' })
  } catch (e) {
    if (isENOENT(e)) {
      return null
    }
    throw e
  }
  const data = jsonParse(fileContent)
  const version = typeof data?.version === 'number' ? data.version : 1
  return { version, data }
}

/**
 * Migrate V1 data to V2 format.
 * All V1 plugins are migrated to 'user' scope since V1 had no scope concept.
 */
function migrateV1ToV2(v1Data: InstalledPluginsFileV1): InstalledPluginsFileV2 {
  const v2Plugins: InstalledPluginsMapV2 = {}

  for (const [pluginId, plugin] of Object.entries(v1Data.plugins)) {
    // V2 format uses versioned cache path: ~/.crabcode/plugins/cache/{marketplace}/{plugin}/{version}
    // Compute it from pluginId and version instead of using the V1 installPath
    const versionedCachePath = getVersionedCachePath(pluginId, plugin.version)

    v2Plugins[pluginId] = [
      {
        scope: 'user', // Default all existing installs to user scope
        installPath: versionedCachePath,
        version: plugin.version,
        installedAt: plugin.installedAt,
        lastUpdated: plugin.lastUpdated,
        gitCommitSha: plugin.gitCommitSha,
      },
    ]
  }

  return { version: 2, plugins: v2Plugins }
}

/**
 * Load installed plugins in V2 format.
 *
 * Reads from installed_plugins.json. If file has version=1,
 * converts to V2 format in memory.
 *
 * @returns V2 format data with array-per-plugin structure
 */
export function loadInstalledPluginsV2(): InstalledPluginsFileV2 {
  // Return cached V2 data if available
  if (installedPluginsCacheV2 !== null) {
    return installedPluginsCacheV2
  }

  const filePath = getInstalledPluginsFilePath()

  try {
    const rawData = readInstalledPluginsFileRaw()

    if (rawData) {
      if (rawData.version === 2) {
        // V2 format - validate and return
        const validated = InstalledPluginsFileSchemaV2().parse(rawData.data)
        installedPluginsCacheV2 = validated
        installedPluginsRegistryHealth = { ok: true }
        logForDebugging(
          `Loaded ${Object.keys(validated.plugins).length} installed plugins from ${filePath}`,
        )
        return validated
      }

      // V1 format - convert to V2
      const v1Validated = InstalledPluginsFileSchemaV1().parse(rawData.data)
      const v2Data = migrateV1ToV2(v1Validated)
      installedPluginsCacheV2 = v2Data
      installedPluginsRegistryHealth = { ok: true }
      logForDebugging(
        `Loaded and converted ${Object.keys(v1Validated.plugins).length} plugins from V1 format`,
      )
      return v2Data
    }

    // File doesn't exist - return empty V2 (legitimate empty state: cache
    // it and keep health ok)
    logForDebugging(
      `installed_plugins.json doesn't exist, returning empty V2 object`,
    )
    installedPluginsCacheV2 = { version: 2, plugins: {} }
    installedPluginsRegistryHealth = { ok: true }
    return installedPluginsCacheV2
  } catch (error) {
    const errorMsg = errorMessage(error)
    logForDebugging(
      `Failed to load installed_plugins.json: ${errorMsg}. Starting with empty state.`,
      { level: 'error' },
    )
    logError(toError(error))

    // Fail loud, not frozen: record the unhealthy read and return an empty
    // view WITHOUT caching it. Caching the empty set here used to disguise
    // single-point corruption as "nothing installed" for the rest of the
    // session; leaving the cache null means the next call re-reads disk, so
    // fixing the file recovers without a restart.
    installedPluginsRegistryHealth = { ok: false, message: errorMsg }
    return { version: 2, plugins: {} }
  }
}

type InstalledPluginsDiskSnapshot = {
  data: InstalledPluginsFileV2
  fileExists: boolean
}

export type InstalledPluginsRegistryMutationResult<T> = {
  changed: boolean
  result: T
}

export type InstalledPluginsRegistryMutation<T> = (
  draft: InstalledPluginsFileV2,
  fileExists: boolean,
) => InstalledPluginsRegistryMutationResult<T>

export type InstalledPluginsSiblingCommit<T> = (result: T) => void

export const INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED =
  'INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED'
export const INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED =
  'INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED'

export type InstalledPluginsSiblingCommitErrorCode =
  | typeof INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED
  | typeof INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED

/**
 * Stable failure taxonomy for a coordinated installed-registry + sibling
 * commit. `cause` and `siblingError` preserve the first failure; when the
 * compensating registry write also fails, `rollbackError` preserves the
 * second failure and `code` is fail-closed as SIBLING_ROLLBACK_FAILED.
 */
export class InstalledPluginsSiblingCommitError extends Error {
  readonly cause: unknown

  constructor(
    readonly code: InstalledPluginsSiblingCommitErrorCode,
    readonly siblingError: unknown,
    readonly rollbackError?: unknown,
  ) {
    const siblingMessage = errorMessage(siblingError)
    const detail =
      code === INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED
        ? `sibling commit failed: ${siblingMessage}; installed plugin registry rollback failed: ${errorMessage(rollbackError)}`
        : `sibling commit failed: ${siblingMessage}; installed plugin registry restored`
    super(`[${code}] ${detail}`)
    this.name = 'InstalledPluginsSiblingCommitError'
    this.cause = siblingError
  }
}

/**
 * Cross-process transaction guard for installed_plugins.json.
 *
 * The shared resource-lock helper uses proper-lockfile's atomic sibling mkdir
 * and `realpath: false`, so the first write is valid before the registry file
 * exists. A lock acquisition failure is fatal: callers never fall back to an
 * unlocked read-modify-write.
 */
function withInstalledPluginsLock<T>(operation: () => T): T {
  const filePath = getInstalledPluginsFilePath()
  return withCrossProcessResourceLockSync(
    'installed-plugins-registry',
    operation,
    // Windows can keep six concurrent registry writers queued for longer than
    // the ~5.8 s covered by 32 capped retries. Preserve the fail-closed bound,
    // but allow ~25 s so a healthy holder can finish without losing writes.
    { targetPath: filePath, syncAttempts: 128 },
  )
}

/**
 * Strict disk read for mutation paths. Unlike the public fail-soft reader,
 * malformed or unreadable state throws so a writer cannot replace a damaged
 * registry with an empty object.
 */
function loadInstalledPluginsFromDiskStrict(): InstalledPluginsDiskSnapshot {
  const rawData = readInstalledPluginsFileRaw()
  if (!rawData) {
    return { data: { version: 2, plugins: {} }, fileExists: false }
  }

  if (rawData.version === 2) {
    return {
      data: InstalledPluginsFileSchemaV2().parse(rawData.data),
      fileExists: true,
    }
  }

  return {
    data: migrateV1ToV2(InstalledPluginsFileSchemaV1().parse(rawData.data)),
    fileExists: true,
  }
}

/**
 * Persist one fully validated snapshot with crash-safe atomic replacement.
 * The shared no-fallback primitive fsyncs a sibling temp generation and uses
 * rename as the sole commit point; it never truncates the authoritative file.
 * Must only be called while holding `withInstalledPluginsLock`.
 */
function saveInstalledPluginsV2WhileLocked(
  data: InstalledPluginsFileV2,
): void {
  const fs = getFsImplementation()
  const filePath = getInstalledPluginsFilePath()
  const validated = InstalledPluginsFileSchemaV2().parse(data)

  fs.mkdirSync(getPluginsDirectory())
  try {
    writeFileSyncAtomicNoFallback(
      filePath,
      `${jsonStringify(validated, null, 2)}\n`,
      { encoding: 'utf-8', mode: 0o600 },
    )
  } catch (error) {
    logError(toError(error))
    throw error
  }

  installedPluginsCacheV2 = validated
  logForDebugging(
    `Saved ${Object.keys(validated.plugins).length} installed plugins to ${filePath}`,
  )
}

function isThenable(value: unknown): value is PromiseLike<unknown> {
  return (typeof value === 'object' && value !== null) ||
    typeof value === 'function'
    ? typeof (value as { then?: unknown }).then === 'function'
    : false
}

/** Must only be called while holding `withInstalledPluginsLock`. */
function restoreInstalledPluginsSnapshotWhileLocked(
  snapshot: InstalledPluginsDiskSnapshot,
): void {
  if (snapshot.fileExists) {
    saveInstalledPluginsV2WhileLocked(snapshot.data)
    return
  }

  // unlink is the atomic commit point for restoring an originally absent
  // registry. ENOENT already represents the requested pre-state.
  try {
    getFsImplementation().unlinkSync(getInstalledPluginsFilePath())
  } catch (error) {
    if (!isENOENT(error)) throw error
  }
  installedPluginsCacheV2 = { version: 2, plugins: {} }
}

/**
 * Commit one complete installed-registry generation and one synchronous
 * sibling mutation as a coordinated transaction.
 *
 * The mutation runs under the installed-registry cross-process lock against a
 * detached V2 draft. Returning `changed: true` performs exactly one atomic
 * registry replacement before `commitSibling` runs; returning `changed:
 * false` skips the registry write but still runs the sibling callback (useful
 * for repairing sibling-only drift). A thrown mutation never writes either
 * side and never invokes the sibling callback.
 *
 * `commitSibling` must contain only synchronous local commit work. Returning a
 * Promise/thenable is rejected as a sibling failure. If a changed registry was
 * already published, the original strict snapshot is restored while the lock
 * is still held. A failed compensating write invalidates the cache and throws
 * an error that retains both failures; the function never reports success in
 * either failure mode.
 */
export function commitInstalledPluginsRegistryWithSibling<T>(
  mutate: InstalledPluginsRegistryMutation<T>,
  commitSibling: InstalledPluginsSiblingCommit<T>,
): T {
  return withInstalledPluginsLock(() => {
    const diskSnapshot = loadInstalledPluginsFromDiskStrict()
    // Schema parsing gives the rollback snapshot and mutation draft distinct
    // object graphs, so a closure cannot accidentally mutate the saved state.
    const preState: InstalledPluginsDiskSnapshot = {
      data: InstalledPluginsFileSchemaV2().parse(diskSnapshot.data),
      fileExists: diskSnapshot.fileExists,
    }
    const draft = InstalledPluginsFileSchemaV2().parse(preState.data)
    const outcome = mutate(draft, preState.fileExists)

    if (isThenable(outcome)) {
      throw new TypeError(
        'installed plugin registry mutation must be synchronous',
      )
    }
    if (
      !outcome ||
      typeof outcome !== 'object' ||
      typeof outcome.changed !== 'boolean' ||
      !('result' in outcome)
    ) {
      throw new TypeError(
        'installed plugin registry mutation must return { changed, result }',
      )
    }

    if (outcome.changed) {
      saveInstalledPluginsV2WhileLocked(draft)
    }

    try {
      const siblingResult = commitSibling(outcome.result)
      if (isThenable(siblingResult)) {
        throw new TypeError(
          'installed plugin sibling commit must be synchronous',
        )
      }
    } catch (siblingError) {
      if (!outcome.changed) {
        throw new InstalledPluginsSiblingCommitError(
          INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED,
          siblingError,
        )
      }

      try {
        restoreInstalledPluginsSnapshotWhileLocked(preState)
      } catch (rollbackError) {
        // The disk state is uncertain after a failed compensating commit. Do
        // not let a cache of the forward generation masquerade as authority.
        installedPluginsCacheV2 = null
        throw new InstalledPluginsSiblingCommitError(
          INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED,
          siblingError,
          rollbackError,
        )
      }

      throw new InstalledPluginsSiblingCommitError(
        INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED,
        siblingError,
      )
    }

    return outcome.result
  })
}

/**
 * Full read-modify-write transaction. Every mutation re-reads strictly after
 * acquiring the process-shared lock, then commits through atomic replacement.
 */
function mutateInstalledPluginsV2<T>(
  mutation: (
    data: InstalledPluginsFileV2,
    fileExists: boolean,
  ) => InstalledPluginsRegistryMutationResult<T>,
): T {
  return withInstalledPluginsLock(() => {
    const { data, fileExists } = loadInstalledPluginsFromDiskStrict()
    const outcome = mutation(data, fileExists)
    if (outcome.changed) {
      saveInstalledPluginsV2WhileLocked(data)
    }
    return outcome.result
  })
}

/**
 * Add or update a plugin installation entry at a specific scope.
 * Used for V2 format where each plugin has an array of installations.
 *
 * @param pluginId - Plugin ID in "plugin@marketplace" format
 * @param scope - Installation scope (managed/user/project/local)
 * @param installPath - Path to versioned plugin directory
 * @param metadata - Additional installation metadata
 * @param projectPath - Project path (required for project/local scopes)
 */
export function addPluginInstallation(
  pluginId: string,
  scope: PersistableScope,
  installPath: string,
  metadata: Partial<PluginInstallationEntry>,
  projectPath?: string,
): void {
  mutateInstalledPluginsV2((data) => {
    // Get or create array for this plugin
    const installations = data.plugins[pluginId] || []

    // Find existing entry for this scope+projectPath
    const existingIndex = installations.findIndex(
      (entry) => entry.scope === scope && entry.projectPath === projectPath,
    )

    const newEntry: PluginInstallationEntry = {
      scope,
      installPath,
      version: metadata.version,
      installedAt: metadata.installedAt || new Date().toISOString(),
      lastUpdated: new Date().toISOString(),
      gitCommitSha: metadata.gitCommitSha,
      ...(projectPath && { projectPath }),
    }

    if (existingIndex >= 0) {
      installations[existingIndex] = newEntry
      logForDebugging(`Updated installation for ${pluginId} at scope ${scope}`)
    } else {
      installations.push(newEntry)
      logForDebugging(`Added installation for ${pluginId} at scope ${scope}`)
    }

    data.plugins[pluginId] = installations
    return { changed: true, result: undefined }
  })
}

/**
 * Remove a plugin installation entry from a specific scope.
 *
 * @param pluginId - Plugin ID in "plugin@marketplace" format
 * @param scope - Installation scope to remove
 * @param projectPath - Project path (for project/local scopes)
 */
export function removePluginInstallation(
  pluginId: string,
  scope: PersistableScope,
  projectPath?: string,
): void {
  mutateInstalledPluginsV2((data) => {
    const installations = data.plugins[pluginId]

    if (!installations) {
      return { changed: false, result: undefined }
    }

    data.plugins[pluginId] = installations.filter(
      (entry) => !(entry.scope === scope && entry.projectPath === projectPath),
    )

    // Remove plugin entirely if no installations left
    if (data.plugins[pluginId].length === 0) {
      delete data.plugins[pluginId]
    }

    logForDebugging(`Removed installation for ${pluginId} at scope ${scope}`)
    return { changed: true, result: undefined }
  })
}

// =============================================================================
// In-Memory vs Disk State Management (for non-in-place updates)
// =============================================================================

/**
 * Get the in-memory installed plugins (session state).
 * This snapshot is loaded at startup and used for the entire session.
 * It is NOT updated by background operations.
 *
 * @returns V2 format data representing the session's view of installed plugins
 */
export function getInMemoryInstalledPlugins(): InstalledPluginsFileV2 {
  if (inMemoryInstalledPlugins === null) {
    const loaded = loadInstalledPluginsV2()
    if (!installedPluginsRegistryHealth.ok) {
      // Unhealthy read (registry present but unreadable): return this
      // call's empty view but DO NOT freeze it into the session snapshot —
      // the next call retries the disk read, so a repaired file recovers
      // without restarting the session.
      return loaded
    }
    inMemoryInstalledPlugins = loaded
  }
  return inMemoryInstalledPlugins
}

/**
 * Load installed plugins directly from disk, bypassing all caches.
 * Used by background updater to check for changes without affecting
 * the running session's view.
 *
 * @returns V2 format data read fresh from disk
 */
export function loadInstalledPluginsFromDisk(): InstalledPluginsFileV2 {
  try {
    // Read from main file
    const rawData = readInstalledPluginsFileRaw()

    if (rawData) {
      if (rawData.version === 2) {
        return InstalledPluginsFileSchemaV2().parse(rawData.data)
      }
      // V1 format - convert to V2
      const v1Data = InstalledPluginsFileSchemaV1().parse(rawData.data)
      return migrateV1ToV2(v1Data)
    }

    return { version: 2, plugins: {} }
  } catch (error) {
    const errorMsg = errorMessage(error)
    logForDebugging(`Failed to load installed plugins from disk: ${errorMsg}`, {
      level: 'error',
    })
    return { version: 2, plugins: {} }
  }
}

/**
 * Check if there are pending updates (disk differs from memory).
 * This happens when background updater has downloaded new versions.
 *
 * @returns true if any plugin has a different install path on disk vs memory
 */
export function hasPendingUpdates(): boolean {
  const memoryState = getInMemoryInstalledPlugins()
  const diskState = loadInstalledPluginsFromDisk()

  for (const [pluginId, diskInstallations] of Object.entries(
    diskState.plugins,
  )) {
    const memoryInstallations = memoryState.plugins[pluginId]
    if (!memoryInstallations) continue

    for (const diskEntry of diskInstallations) {
      const memoryEntry = memoryInstallations.find(
        (m) =>
          m.scope === diskEntry.scope &&
          m.projectPath === diskEntry.projectPath,
      )
      if (memoryEntry && memoryEntry.installPath !== diskEntry.installPath) {
        return true // Disk has different version than memory
      }
    }
  }

  return false
}

/**
 * Get the count of pending updates (installations where disk differs from memory).
 *
 * @returns Number of installations with pending updates
 */
export function getPendingUpdateCount(): number {
  let count = 0
  const memoryState = getInMemoryInstalledPlugins()
  const diskState = loadInstalledPluginsFromDisk()

  for (const [pluginId, diskInstallations] of Object.entries(
    diskState.plugins,
  )) {
    const memoryInstallations = memoryState.plugins[pluginId]
    if (!memoryInstallations) continue

    for (const diskEntry of diskInstallations) {
      const memoryEntry = memoryInstallations.find(
        (m) =>
          m.scope === diskEntry.scope &&
          m.projectPath === diskEntry.projectPath,
      )
      if (memoryEntry && memoryEntry.installPath !== diskEntry.installPath) {
        count++
      }
    }
  }

  return count
}

/**
 * Get details about pending updates for display.
 *
 * @returns Array of objects with pluginId, scope, oldVersion, newVersion
 */
export function getPendingUpdatesDetails(): Array<{
  pluginId: string
  scope: string
  oldVersion: string
  newVersion: string
}> {
  const updates: Array<{
    pluginId: string
    scope: string
    oldVersion: string
    newVersion: string
  }> = []

  const memoryState = getInMemoryInstalledPlugins()
  const diskState = loadInstalledPluginsFromDisk()

  for (const [pluginId, diskInstallations] of Object.entries(
    diskState.plugins,
  )) {
    const memoryInstallations = memoryState.plugins[pluginId]
    if (!memoryInstallations) continue

    for (const diskEntry of diskInstallations) {
      const memoryEntry = memoryInstallations.find(
        (m) =>
          m.scope === diskEntry.scope &&
          m.projectPath === diskEntry.projectPath,
      )
      if (memoryEntry && memoryEntry.installPath !== diskEntry.installPath) {
        updates.push({
          pluginId,
          scope: diskEntry.scope,
          oldVersion: memoryEntry.version || 'unknown',
          newVersion: diskEntry.version || 'unknown',
        })
      }
    }
  }

  return updates
}

/**
 * Reset the in-memory session state.
 * This should only be called at startup or for testing.
 */
export function resetInMemoryState(): void {
  inMemoryInstalledPlugins = null
}

/**
 * Repair registry records poisoned by the home-as-project aliasing bug.
 *
 * When the CLI started with cwd = home, project/local settings resolved
 * to the same physical file as user settings, and the startup migration
 * (before its physical dedupe + Step 4a removal) re-attributed user-scope
 * installations to `scope: project|local, projectPath: <home>`. The home
 * directory is never a legitimate project permission domain, so such records
 * are ill-formed by construction and are repaired in place:
 *
 * - a user-scope installation for the same plugin already exists → the
 *   poisoned record is redundant and is DROPPED;
 * - no user-scope installation exists → the record is CONVERTED to
 *   `scope: user` (projectPath removed), preserving the install bytes.
 *
 * Before mutating anything, the pre-repair snapshot is written to
 * `<plugins dir>/installed_plugins.repair-<YYYYMMDD>.bak` (skipped when a
 * same-day backup already exists — a same-day rerun means the pre-state was
 * already captured). Idempotent: a clean registry returns all zeros with no
 * disk write.
 */
export function repairHomeProjectScopeRecords(): {
  repaired: number
  dropped: number
  converted: number
} {
  const resolvedHome = resolve(homedir())

  const isPoisoned = (entry: PluginInstallationEntry): boolean =>
    (entry.scope === 'project' || entry.scope === 'local') &&
    entry.projectPath !== undefined &&
    pathsEqual(resolve(entry.projectPath), resolvedHome)

  return mutateInstalledPluginsV2((data) => {
    const hasPoison = Object.values(data.plugins).some((installations) =>
      installations.some(isPoisoned),
    )
    if (!hasPoison) {
      return { changed: false, result: { repaired: 0, dropped: 0, converted: 0 } }
    }

    // Capture the pre-repair snapshot BEFORE mutating, while the registry
    // lock is held. A failed backup write aborts the repair (mutation throws
    // → no registry write); the next startup retries both.
    const fs = getFsImplementation()
    const dateStamp = new Date().toISOString().slice(0, 10).replace(/-/g, '')
    const backupPath = join(
      getPluginsDirectory(),
      `installed_plugins.repair-${dateStamp}.bak`,
    )
    if (!fs.existsSync(backupPath)) {
      fs.mkdirSync(getPluginsDirectory())
      writeFileSyncAtomicNoFallback(
        backupPath,
        `${jsonStringify(data, null, 2)}\n`,
        { encoding: 'utf-8', mode: 0o600 },
      )
      logForDebugging(
        `Wrote pre-repair installed plugins backup to ${backupPath}`,
      )
    }

    const now = new Date().toISOString()
    let dropped = 0
    let converted = 0

    for (const [pluginId, installations] of Object.entries(data.plugins)) {
      const kept: PluginInstallationEntry[] = []
      // Tracks user-scope presence dynamically so the second poisoned record
      // of the same plugin (project + local both aliased to home) is dropped
      // once the first one converted to user scope.
      let hasUserScope = installations.some((entry) => entry.scope === 'user')

      for (const entry of installations) {
        if (!isPoisoned(entry)) {
          kept.push(entry)
          continue
        }
        if (hasUserScope) {
          dropped++
          logForDebugging(
            `Repaired poisoned plugin record: ${pluginId} ${entry.scope} ${entry.projectPath} → dropped (user-scope installation already exists)`,
          )
          continue
        }
        const originalScope = entry.scope
        const originalProjectPath = entry.projectPath
        entry.scope = 'user'
        delete entry.projectPath
        entry.lastUpdated = now
        hasUserScope = true
        converted++
        kept.push(entry)
        logForDebugging(
          `Repaired poisoned plugin record: ${pluginId} ${originalScope} ${originalProjectPath} → converted to user scope`,
        )
      }

      if (kept.length === 0) {
        delete data.plugins[pluginId]
      } else {
        data.plugins[pluginId] = kept
      }
    }

    const repaired = dropped + converted
    return {
      changed: repaired > 0,
      result: { repaired, dropped, converted },
    }
  })
}

/**
 * Minimum number of home-bound records sharing one millisecond-identical
 * `installedAt` before that timestamp is treated as a machine batch write.
 *
 * A human installing plugins one by one produces timestamps seconds apart; a
 * seeding loop stamps its whole catalog from a single `new Date()`.
 *
 * The one path that could plausibly collide is a multi-plugin install, so it is
 * worth being explicit: it cannot. Batch installs stamp each record from its own
 * `getCurrentTimestamp()` during per-item preparation
 * (`pluginInstallationHelpers.ts::preparePluginInstallation`), and each
 * preparation copies plugin bytes to disk. Ten of those completing inside one
 * millisecond is not reachable. Ten is therefore far above any real burst and
 * far below the catalog sizes the removed seeder produced.
 */
const SEEDED_BATCH_MIN_RECORDS = 10

/**
 * Reclaim installation records left behind by the removed
 * `seedOfficialPluginsDisabled` auto-seeder, plus their `enabledPlugins: false`
 * settings keys.
 *
 * Background (`officialMarketplaceStartupCheck.ts`, "NOTE: seedOfficialPlugins-
 * Disabled ... was removed"): the seeder wrote the entire official catalog into
 * `userSettings.enabledPlugins` as `false`, the startup migration read every
 * seeded key as an install intent and materialized it, and — when the runtime ran
 * with cwd = home — attributed all of it to `scope: project, projectPath:
 * <home>`. The producer is gone, but no migration ever reclaimed what it wrote,
 * so affected machines keep a registry where the large majority of entries are
 * plugins the user never chose. They read as installed-but-disabled at home and
 * as absent everywhere else, which is what makes the plugin surface look broken.
 *
 * Why this is deterministic, not heuristic — three conjunctive conditions, each
 * of which a genuine install fails:
 *
 * 1. the record is `project`/`local` scope bound to the home directory (home is
 *    never a legitimate project permission domain — same ill-formed shape
 *    `repairHomeProjectScopeRecords` keys off);
 * 2. the plugin has NO `user`-scope record (a user record proves a real install;
 *    such plugins are left entirely alone here and the existing repair drops
 *    their redundant home record);
 * 3. the record's `installedAt` is shared to the millisecond by at least
 *    {@link SEEDED_BATCH_MIN_RECORDS} home-bound records — the batch-write
 *    fingerprint no sequence of human clicks can produce.
 *
 * Ordering requirement: this MUST run before `repairHomeProjectScopeRecords`.
 * That repair converts a home-bound record with no user-scope sibling INTO a
 * user-scope record — i.e. it would promote every seeded zombie to a first-class
 * install and permanently erase condition 1, making this reclamation a no-op
 * forever after.
 *
 * Settings keys are removed only where the value is exactly `false`. A `true`
 * value is an explicit user enable and is never touched — nor is the record of
 * a plugin holding one, because condition 2 already excluded it.
 *
 * Enable state is not otherwise altered: reclaiming a fake record must not
 * decide, on the user's behalf, that anything should now be on.
 *
 * Idempotent: once reclaimed, no record satisfies condition 1, so later runs
 * return zeros without a disk write. A machine that never ran the seeder has no
 * batch fingerprint at all and is a no-op from the first run.
 *
 * @returns `reclaimed` — plugin ids from which a residue record was removed.
 * Removal is per-record: a plugin that also holds a real install in some other
 * project keeps that record and stays in the registry, and still appears here.
 */
export function reclaimSeededPluginResidue(): {
  reclaimed: string[]
  settingsKeysRemoved: number
} {
  const resolvedHome = resolve(homedir())

  const isHomeBound = (entry: PluginInstallationEntry): boolean =>
    (entry.scope === 'project' || entry.scope === 'local') &&
    entry.projectPath !== undefined &&
    pathsEqual(resolve(entry.projectPath), resolvedHome)

  const reclaimed = mutateInstalledPluginsV2((data) => {
    // Fingerprint pass — count home-bound records per exact `installedAt`.
    // Restricting the census to home-bound records keeps a hypothetical
    // scripted bulk install inside a real project from ever registering as a
    // seeding batch.
    const stampCounts = new Map<string, number>()
    for (const installations of Object.values(data.plugins)) {
      for (const entry of installations) {
        if (!isHomeBound(entry)) continue
        const stamp = entry.installedAt
        if (stamp === undefined) continue
        stampCounts.set(stamp, (stampCounts.get(stamp) ?? 0) + 1)
      }
    }
    const batchStamps = new Set(
      [...stampCounts.entries()]
        .filter(([, count]) => count >= SEEDED_BATCH_MIN_RECORDS)
        .map(([stamp]) => stamp),
    )
    if (batchStamps.size === 0) {
      return { changed: false, result: [] as string[] }
    }

    const isSeededResidue = (entry: PluginInstallationEntry): boolean =>
      isHomeBound(entry) &&
      entry.installedAt !== undefined &&
      batchStamps.has(entry.installedAt)

    const doomed: string[] = []
    for (const [pluginId, installations] of Object.entries(data.plugins)) {
      // A user-scope record proves a real install — leave the whole plugin to
      // `repairHomeProjectScopeRecords`, which drops its redundant home record.
      if (installations.some((entry) => entry.scope === 'user')) continue
      if (!installations.some(isSeededResidue)) continue
      doomed.push(pluginId)
    }
    if (doomed.length === 0) {
      return { changed: false, result: [] as string[] }
    }

    // Snapshot before mutating, while the registry lock is held. A failed
    // backup write aborts the reclamation (throw → no registry write) and the
    // next startup retries both. The filename differs from the repair backup so
    // a same-day run of both keeps two distinct pre-states.
    const fs = getFsImplementation()
    const dateStamp = new Date().toISOString().slice(0, 10).replace(/-/g, '')
    const backupPath = join(
      getPluginsDirectory(),
      `installed_plugins.seed-reclaim-${dateStamp}.bak`,
    )
    if (!fs.existsSync(backupPath)) {
      fs.mkdirSync(getPluginsDirectory())
      writeFileSyncAtomicNoFallback(
        backupPath,
        `${jsonStringify(data, null, 2)}\n`,
        { encoding: 'utf-8', mode: 0o600 },
      )
      logForDebugging(
        `Wrote pre-reclaim installed plugins backup to ${backupPath}`,
      )
    }

    for (const pluginId of doomed) {
      // Keep any record that is not seeded residue — a plugin can hold a real
      // install in some other project alongside the seeded home record.
      const kept = data.plugins[pluginId]!.filter(
        (entry) => !isSeededResidue(entry),
      )
      if (kept.length === 0) {
        delete data.plugins[pluginId]
      } else {
        data.plugins[pluginId] = kept
      }
    }

    return { changed: true, result: doomed }
  })

  if (reclaimed.length === 0) {
    return { reclaimed: [], settingsKeysRemoved: 0 }
  }

  // The seeder wrote its keys to userSettings; that is the only source swept.
  let settingsKeysRemoved = 0
  try {
    const userSettings = getSettingsForSource('userSettings')
    const currentEnabled = userSettings?.enabledPlugins
    if (currentEnabled) {
      const next: Record<string, boolean | string[] | undefined> = {
        ...currentEnabled,
      }
      for (const pluginId of reclaimed) {
        if (currentEnabled[pluginId] !== false) continue
        // undefined signals key deletion via mergeWith in
        // updateSettingsForSource (matches the uninstall path).
        next[pluginId] = undefined
        settingsKeysRemoved++
      }
      if (settingsKeysRemoved > 0) {
        updateSettingsForSource('userSettings', { enabledPlugins: next })
      }
    }
  } catch (error) {
    // Registry reclamation already succeeded and is the user-visible half. A
    // surviving `false` key with no install record is inert — it is exactly the
    // shape of "known but disabled", which never materializes.
    settingsKeysRemoved = 0
    logError(toError(error))
  }

  logForDebugging(
    `Reclaimed ${reclaimed.length} seeded plugin record(s) and ${settingsKeysRemoved} disabled settings key(s)`,
  )
  logEvent('tengu_plugin_seed_residue_reclaimed', {
    records: reclaimed.length,
    settings_keys: settingsKeysRemoved,
  })

  return { reclaimed, settingsKeysRemoved }
}

/**
 * Run every registry-hygiene migration, in the one order that is correct.
 *
 * Ordering is a correctness constraint, not a preference:
 * `reclaimSeededPluginResidue` matches on home-bound records that have no
 * user-scope sibling, and `repairHomeProjectScopeRecords` converts exactly that
 * shape into a user-scope record. Repair-first would promote every seeded zombie
 * to a first-class install and leave nothing to reclaim, permanently. Keeping
 * both calls behind this single function is what stops the two entry points
 * (`initializeVersionedPlugins` and runtime bootstrap) from drifting apart.
 *
 * Both passes read and write only `installed_plugins.json` (plus, for the
 * reclamation, the disabled `enabledPlugins` keys it is retracting). Neither
 * materializes plugin bytes — that stays in `migrateFromEnabledPlugins`, which
 * does real install work and therefore is NOT part of registry hygiene.
 *
 * Each pass is individually fail-soft: a throw is logged and the next one still
 * runs, because a startup path must never be blocked by a self-heal.
 */
export function runInstalledPluginsRegistryHygiene(): void {
  try {
    reclaimSeededPluginResidue()
  } catch (error) {
    logError(error)
  }

  try {
    repairHomeProjectScopeRecords()
  } catch (error) {
    logError(error)
  }
}

/**
 * Initialize the versioned plugins system.
 * This triggers V1→V2 migration and initializes the in-memory session state.
 *
 * This should be called early during startup in all modes (REPL and headless).
 *
 * @returns Promise that resolves when initialization is complete
 */
export async function initializeVersionedPlugins(): Promise<void> {
  // Step 1: Migrate to single file format (consolidates V1/V2 files, cleans up legacy cache)
  migrateToSinglePluginFile()

  // Step 1.5: Reclaim seeded residue, then self-heal poisoned
  // `projectPath = <home>` records — BEFORE the enabledPlugins sync, so scope
  // matching below sees a repaired registry. Failures must not block startup.
  runInstalledPluginsRegistryHygiene()

  // Step 2: Sync enabledPlugins from settings.json to installed_plugins.json
  // This must complete before CLI exits (especially in headless mode)
  try {
    await migrateFromEnabledPlugins()
  } catch (error) {
    logError(error)
  }

  // Step 3: Initialize in-memory session state
  // Calling getInMemoryInstalledPlugins triggers:
  // 1. Loading from disk
  // 2. Caching in inMemoryInstalledPlugins for session state
  const data = getInMemoryInstalledPlugins()
  logForDebugging(
    `Initialized versioned plugins system with ${Object.keys(data.plugins).length} plugins`,
  )
}

export const MARKETPLACE_REMOVE_BLOCKED_INSTALLED_PLUGINS =
  'MARKETPLACE_REMOVE_BLOCKED_INSTALLED_PLUGINS'
export const MARKETPLACE_REMOVE_INSTALLED_REGISTRY_INVALID =
  'MARKETPLACE_REMOVE_INSTALLED_REGISTRY_INVALID'

export type MarketplaceRemovalGuardCode =
  | typeof MARKETPLACE_REMOVE_BLOCKED_INSTALLED_PLUGINS
  | typeof MARKETPLACE_REMOVE_INSTALLED_REGISTRY_INVALID

/**
 * Stable, classifiable failure raised before marketplace removal mutates any
 * registry, cache, settings, options, or data. The symbolic code is also part
 * of the message because JSON-RPC currently preserves only the error string.
 */
export class MarketplaceRemovalGuardError extends Error {
  readonly code: MarketplaceRemovalGuardCode

  constructor(code: MarketplaceRemovalGuardCode, message: string) {
    super(`[${code}] ${message}`)
    this.name = 'MarketplaceRemovalGuardError'
    this.code = code
  }
}

/**
 * Strictly enumerate installed plugin IDs belonging to one marketplace.
 *
 * This is a guard read, not an inventory read: it bypasses the fail-soft cache,
 * takes the concrete installed_plugins.json transaction lock, and rejects a
 * malformed/unreadable registry. Marketplace matching goes through the
 * canonical identifier parser; suffix matching would incorrectly treat names
 * such as `demo-extra` as `demo` and is forbidden here.
 */
export function listInstalledPluginIdsForMarketplaceStrict(
  marketplaceName: string,
): string[] {
  return withInstalledPluginsLock(() => {
    const { data } = loadInstalledPluginsFromDiskStrict()
    return Object.entries(data.plugins)
      .filter(
        ([pluginId, installations]) =>
          installations.length > 0 &&
          parsePluginIdentifier(pluginId).marketplace === marketplaceName,
      )
      .map(([pluginId]) => pluginId)
      .sort()
  })
}

/** Fail closed unless the marketplace has zero installations in every scope. */
export function assertNoInstalledPluginsForMarketplaceStrict(
  marketplaceName: string,
): void {
  let installedPluginIds: string[]
  try {
    installedPluginIds =
      listInstalledPluginIdsForMarketplaceStrict(marketplaceName)
  } catch (error) {
    throw new MarketplaceRemovalGuardError(
      MARKETPLACE_REMOVE_INSTALLED_REGISTRY_INVALID,
      `Cannot remove marketplace '${marketplaceName}' because installed_plugins.json could not be validated: ${errorMessage(error)}`,
    )
  }

  if (installedPluginIds.length > 0) {
    throw new MarketplaceRemovalGuardError(
      MARKETPLACE_REMOVE_BLOCKED_INSTALLED_PLUGINS,
      `Cannot remove marketplace '${marketplaceName}' while ${installedPluginIds.length} installed plugin entr${installedPluginIds.length === 1 ? 'y remains' : 'ies remain'}; uninstall every scope first`,
    )
  }
}

/**
 * Predicate: is this installation relevant to the current project context?
 *
 * V2 installed_plugins.json may contain project-scoped entries from OTHER
 * projects (a single user-level file tracks all scopes). Callers asking
 * "is this plugin installed" almost always mean "installed in a way that's
 * active here" — not "installed anywhere on this machine". See #29608:
 * DiscoverPlugins.tsx was hiding plugins that were only installed in an
 * unrelated project.
 *
 * - user/managed scopes: always relevant (global)
 * - project/local scopes: only if projectPath matches the current project
 *
 * getOriginalCwd() (not getCwd()) because "current project" is where CrabCode
 * Code was launched from, not wherever the working directory has drifted to.
 */
export function isInstallationRelevantToCurrentProject(
  inst: PluginInstallationEntry,
): boolean {
  return (
    inst.scope === 'user' ||
    inst.scope === 'managed' ||
    (inst.projectPath !== undefined &&
      pathsEqual(inst.projectPath, getOriginalCwd()))
  )
}

/**
 * Check if a plugin is installed in a way relevant to the current project.
 *
 * @param pluginId - Plugin ID in "plugin@marketplace" format
 * @returns True if the plugin has a user/managed-scoped installation, OR a
 *   project/local-scoped installation whose projectPath matches the current
 *   project. Returns false for plugins only installed in other projects.
 */
export function isPluginInstalled(pluginId: string): boolean {
  const v2Data = loadInstalledPluginsV2()
  const installations = v2Data.plugins[pluginId]
  if (!installations || installations.length === 0) {
    return false
  }
  if (!installations.some(isInstallationRelevantToCurrentProject)) {
    return false
  }
  // Plugins are loaded from settings.enabledPlugins
  // If settings.enabledPlugins and installed_plugins.json diverge
  // (via settings.json clobber), return false
  return getSettings_DEPRECATED().enabledPlugins?.[pluginId] !== undefined
}

/**
 * True only if the plugin has a USER or MANAGED scope installation.
 *
 * Use this in UI flows that decide whether to offer installation at all.
 * A user/managed-scope install means the plugin is available everywhere —
 * there's nothing the user can add. A project/local-scope install means the
 * user might still want to install at user scope to make it global.
 *
 * gh-29997 / gh-29240 / gh-29392: the browse UI was blocking on
 * isPluginInstalled() which returns true for project-scope installs,
 * preventing users from adding a user-scope entry for the same plugin.
 * The backend (installPluginOp → addInstalledPlugin) already supports
 * multiple scope entries per plugin — only the UI gate was wrong.
 *
 * @param pluginId - Plugin ID in "plugin@marketplace" format
 */
export function isPluginGloballyInstalled(pluginId: string): boolean {
  const v2Data = loadInstalledPluginsV2()
  const installations = v2Data.plugins[pluginId]
  if (!installations || installations.length === 0) {
    return false
  }
  const hasGlobalEntry = installations.some(
    (entry) => entry.scope === 'user' || entry.scope === 'managed',
  )
  if (!hasGlobalEntry) return false
  // Same settings divergence guard as isPluginInstalled — if enabledPlugins
  // was clobbered, treat as not-installed so the user can re-enable.
  return getSettings_DEPRECATED().enabledPlugins?.[pluginId] !== undefined
}

/**
 * Add or update a plugin's installation metadata
 *
 * Implements double-write: updates both V1 and V2 files.
 *
 * @param pluginId - Plugin ID in "plugin@marketplace" format
 * @param metadata - Installation metadata
 * @param scope - Installation scope (defaults to 'user' for backward compatibility)
 * @param projectPath - Project path (for project/local scopes)
 */
export function addInstalledPlugin(
  pluginId: string,
  metadata: InstalledPlugin,
  scope: PersistableScope = 'user',
  projectPath?: string,
): void {
  mutateInstalledPluginsV2((v2Data) => {
    const v2Entry: PluginInstallationEntry = {
      scope,
      installPath: metadata.installPath,
      version: metadata.version,
      installedAt: metadata.installedAt,
      lastUpdated: metadata.lastUpdated,
      gitCommitSha: metadata.gitCommitSha,
      ...(projectPath && { projectPath }),
    }

    // Get or create array for this plugin (preserves other scope installations)
    const installations = v2Data.plugins[pluginId] || []

    // Find existing entry for this scope+projectPath
    const existingIndex = installations.findIndex(
      (entry) => entry.scope === scope && entry.projectPath === projectPath,
    )

    const isUpdate = existingIndex >= 0
    if (isUpdate) {
      installations[existingIndex] = v2Entry
    } else {
      installations.push(v2Entry)
    }

    v2Data.plugins[pluginId] = installations
    logForDebugging(
      `${isUpdate ? 'Updated' : 'Added'} installed plugin: ${pluginId} (scope: ${scope})`,
    )
    return { changed: true, result: undefined }
  })
}

/**
 * Remove a plugin from the installed plugins registry
 * This should be called when a plugin is uninstalled.
 *
 * Note: This function only updates the registry file. To fully uninstall,
 * call deletePluginCache() afterward to remove the physical files.
 *
 * @param pluginId - Plugin ID in "plugin@marketplace" format
 * @returns The removed plugin metadata, or undefined if it wasn't installed
 */
export function removeInstalledPlugin(
  pluginId: string,
): InstalledPlugin | undefined {
  return mutateInstalledPluginsV2((v2Data) => {
    const installations = v2Data.plugins[pluginId]

    if (!installations || installations.length === 0) {
      return { changed: false, result: undefined }
    }

    // Extract V1-compatible metadata from first installation for return value
    const firstInstall = installations[0]
    const metadata: InstalledPlugin | undefined = firstInstall
      ? {
          version: firstInstall.version || 'unknown',
          installedAt: firstInstall.installedAt || new Date().toISOString(),
          lastUpdated: firstInstall.lastUpdated,
          installPath: firstInstall.installPath,
          gitCommitSha: firstInstall.gitCommitSha,
        }
      : undefined

    delete v2Data.plugins[pluginId]
    logForDebugging(`Removed installed plugin: ${pluginId}`)
    return { changed: true, result: metadata }
  })
}

/**
 * Delete a plugin's cache directory
 * This physically removes the plugin files from disk
 *
 * @param installPath - Absolute path to the plugin's cache directory
 */
export function deletePluginCache(installPath: string): void {
  const fs = getFsImplementation()

  try {
    const lexicalCacheRoot = resolve(getPluginCachePath())
    const canonicalCacheRoot = fs.realpathSync(lexicalCacheRoot)
    const lexicalInstallPath = resolve(installPath)
    let scanRoot = lexicalCacheRoot
    let scanRelative = relative(lexicalCacheRoot, lexicalInstallPath)
    if (
      scanRelative === '..' ||
      scanRelative.startsWith(`..${sep}`) ||
      isAbsolute(scanRelative)
    ) {
      scanRoot = canonicalCacheRoot
      scanRelative = relative(canonicalCacheRoot, lexicalInstallPath)
    }
    if (
      scanRelative === '' ||
      scanRelative === '..' ||
      scanRelative.startsWith(`..${sep}`) ||
      isAbsolute(scanRelative)
    ) {
      throw new Error(`Refusing plugin cache delete outside a version path`)
    }
    let current = scanRoot
    for (const segment of scanRelative.split(sep)) {
      current = join(current, segment)
      if (fs.lstatSync(current).isSymbolicLink()) {
        throw new Error(
          `Refusing plugin cache delete through symlink: ${current}`,
        )
      }
    }
    const safeInstallPath = fs.realpathSync(lexicalInstallPath)
    const canonicalRelative = relative(canonicalCacheRoot, safeInstallPath)
    if (
      canonicalRelative === '' ||
      canonicalRelative === '..' ||
      canonicalRelative.startsWith(`..${sep}`) ||
      isAbsolute(canonicalRelative)
    ) {
      throw new Error(`Refusing plugin cache delete outside cache root`)
    }
    fs.rmSync(safeInstallPath, { recursive: true, force: true })
    logForDebugging(`Deleted plugin cache at ${safeInstallPath}`)

    // Clean up empty parent plugin directory (cache/{marketplace}/{plugin})
    // Versioned paths have structure: cache/{marketplace}/{plugin}/{version}
    if (
      safeInstallPath.includes(`${sep}cache${sep}`) &&
      safeInstallPath.startsWith(canonicalCacheRoot + sep)
    ) {
      const pluginDir = dirname(safeInstallPath) // e.g., cache/{marketplace}/{plugin}
      if (
        pluginDir !== canonicalCacheRoot &&
        pluginDir.startsWith(canonicalCacheRoot + sep)
      ) {
        try {
          const contents = fs.readdirSync(pluginDir)
          if (contents.length === 0) {
            fs.rmdirSync(pluginDir)
            logForDebugging(`Deleted empty plugin directory at ${pluginDir}`)
          }
        } catch {
          // Parent dir doesn't exist or isn't readable — skip cleanup
        }
      }
    }
  } catch (error) {
    const errorMsg = errorMessage(error)
    logError(toError(error))
    throw new Error(
      `Failed to delete plugin cache at ${installPath}: ${errorMsg}`,
    )
  }
}

/**
 * Sync installed_plugins.json with enabledPlugins from settings
 *
 * Checks the schema version and only updates if:
 * - File doesn't exist (version 0 → current)
 * - Schema version is outdated (old version → current)
 * - New plugins appear in enabledPlugins
 *
 * This version-based approach makes it easy to add new fields in the future:
 * 1. Increment CURRENT_SCHEMA_VERSION
 * 2. Add migration logic for the new version
 * 3. File is automatically updated on next startup
 *
 * For each plugin in enabledPlugins that's not in installed_plugins.json, the
 * exact marketplace snapshot is resolved outside locks and materialized through
 * the same coordinated generation-scoped install transaction as explicit
 * installs. Migration never invents a path or points the registry at mutable
 * marketplace/legacy cache bytes.
 *
 * Only an explicit enable intent (`true`, or an array of version constraints)
 * is migrated. A `false` key means "known but disabled" — it never enters the
 * materialization flow; enabling it later materializes on demand
 * (setPluginEnabledOp). The enabled/disabled state remains in settings.json.
 */
export async function migrateFromEnabledPlugins(): Promise<void> {
  // Use merged settings for shouldSkipSync check
  const settings = getSettings_DEPRECATED()
  const enabledPlugins = settings.enabledPlugins || {}

  // No plugins in settings = nothing to sync
  if (Object.keys(enabledPlugins).length === 0) {
    return
  }

  // Check if main file exists and has V2 format
  const rawFileData = readInstalledPluginsFileRaw()
  const fileExists = rawFileData !== null
  const initialData = rawFileData
    ? rawFileData.version === 2
      ? InstalledPluginsFileSchemaV2().parse(rawFileData.data)
      : migrateV1ToV2(InstalledPluginsFileSchemaV1().parse(rawFileData.data))
    : { version: 2 as const, plugins: {} }

  logForDebugging(
    fileExists
      ? 'Syncing installed_plugins.json with enabledPlugins from all settings.json files'
      : 'Creating installed_plugins.json from settings.json files',
  )

  // Scope identity is pinned to the startup project. Mid-session cwd/worktree
  // changes must not migrate settings ownership to a different repository.
  const projectPath = getOriginalCwd()

  // Step 1: Build a map of pluginId -> scope from all settings.json files
  // Settings.json is the source of truth for scope
  const pluginScopeFromSettings = new Map<
    string,
    {
      scope: 'user' | 'project' | 'local'
      projectPath: string | undefined
    }
  >()

  // Iterate through each editable settings source (order matters: user first)
  const settingSources: EditableSettingSource[] = [
    'userSettings',
    'projectSettings',
    'localSettings',
  ]

  // Physical-identity dedupe: when the CLI starts with cwd = home, the
  // project (or local) settings file resolves to the SAME physical file as
  // user settings (~/.crabcode/settings.json). Interpreting that one file
  // under both sources let last-wins re-attribute every user-scope plugin to
  // `scope: project, projectPath: <home>`. One physical file is interpreted
  // exactly once — as user settings.
  const userSettingsFilePath = getSettingsFilePathForSource('userSettings')

  for (const source of settingSources) {
    if (source !== 'userSettings' && userSettingsFilePath !== undefined) {
      const sourceFilePath = getSettingsFilePathForSource(source)
      if (
        sourceFilePath !== undefined &&
        pathsEqual(resolve(sourceFilePath), resolve(userSettingsFilePath))
      ) {
        logForDebugging(
          `Skipping ${source} during enabledPlugins migration: resolves to the same physical file as userSettings (${sourceFilePath})`,
        )
        continue
      }
    }

    const sourceSettings = getSettingsForSource(source)
    const sourceEnabledPlugins = sourceSettings?.enabledPlugins || {}

    for (const pluginId of Object.keys(sourceEnabledPlugins)) {
      // Skip non-standard plugin IDs
      if (!pluginId.includes('@')) continue

      // Only explicit enable intent migrates. `true` and version-constraint
      // arrays match getEnabledPluginIdsForScope's enable predicate; `false`
      // ("known but disabled") must never materialize an installation.
      const value = sourceEnabledPlugins[pluginId]
      if (!(value === true || Array.isArray(value))) continue

      // Settings.json is source of truth - always update scope
      // Use the most specific scope (last one wins: local > project > user)
      const scope = settingSourceToScope(source)
      pluginScopeFromSettings.set(pluginId, {
        scope,
        projectPath: scope === 'user' ? undefined : projectPath,
      })
    }
  }

  // Step 2: Resolve the desired changes from a strict, atomic disk snapshot.
  // The expensive marketplace/Git lookups stay outside the transaction lock;
  // Step 4 re-reads current disk state under lock before applying them.
  const resolvedMissingPlugins = new Map<
    string,
    {
      scope: 'user' | 'project' | 'local'
      entry: PluginMarketplaceEntry
      marketplaceInstallLocation: string
      marketplaceGenerationId: string
      marketplaceContentDigest: string
    }
  >()

  // Step 3: Update V2 scopes based on settings.json (settings is source of truth)
  for (const [pluginId, scopeInfo] of pluginScopeFromSettings) {
    const existingInstallations = initialData.plugins[pluginId]

    const matchingInstallation = existingInstallations?.find(
      (installation) =>
        installation.scope === scopeInfo.scope &&
        (scopeInfo.projectPath === undefined
          ? installation.projectPath === undefined
          : installation.projectPath !== undefined &&
            pathsEqual(installation.projectPath, scopeInfo.projectPath)),
    )
    const currentInstallation =
      matchingInstallation ?? existingInstallations?.[0]

    if (
      currentInstallation &&
      (await isInstalledPluginCachePathUsable(currentInstallation.installPath))
    ) {
      // Scope reconciliation happens against the fresh locked snapshot below.
      // Do not retain this old entry as an add candidate: if it disappears
      // before commit, a concurrent uninstall/removal must win.
      continue
    } else {
      // Missing membership, an untrusted legacy source path, or missing cache
      // bytes all recover through the same coordinated install transaction.
      const { name: pluginName, marketplace } = parsePluginIdentifier(pluginId)

      if (!pluginName || !marketplace) {
        continue
      }

      try {
        logForDebugging(
          `Looking up plugin ${pluginId} in marketplace ${marketplace}`,
        )
        const pluginInfo = await getPluginById(pluginId)
        if (!pluginInfo) {
          logForDebugging(
            `Plugin ${pluginId} not found in any marketplace, skipping`,
          )
          continue
        }

        resolvedMissingPlugins.set(pluginId, {
          scope: scopeInfo.scope,
          entry: pluginInfo.entry,
          marketplaceInstallLocation: pluginInfo.marketplaceInstallLocation,
          marketplaceGenerationId: pluginInfo.marketplaceGenerationId,
          marketplaceContentDigest: pluginInfo.marketplaceContentDigest,
        })

        logForDebugging(`Resolved ${pluginId} with scope ${scopeInfo.scope}`)
      } catch (error) {
        logForDebugging(`Failed to add plugin ${pluginId}: ${error}`)
      }
    }
  }

  // NOTE: The former "Step 4a" (settings→registry scope rewrite of the FIRST
  // installation entry) was removed. A one-directional silent scope rewrite
  // has no legitimate scenario: it destroyed valid records whenever the entry
  // at index 0 was not the one the settings key referred to, and it is how
  // home-aliased project keys converted healthy user-scope records into
  // `scope: project, projectPath: <home>` poison. Scope changes converge by
  // materializing an installation AT the new scope (Step 4b below).

  // Step 4b: Every empty→installed transition uses the complete production
  // installation transaction: prepare unpublished bytes, validate the exact
  // marketplace generation, publish a unique cache generation, then commit the
  // installed registry and settings sibling. The dynamic import avoids a
  // static module cycle (the helper itself owns installed-registry primitives).
  let addedCount = 0
  const installResolvedPlugin =
    resolvedMissingPlugins.size > 0
      ? (await import('./pluginInstallationHelpers.js')).installResolvedPlugin
      : null
  for (const [pluginId, resolved] of resolvedMissingPlugins) {
    if (!installResolvedPlugin) break
    try {
      const result = await installResolvedPlugin({
        pluginId,
        entry: resolved.entry,
        scope: resolved.scope,
        marketplaceInstallLocation: resolved.marketplaceInstallLocation,
        marketplaceGenerationId: resolved.marketplaceGenerationId,
        marketplaceContentDigest: resolved.marketplaceContentDigest,
      })
      if (result.ok) {
        addedCount++
        logForDebugging(`Materialized ${pluginId} through coordinated install`)
      } else {
        logForDebugging(
          `Skipped startup installation ${pluginId}: ${result.reason}`,
          { level: 'warn' },
        )
      }
    } catch (error) {
      logForDebugging(
        `Failed coordinated startup installation ${pluginId}: ${errorMessage(error)}`,
        { level: 'warn' },
      )
    }
  }

  if (addedCount > 0) {
    logForDebugging(
      `Sync completed: ${addedCount} added in installed_plugins.json`,
    )
  }
}
