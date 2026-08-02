import { readdir, rm, stat, unlink } from 'fs/promises'
import { isAbsolute, join, relative, sep } from 'path'
import { clearAllOutputStylesCache } from '../../constants/outputStyles.js'
import { clearAgentDefinitionsCache } from '../../tools/AgentTool/loadAgentsDir.js'
import { clearPromptCache } from '../../tools/SkillTool/prompt.js'
import { resetSentSkillNames } from '../attachments.js'
import { logForDebugging } from '../debug.js'
import { getErrnoCode } from '../errors.js'
import { logError } from '../log.js'
import { loadInstalledPluginsFromDisk } from './installedPluginsManager.js'
import { clearPluginAgentCache } from './loadPluginAgents.js'
import { clearPluginCommandCache } from './loadPluginCommands.js'
import {
  clearPluginHookCache,
  pruneRemovedPluginHooks,
} from './loadPluginHooks.js'
import { clearPluginOutputStyleCache } from './loadPluginOutputStyles.js'
import { clearPluginCache } from './pluginLoader.js'
import { clearPluginOptionsCache } from './pluginOptionsStorage.js'
import { getPluginsDirectory } from './pluginDirectories.js'
import {
  resolveInternalPluginPath,
  resolvePluginComponentPath,
  writeCanonicalPluginFile,
} from './pluginPathSecurity.js'
import { isPluginZipCacheEnabled } from './zipCache.js'
import { clearActiveSurfaceCommandCaches } from '../commandCacheInvalidation.js'

const ORPHANED_AT_FILENAME = '.orphaned_at'
const CLEANUP_AGE_MS = 7 * 24 * 60 * 60 * 1000 // 7 days

async function resolveWritableCachedVersionPath(
  versionPath: string,
): Promise<string> {
  const cacheRoot = await resolvePluginComponentPath(
    getPluginsDirectory(),
    'cache',
    { rejectSymlinks: true, component: 'plugin cache root' },
  )
  return resolveInternalPluginPath(cacheRoot, versionPath, {
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'plugin cache version',
  })
}

export function clearAllPluginCaches(): void {
  clearPluginCache()
  clearPluginCommandCache()
  clearPluginAgentCache()
  clearPluginHookCache()
  // Prune hooks from plugins no longer in the enabled set so uninstalled/
  // disabled plugins stop firing immediately (gh-36995). Prune-only: hooks
  // from newly-enabled plugins are NOT added here — they wait for
  // /reload-plugins like commands/agents/MCP do. Fire-and-forget: old hooks
  // stay valid until the prune completes (preserves gh-29767). No-op when
  // STATE.registeredHooks is empty (test/preload.ts beforeEach clears it via
  // resetStateForTests before reaching here).
  pruneRemovedPluginHooks().catch(e => logError(e))
  clearPluginOptionsCache()
  clearPluginOutputStyleCache()
  clearAllOutputStylesCache()
}

export function clearAllCaches(): void {
  clearAllPluginCaches()
  clearActiveSurfaceCommandCaches()
  clearAgentDefinitionsCache()
  clearPromptCache()
  resetSentSkillNames()
}

/**
 * Mark a plugin version as orphaned.
 * Called when a plugin is uninstalled or updated to a new version.
 */
export async function markPluginVersionOrphaned(
  versionPath: string,
): Promise<void> {
  try {
    const safeVersionPath = await resolveWritableCachedVersionPath(versionPath)
    const markerPath = await resolveInternalPluginPath(
      safeVersionPath,
      getOrphanedAtPath(safeVersionPath),
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'plugin orphan marker',
      },
    )
    await writeCanonicalPluginFile(
      safeVersionPath,
      markerPath,
      `${Date.now()}`,
      'plugin orphan marker',
    )
  } catch (error) {
    logForDebugging(
      `Refused or failed to write .orphaned_at: ${versionPath}: ${error}`,
    )
  }
}

/**
 * Clean up orphaned plugin versions that have been orphaned for more than 7 days.
 *
 * Pass 1: Remove .orphaned_at from installed versions AND from version roots
 *   protected by a generation-deep installPath (clears stale markers,
 *   self-heals roots mis-marked by the pre-fix exact-compare algorithm)
 * Pass 2: For each cached version not in installed_plugins.json and without
 *   any active installPath at or below it:
 *   - If no .orphaned_at exists: create it (handles old CC versions, manual edits)
 *   - If .orphaned_at exists and > 7 days old: delete the version
 */
export async function cleanupOrphanedPluginVersionsInBackground(): Promise<void> {
  // Zip cache mode stores plugins as .zip files, not directories. readSubdirs
  // filters to directories only, so removeIfEmpty would see plugin dirs as empty
  // and delete them (including the ZIPs). Skip cleanup entirely in zip mode.
  if (isPluginZipCacheEnabled()) {
    return
  }
  try {
    const installedVersions = getInstalledVersionPaths()
    if (!installedVersions) return

    const cachePath = await resolvePluginComponentPath(
      getPluginsDirectory(),
      'cache',
      { rejectSymlinks: true, component: 'plugin cache root' },
    )

    const now = Date.now()

    // Protection set: v2 registry entries carry generation-deep install
    // paths (cache/<mkt>/<plugin>/<version>/marketplace-<gen>/<uuid>),
    // while this GC walks version roots (cache/<mkt>/<plugin>/<version>).
    // A version root with any active installPath at or below it must never
    // be treated as an orphan — the exact-compare Set.has alone used to
    // mis-mark active roots and rm -rf them (live generation included)
    // after 7 days. Legacy entries whose installPath IS the version root
    // map to themselves, so legacy behavior does not regress.
    const protectedVersionRoots = getProtectedVersionRoots(
      installedVersions,
      cachePath,
    )

    // Pass 1: Remove .orphaned_at from installed versions and from their
    // protected version roots. This handles cases where a plugin was
    // reinstalled after being orphaned, and self-heals version roots that
    // the pre-fix algorithm already mis-marked in the field (e.g.
    // <plugin>/<version>/.orphaned_at next to a live generation dir).
    // removeOrphanedAtMarker resolves through
    // resolveWritableCachedVersionPath, which accepts version roots and
    // deeper install paths alike.
    await Promise.all(
      [...new Set([...installedVersions, ...protectedVersionRoots])].map(p =>
        removeOrphanedAtMarker(p),
      ),
    )

    // Pass 2: Process orphaned versions
    for (const marketplace of await readSubdirs(cachePath)) {
      const marketplacePath = join(cachePath, marketplace)

      for (const plugin of await readSubdirs(marketplacePath)) {
        const pluginPath = join(marketplacePath, plugin)

        for (const version of await readSubdirs(pluginPath)) {
          const versionPath = join(pluginPath, version)
          // Skip when exactly installed (legacy installPath == version
          // root) or when any active installPath lives below this root
          // (v2 generation-deep form).
          if (
            installedVersions.has(versionPath) ||
            protectedVersionRoots.has(versionPath)
          ) {
            continue
          }
          await processOrphanedPluginVersion(versionPath, now)
        }

        await removeIfEmpty(pluginPath)
      }

      await removeIfEmpty(marketplacePath)
    }
  } catch (error) {
    logForDebugging(`Plugin cache cleanup failed: ${error}`)
  }
}

function getOrphanedAtPath(versionPath: string): string {
  return join(versionPath, ORPHANED_AT_FILENAME)
}

async function removeOrphanedAtMarker(versionPath: string): Promise<void> {
  try {
    const safeVersionPath = await resolveWritableCachedVersionPath(versionPath)
    const orphanedAtPath = await resolveInternalPluginPath(
      safeVersionPath,
      getOrphanedAtPath(safeVersionPath),
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'plugin orphan marker',
      },
    )
    await unlink(orphanedAtPath)
  } catch (error) {
    const code = getErrnoCode(error)
    if (code === 'ENOENT') return
    logForDebugging(`Failed to remove .orphaned_at: ${versionPath}: ${error}`)
  }
}

function getInstalledVersionPaths(): Set<string> | null {
  try {
    const paths = new Set<string>()
    const diskData = loadInstalledPluginsFromDisk()
    for (const installations of Object.values(diskData.plugins)) {
      for (const entry of installations) {
        paths.add(entry.installPath)
      }
    }
    return paths
  } catch (error) {
    logForDebugging(`Failed to load installed plugins: ${error}`)
    return null
  }
}

/**
 * Derive the set of version roots (cache/<mkt>/<plugin>/<version>) that are
 * protected by at least one registry installPath equal to them or below them.
 *
 * Containment is a pure string-domain check via path.relative()
 * (`rel && !rel.startsWith('..') && !isAbsolute(rel)`): both sides are
 * same-origin absolute paths — registry installPath strings vs
 * join(cachePath, ...) — matching the comparison domain the exact-match
 * Set.has already uses. No realpath resolution is introduced.
 *
 * An installPath exactly three segments below cachePath IS a version root
 * (legacy form) and maps to itself; deeper paths (v2 generation form,
 * .../marketplace-<gen>/<uuid>) map to their enclosing version root.
 * Paths outside cachePath (or shallower than a version root) contribute
 * nothing — exactly like the pre-existing exact compare.
 */
function getProtectedVersionRoots(
  installedVersions: Set<string>,
  cachePath: string,
): Set<string> {
  const roots = new Set<string>()
  for (const installPath of installedVersions) {
    const rel = relative(cachePath, installPath)
    if (!rel || rel.startsWith('..') || isAbsolute(rel)) continue
    const segments = rel.split(sep).filter(segment => segment.length > 0)
    if (segments.length < 3) continue
    roots.add(join(cachePath, segments[0], segments[1], segments[2]))
  }
  return roots
}

async function processOrphanedPluginVersion(
  versionPath: string,
  now: number,
): Promise<void> {
  let safeVersionPath: string
  try {
    safeVersionPath = await resolveWritableCachedVersionPath(versionPath)
  } catch (error) {
    logForDebugging(`Refused unsafe plugin cache version: ${versionPath}: ${error}`)
    return
  }
  const orphanedAtPath = getOrphanedAtPath(safeVersionPath)

  let orphanedAt: number
  try {
    orphanedAt = (await stat(orphanedAtPath)).mtimeMs
  } catch (error) {
    const code = getErrnoCode(error)
    if (code === 'ENOENT') {
      await markPluginVersionOrphaned(safeVersionPath)
      return
    }
    logForDebugging(`Failed to stat orphaned marker: ${versionPath}: ${error}`)
    return
  }

  if (now - orphanedAt > CLEANUP_AGE_MS) {
    try {
      await rm(safeVersionPath, { recursive: true, force: true })
    } catch (error) {
      logForDebugging(
        `Failed to delete orphaned version: ${versionPath}: ${error}`,
      )
    }
  }
}

async function removeIfEmpty(dirPath: string): Promise<void> {
  let safeDirPath: string
  try {
    const cacheRoot = await resolvePluginComponentPath(
      getPluginsDirectory(),
      'cache',
      { rejectSymlinks: true, component: 'plugin cache root' },
    )
    safeDirPath = await resolveInternalPluginPath(cacheRoot, dirPath, {
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'plugin cache directory',
    })
  } catch (error) {
    logForDebugging(`Refused unsafe plugin cache directory: ${dirPath}: ${error}`)
    return
  }
  if ((await readSubdirs(safeDirPath)).length === 0) {
    try {
      await rm(safeDirPath, { recursive: true, force: true })
    } catch (error) {
      logForDebugging(`Failed to remove empty dir: ${dirPath}: ${error}`)
    }
  }
}

async function readSubdirs(dirPath: string): Promise<string[]> {
  try {
    const entries = await readdir(dirPath, { withFileTypes: true })
    return entries.filter(d => d.isDirectory()).map(d => d.name)
  } catch {
    return []
  }
}
