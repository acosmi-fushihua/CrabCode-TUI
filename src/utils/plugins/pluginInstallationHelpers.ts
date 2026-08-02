/** Coordinated plugin installation helpers. */

import { randomUUID } from 'crypto'
import { rm, rmdir } from 'fs/promises'
import { dirname, join } from 'path'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_PII_TAGGED,
  logEvent,
} from '../../services/analytics/index.js'
import { getCwd } from '../cwd.js'
import { errorMessage, isENOENT, toError } from '../errors.js'
import { getFsImplementation } from '../fsOperations.js'
import { logError } from '../log.js'
import type { EditableSettingSource } from '../settings/constants.js'
import { updateSettingsForSource } from '../settings/settings.js'
import { buildPluginTelemetryFields } from '../telemetry/pluginTelemetry.js'
import { markPluginVersionOrphaned } from './cacheUtils.js'
import {
  formatDependencyCountSuffix,
  getEnabledPluginIdsForScope,
  type ResolutionResult,
  resolveDependencyClosure,
} from './dependencyResolver.js'
import {
  commitInstalledPluginsRegistryWithSibling,
  isInstalledPluginCachePathUsable,
  INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED,
  INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED,
  InstalledPluginsSiblingCommitError,
  loadInstalledPluginsFromDisk,
} from './installedPluginsManager.js'
import { getManagedPluginNames } from './managedPlugins.js'
import {
  commitMarketplacePluginInstallations,
  getMarketplaceCacheOnly,
  getPluginById,
  type MarketplacePluginInstallationExpectation,
  type ResolvedMarketplacePlugin,
} from './marketplaceManager.js'
import {
  isOfficialMarketplaceName,
  parsePluginIdentifier,
  scopeToSettingSource,
} from './pluginIdentifier.js'
import { cachePlugin, getVersionedCachePath } from './pluginLoader.js'
import { getPluginsDirectory } from './pluginDirectories.js'
import { bumpPluginLifecycleGeneration } from './pluginLifecycle.js'
import {
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from './pluginPathSecurity.js'
import { isPluginBlockedByPolicy } from './pluginPolicy.js'
import {
  calculatePluginVersion,
  getGitCommitSha,
} from './pluginVersioning.js'
import {
  isLocalPluginSource,
  type InstalledPluginsFileV2,
  type PluginInstallationEntry,
  type PluginMarketplaceEntry,
  type PluginScope,
  type PluginSource,
} from './schemas.js'
import {
  convertDirectoryToZipInPlace,
  isPluginZipCacheEnabled,
} from './zipCache.js'

export type PluginInstallationInfo = {
  pluginId: string
  installPath: string
  version?: string
}

export function getCurrentTimestamp(): string {
  return new Date().toISOString()
}

type PreparedPluginInstallation = {
  expectation: MarketplacePluginInstallationExpectation
  scope: PluginScope
  projectPath?: string
  stagingPath: string
  finalPath: string
  version: string
  installedAt: string
  gitCommitSha?: string
  published: boolean
  expectedInstallPath?: string
}

type InstalledDraftCommitResult = {
  orphanedPaths: string[]
}

async function removeOwnedPath(path: string): Promise<void> {
  try {
    await rm(path, { recursive: true, force: true })
  } catch (error) {
    logError(toError(error))
  }
}

async function cleanupPreparedInstallations(
  prepared: readonly PreparedPluginInstallation[],
  removePublished: boolean,
): Promise<void> {
  const owned = new Set<string>()
  const published = new Set<string>()
  for (const item of prepared) {
    owned.add(item.stagingPath)
    if (removePublished && item.published) {
      owned.add(item.finalPath)
      published.add(item.finalPath)
    }
  }
  await Promise.all([...owned].map(removeOwnedPath))
  await Promise.all(
    [...published].map(async path => {
      // The generation directory is shared only by unique children. Remove it
      // when this failed transaction owned the last child; ENOTEMPTY means a
      // concurrent committed installation still owns that generation.
      try {
        await rmdir(dirname(path))
      } catch {}
    }),
  )
}

async function resolveLocalMarketplacePluginSource(
  snapshot: ResolvedMarketplacePlugin,
): Promise<string> {
  const marketplaceStats = await getFsImplementation().stat(
    snapshot.marketplaceInstallLocation,
  )
  const marketplaceRoot = marketplaceStats.isDirectory()
    ? snapshot.marketplaceInstallLocation
    : dirname(snapshot.marketplaceInstallLocation)
  return resolvePluginComponentPath(
    marketplaceRoot,
    snapshot.entry.source as string,
    { component: 'local marketplace plugin source' },
  )
}

function upsertInstalledPluginDraft(
  draft: InstalledPluginsFileV2,
  prepared: Pick<
    PreparedPluginInstallation,
    | 'expectation'
    | 'scope'
    | 'projectPath'
    | 'finalPath'
    | 'version'
    | 'installedAt'
    | 'gitCommitSha'
    | 'expectedInstallPath'
  >,
): string | undefined {
  const pluginId = prepared.expectation.pluginId
  const installations = draft.plugins[pluginId] ?? []
  const existingIndex = installations.findIndex(
    (candidate) =>
      candidate.scope === prepared.scope &&
      candidate.projectPath === prepared.projectPath,
  )
  if (
    prepared.expectedInstallPath !== undefined &&
    (existingIndex < 0 ||
      installations[existingIndex]?.installPath !==
        prepared.expectedInstallPath)
  ) {
    throw new Error(
      `Installed plugin ${pluginId} changed while its update was being prepared`,
    )
  }
  const previousPath =
    existingIndex >= 0 ? installations[existingIndex]?.installPath : undefined
  const next: PluginInstallationEntry = {
    scope: prepared.scope,
    installPath: prepared.finalPath,
    version: prepared.version,
    installedAt: prepared.installedAt,
    lastUpdated: prepared.installedAt,
    gitCommitSha: prepared.gitCommitSha,
    ...(prepared.projectPath && { projectPath: prepared.projectPath }),
  }
  if (existingIndex >= 0) installations[existingIndex] = next
  else installations.push(next)
  draft.plugins[pluginId] = installations
  return previousPath
}

async function preparePluginInstallation(
  snapshot: ResolvedMarketplacePlugin,
  pluginId: string,
  scope: PluginScope,
  projectPath?: string,
  localSourcePath?: string,
): Promise<PreparedPluginInstallation> {
  const source: PluginSource =
    typeof snapshot.entry.source === 'string' && localSourcePath
      ? (localSourcePath as PluginSource)
      : snapshot.entry.source
  let stagingDirectory: string | undefined
  let stagingZip: string | undefined
  try {
    const cacheResult = await cachePlugin(source, {
      manifest: snapshot.entry,
      preserveTemporaryPath: true,
    })
    stagingDirectory = cacheResult.path
    const pathForGitSha = localSourcePath ?? stagingDirectory
    const gitCommitSha =
      cacheResult.gitCommitSha ??
      (await getGitCommitSha(pathForGitSha)) ??
      undefined
    const version = await calculatePluginVersion(
      pluginId,
      snapshot.entry.source,
      cacheResult.manifest,
      pathForGitSha,
      snapshot.entry.version,
      cacheResult.gitCommitSha,
    )

    const pluginsRoot = getPluginsDirectory()
    const fs = getFsImplementation()
    await fs.mkdir(pluginsRoot)
    const cacheRoot = await resolvePluginComponentPath(pluginsRoot, 'cache', {
      mustExist: false,
      rejectSymlinks: true,
      component: 'plugin cache root',
    })
    await fs.mkdir(cacheRoot)
    stagingDirectory = await resolveInternalPluginPath(
      cacheRoot,
      stagingDirectory,
      {
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'unpublished plugin cache generation',
      },
    )

    const generationRoot = join(
      getVersionedCachePath(pluginId, version),
      `marketplace-${snapshot.marketplaceGenerationId}`,
    )
    const uniqueDestination = join(generationRoot, randomUUID())
    let stagingPath = stagingDirectory
    let finalCandidate = uniqueDestination
    if (isPluginZipCacheEnabled()) {
      stagingZip = `${stagingDirectory}.${randomUUID()}.zip`
      stagingZip = await resolveInternalPluginPath(cacheRoot, stagingZip, {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'unpublished plugin ZIP generation',
      })
      await convertDirectoryToZipInPlace(stagingDirectory, stagingZip)
      stagingPath = stagingZip
      finalCandidate = `${uniqueDestination}.zip`
    }
    const finalPath = await resolveInternalPluginPath(
      cacheRoot,
      finalCandidate,
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'generation-scoped plugin cache destination',
      },
    )
    return {
      expectation: {
        pluginId,
        entry: snapshot.entry,
        marketplaceInstallLocation: snapshot.marketplaceInstallLocation,
        marketplaceGenerationId: snapshot.marketplaceGenerationId,
        marketplaceContentDigest: snapshot.marketplaceContentDigest,
      },
      scope,
      projectPath,
      stagingPath,
      finalPath,
      version,
      installedAt: getCurrentTimestamp(),
      gitCommitSha,
      published: false,
    }
  } catch (error) {
    await Promise.all(
      [stagingDirectory, stagingZip]
        .filter((path): path is string => !!path)
        .map(removeOwnedPath),
    )
    throw error
  }
}

async function publishPreparedInstallation(
  prepared: PreparedPluginInstallation,
): Promise<void> {
  const fs = getFsImplementation()
  await fs.mkdir(dirname(prepared.finalPath))
  try {
    await fs.stat(prepared.finalPath)
    throw new Error(
      `Refusing to overwrite existing plugin cache generation at ${prepared.finalPath}`,
    )
  } catch (error) {
    if (!isENOENT(error)) throw error
  }
  await fs.rename(prepared.stagingPath, prepared.finalPath)
  prepared.published = true
}

async function commitPreparedPluginInstallations(
  prepared: readonly PreparedPluginInstallation[],
  settingSource?: EditableSettingSource,
  options: {
    validationExpectations?: readonly MarketplacePluginInstallationExpectation[]
    enabledPluginIds?: readonly string[]
    requiredInstallations?: readonly {
      pluginId: string
      scope: PluginScope
      projectPath?: string
      installPath: string
    }[]
  } = {},
): Promise<void> {
  let commitResult: InstalledDraftCommitResult | undefined
  const expectationByPluginId = new Map<
    string,
    MarketplacePluginInstallationExpectation
  >()
  for (const expectation of options.validationExpectations ?? []) {
    expectationByPluginId.set(expectation.pluginId, expectation)
  }
  for (const item of prepared) {
    expectationByPluginId.set(item.expectation.pluginId, item.expectation)
  }
  const enabledPluginIds =
    options.enabledPluginIds ??
    prepared.map((item) => item.expectation.pluginId)
  try {
    await commitMarketplacePluginInstallations(
      [...expectationByPluginId.values()],
      async () => {
        for (const item of prepared) await publishPreparedInstallation(item)

        commitResult = commitInstalledPluginsRegistryWithSibling(
          (draft) => {
            for (const expected of options.requiredInstallations ?? []) {
              const stillCurrent = draft.plugins[expected.pluginId]?.some(
                (candidate) =>
                  candidate.scope === expected.scope &&
                  candidate.projectPath === expected.projectPath &&
                  candidate.installPath === expected.installPath,
              )
              if (!stillCurrent) {
                throw new Error(
                  `Installed plugin ${expected.pluginId} changed while its batch was being prepared`,
                )
              }
            }
            const replacedPaths = new Set<string>()
            for (const item of prepared) {
              const replaced = upsertInstalledPluginDraft(draft, item)
              if (replaced && replaced !== item.finalPath) {
                replacedPaths.add(replaced)
              }
            }
            const referencedPaths = new Set(
              Object.values(draft.plugins)
                .flat()
                .map((installation) => installation.installPath),
            )
            return {
              changed: prepared.length > 0,
              result: {
                orphanedPaths: [...replacedPaths].filter(
                  (path) => !referencedPaths.has(path),
                ),
              },
            }
          },
          () => {
            if (!settingSource || enabledPluginIds.length === 0) return
            const enabledPlugins: Record<string, true> = {}
            for (const pluginId of enabledPluginIds) {
              enabledPlugins[pluginId] = true
            }
            const result = updateSettingsForSource(settingSource, {
              enabledPlugins,
            })
            if (result.error) throw result.error
          },
        )
      },
    )
  } catch (error) {
    const registryStateUncertain =
      error instanceof InstalledPluginsSiblingCommitError &&
      error.code === INSTALLED_PLUGINS_SIBLING_ROLLBACK_FAILED
    await cleanupPreparedInstallations(prepared, !registryStateUncertain)
    throw error
  }

  await cleanupPreparedInstallations(prepared, false)
  for (const path of commitResult?.orphanedPaths ?? []) {
    await markPluginVersionOrphaned(path)
  }
}

/** Prepare, generation-validate, publish, and register one plugin. */
export async function cacheAndRegisterPlugin(
  pluginId: string,
  entry: PluginMarketplaceEntry,
  scope: PluginScope,
  projectPath: string | undefined,
  marketplaceInstallLocation: string,
  marketplaceGenerationId: string,
  marketplaceContentDigest: string,
  localSourcePath?: string,
): Promise<string> {
  const prepared = await preparePluginInstallation(
    {
      entry,
      marketplaceInstallLocation,
      marketplaceGenerationId,
      marketplaceContentDigest,
    },
    pluginId,
    scope,
    projectPath,
    localSourcePath,
  )
  await commitPreparedPluginInstallations([prepared])
  return prepared.finalPath
}

export type CoordinatedPluginUpdateResult = {
  version: string
  installPath: string
  alreadyUpToDate: boolean
}

/**
 * Prepare an update from one coherent marketplace snapshot and replace only
 * the exact still-current installed scope. No deterministic cache path is
 * reused, and a concurrent uninstall/reinstall makes the locked draft guard
 * fail instead of resurrecting or overwriting it.
 */
export async function updateResolvedPluginInstallation({
  pluginId,
  snapshot,
  scope,
  projectPath,
  expectedInstallPath,
  expectedVersion,
}: {
  pluginId: string
  snapshot: ResolvedMarketplacePlugin
  scope: PluginScope
  projectPath?: string
  expectedInstallPath: string
  expectedVersion?: string
}): Promise<CoordinatedPluginUpdateResult> {
  let localSourcePath: string | undefined
  if (isLocalPluginSource(snapshot.entry.source)) {
    localSourcePath = await resolveLocalMarketplacePluginSource(snapshot)
  }
  const prepared = await preparePluginInstallation(
    snapshot,
    pluginId,
    scope,
    projectPath,
    localSourcePath,
  )
  const currentGenerationSegment = `marketplace-${snapshot.marketplaceGenerationId}`
  if (
    expectedVersion === prepared.version &&
    expectedInstallPath.split(/[\\/]/u).includes(currentGenerationSegment) &&
    (await isInstalledPluginCachePathUsable(expectedInstallPath))
  ) {
    try {
      // A no-write result is still a claim about authoritative marketplace
      // state. Re-enter the same locked generation/digest validation used by
      // real commits so a concurrent refresh cannot turn a stale snapshot into
      // a false "already up to date" success.
      await commitMarketplacePluginInstallations(
        [prepared.expectation],
        () => {},
      )
    } finally {
      await cleanupPreparedInstallations([prepared], true)
    }
    return {
      version: prepared.version,
      installPath: expectedInstallPath,
      alreadyUpToDate: true,
    }
  }

  prepared.expectedInstallPath = expectedInstallPath
  await commitPreparedPluginInstallations([prepared])
  return {
    version: prepared.version,
    installPath: prepared.finalPath,
    alreadyUpToDate: false,
  }
}

/** Register a pre-existing path after exact marketplace generation validation. */
export async function registerPluginInstallation(
  info: PluginInstallationInfo,
  scope: PluginScope,
  projectPath: string | undefined,
  marketplaceEntry: PluginMarketplaceEntry,
  marketplaceInstallLocation: string,
  marketplaceGenerationId: string,
  marketplaceContentDigest: string,
): Promise<void> {
  const expectation: MarketplacePluginInstallationExpectation = {
    pluginId: info.pluginId,
    entry: marketplaceEntry,
    marketplaceInstallLocation,
    marketplaceGenerationId,
    marketplaceContentDigest,
  }
  await commitMarketplacePluginInstallations([expectation], () => {
    const now = getCurrentTimestamp()
    commitInstalledPluginsRegistryWithSibling(
      (draft) => {
        upsertInstalledPluginDraft(draft, {
          expectation,
          scope,
          projectPath,
          finalPath: info.installPath,
          version: info.version ?? 'unknown',
          installedAt: now,
        })
        return { changed: true, result: undefined }
      },
      () => {},
    )
  })
}

export function parsePluginId(
  pluginId: string,
): { name: string; marketplace: string } | null {
  const parts = pluginId.split('@')
  if (parts.length !== 2 || !parts[0] || !parts[1]) return null
  return { name: parts[0], marketplace: parts[1] }
}

export type InstallCoreResult =
  | { ok: true; closure: string[]; depNote: string }
  | { ok: false; reason: 'local-source-no-location'; pluginName: string }
  | { ok: false; reason: 'settings-write-failed'; message: string }
  | {
      ok: false
      reason: 'resolution-failed'
      resolution: ResolutionResult & { ok: false }
    }
  | { ok: false; reason: 'blocked-by-policy'; pluginName: string }
  | {
      ok: false
      reason: 'dependency-blocked-by-policy'
      pluginName: string
      blockedDependency: string
    }

export function formatResolutionError(
  resolution: ResolutionResult & { ok: false },
): string {
  switch (resolution.reason) {
    case 'cycle':
      return `Dependency cycle: ${resolution.chain.join(' → ')}`
    case 'cross-marketplace': {
      const marketplace = parsePluginIdentifier(
        resolution.dependency,
      ).marketplace
      const where = marketplace
        ? `marketplace "${marketplace}"`
        : 'a different marketplace'
      const hint = marketplace
        ? ` Add "${marketplace}" to allowCrossMarketplaceDependenciesOn in the ROOT marketplace's marketplace.json (the marketplace of the plugin you're installing — only its allowlist applies; no transitive trust).`
        : ''
      return `Dependency "${resolution.dependency}" (required by ${resolution.requiredBy}) is in ${where}, which is not in the allowlist — cross-marketplace dependencies are blocked by default. Install it manually first.${hint}`
    }
    case 'not-found': {
      const marketplace = parsePluginIdentifier(resolution.missing).marketplace
      return marketplace
        ? `Dependency "${resolution.missing}" (required by ${resolution.requiredBy}) not found. Is the "${marketplace}" marketplace added?`
        : `Dependency "${resolution.missing}" (required by ${resolution.requiredBy}) not found in any configured marketplace`
    }
  }
}

export type ResolvedPluginBatchRoot = {
  pluginId: string
  snapshot: ResolvedMarketplacePlugin
}

export type CoordinatedPluginBatchInstallResult =
  | { ok: true; closure: string[]; changedPluginIds: string[] }
  | { ok: false; failures: { pluginId: string; message: string }[] }

/**
 * Install several roots as one coordinated transaction.
 *
 * Every root independently validates dependency trust/policy against the
 * pre-batch enabled set. Their closures are then de-duplicated, fully staged,
 * and passed to the existing multi-expectation commit primitive exactly once.
 * No cache generation is published and neither registry/settings file is
 * changed unless every root and dependency can be prepared.
 */
export async function installResolvedPluginBatch({
  roots,
  scope,
}: {
  roots: readonly ResolvedPluginBatchRoot[]
  scope: 'user' | 'project' | 'local'
}): Promise<CoordinatedPluginBatchInstallResult> {
  if (roots.length === 0) {
    return { ok: true, closure: [], changedPluginIds: [] }
  }

  const settingSource = scopeToSettingSource(scope)
  const alreadyEnabled = getEnabledPluginIdsForScope(settingSource)
  const snapshots = new Map<string, ResolvedMarketplacePlugin>()
  for (const root of roots) {
    if (snapshots.has(root.pluginId)) {
      return {
        ok: false,
        failures: [
          {
            pluginId: root.pluginId,
            message: `Marketplace catalog contains duplicate plugin id ${root.pluginId}`,
          },
        ],
      }
    }
    snapshots.set(root.pluginId, root.snapshot)
  }

  const closure: string[] = []
  const closureSet = new Set<string>()
  for (const root of roots) {
    if (isPluginBlockedByPolicy(root.pluginId)) {
      return {
        ok: false,
        failures: [
          {
            pluginId: root.pluginId,
            message: `Plugin "${root.snapshot.entry.name}" is blocked by your organization's policy and cannot be installed`,
          },
        ],
      }
    }

    const rootMarketplace = parsePluginIdentifier(root.pluginId).marketplace
    const allowedCrossMarketplaces = new Set(
      (rootMarketplace
        ? (await getMarketplaceCacheOnly(rootMarketplace))
            ?.allowCrossMarketplaceDependenciesOn
        : undefined) ?? [],
    )
    const resolution = await resolveDependencyClosure(
      root.pluginId,
      async (pluginId) => {
        const cached = snapshots.get(pluginId)
        if (cached) return cached.entry
        const found = await getPluginById(pluginId)
        if (found) snapshots.set(pluginId, found)
        return found?.entry ?? null
      },
      alreadyEnabled,
      allowedCrossMarketplaces,
    )
    if (!resolution.ok) {
      return {
        ok: false,
        failures: [
          {
            pluginId: root.pluginId,
            message: formatResolutionError(resolution),
          },
        ],
      }
    }

    for (const pluginId of resolution.closure) {
      if (pluginId !== root.pluginId && isPluginBlockedByPolicy(pluginId)) {
        return {
          ok: false,
          failures: [
            {
              pluginId: root.pluginId,
              message: `Plugin "${root.snapshot.entry.name}" depends on "${pluginId}", which is blocked by your organization's policy`,
            },
          ],
        }
      }
      if (!closureSet.has(pluginId)) {
        closureSet.add(pluginId)
        closure.push(pluginId)
      }
    }
  }

  const projectPath = scope === 'user' ? undefined : getCwd()
  const installed = loadInstalledPluginsFromDisk()
  const prepared: PreparedPluginInstallation[] = []
  const requiredInstallations: Array<{
    pluginId: string
    scope: PluginScope
    projectPath?: string
    installPath: string
  }> = []
  for (const pluginId of closure) {
    const snapshot = snapshots.get(pluginId)
    if (!snapshot) {
      await cleanupPreparedInstallations(prepared, true)
      return {
        ok: false,
        failures: [
          {
            pluginId,
            message: `Plugin ${pluginId} lost its marketplace generation during batch preparation`,
          },
        ],
      }
    }

    const exactInstallation = installed.plugins[pluginId]?.find(
      (candidate) =>
        candidate.scope === scope && candidate.projectPath === projectPath,
    )
    if (
      exactInstallation &&
      (await isInstalledPluginCachePathUsable(exactInstallation.installPath))
    ) {
      requiredInstallations.push({
        pluginId,
        scope,
        projectPath,
        installPath: exactInstallation.installPath,
      })
      continue
    }
    try {
      const localSourcePath = isLocalPluginSource(snapshot.entry.source)
        ? await resolveLocalMarketplacePluginSource(snapshot)
        : undefined
      prepared.push(
        await preparePluginInstallation(
          snapshot,
          pluginId,
          scope,
          projectPath,
          localSourcePath,
        ),
      )
    } catch (error) {
      await cleanupPreparedInstallations(prepared, true)
      return {
        ok: false,
        failures: [
          {
            pluginId,
            message: `Failed to prepare atomic batch: ${errorMessage(error)}`,
          },
        ],
      }
    }
  }

  const enabledPluginIds = closure.filter(
    (pluginId) => !alreadyEnabled.has(pluginId),
  )
  try {
    await commitPreparedPluginInstallations(prepared, settingSource, {
      validationExpectations: closure.map((pluginId) => {
        const snapshot = snapshots.get(pluginId)!
        return {
          pluginId,
          entry: snapshot.entry,
          marketplaceInstallLocation: snapshot.marketplaceInstallLocation,
          marketplaceGenerationId: snapshot.marketplaceGenerationId,
          marketplaceContentDigest: snapshot.marketplaceContentDigest,
        }
      }),
      enabledPluginIds,
      requiredInstallations,
    })
  } catch (error) {
    const message =
      error instanceof InstalledPluginsSiblingCommitError &&
      error.code === INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED
        ? `Failed to update settings: ${errorMessage(error.siblingError)}`
        : `Atomic batch commit failed: ${errorMessage(error)}`
    return {
      ok: false,
      failures: [{ pluginId: roots[0]!.pluginId, message }],
    }
  }

  // 前台生命周期事件：原 clearAllCaches() 替换为 bump——其内部已做
  // clearAllCaches + 快照重建，并让其它进程在下一安全边界收敛。此处事务
  // （commitPreparedPluginInstallations）已返回、registry 锁已释放。
  bumpPluginLifecycleGeneration()
  return {
    ok: true,
    closure,
    changedPluginIds: [
      ...new Set([
        ...prepared.map((item) => item.expectation.pluginId),
        ...enabledPluginIds,
      ]),
    ],
  }
}

/**
 * Resolve and prepare the full closure without locks, then perform one
 * generation-validated cache publication + installed/settings transaction.
 */
export async function installResolvedPlugin({
  pluginId,
  entry,
  scope,
  marketplaceInstallLocation,
  marketplaceGenerationId,
  marketplaceContentDigest,
}: {
  pluginId: string
  entry: PluginMarketplaceEntry
  scope: 'user' | 'project' | 'local'
  marketplaceInstallLocation?: string
  marketplaceGenerationId?: string
  marketplaceContentDigest?: string
}): Promise<InstallCoreResult> {
  const settingSource = scopeToSettingSource(scope)
  if (isPluginBlockedByPolicy(pluginId)) {
    return { ok: false, reason: 'blocked-by-policy', pluginName: entry.name }
  }

  let rootSnapshot: ResolvedMarketplacePlugin | null = null
  if (
    marketplaceInstallLocation &&
    marketplaceGenerationId &&
    marketplaceContentDigest
  ) {
    rootSnapshot = {
      entry,
      marketplaceInstallLocation,
      marketplaceGenerationId,
      marketplaceContentDigest,
    }
  } else {
    rootSnapshot = await getPluginById(pluginId)
  }
  if (!rootSnapshot) {
    if (isLocalPluginSource(entry.source)) {
      return {
        ok: false,
        reason: 'local-source-no-location',
        pluginName: entry.name,
      }
    }
    throw new Error(`No authoritative marketplace generation for ${pluginId}`)
  }

  const depInfo = new Map<string, ResolvedMarketplacePlugin>([
    [pluginId, rootSnapshot],
  ])
  const rootMarketplace = parsePluginIdentifier(pluginId).marketplace
  const allowedCrossMarketplaces = new Set(
    (rootMarketplace
      ? (await getMarketplaceCacheOnly(rootMarketplace))
          ?.allowCrossMarketplaceDependenciesOn
      : undefined) ?? [],
  )
  const resolution = await resolveDependencyClosure(
    pluginId,
    async (id) => {
      const cached = depInfo.get(id)
      if (cached) return cached.entry
      const found = await getPluginById(id)
      if (found) depInfo.set(id, found)
      return found?.entry ?? null
    },
    getEnabledPluginIdsForScope(settingSource),
    allowedCrossMarketplaces,
  )
  if (!resolution.ok) {
    return { ok: false, reason: 'resolution-failed', resolution }
  }

  for (const id of resolution.closure) {
    if (id !== pluginId && isPluginBlockedByPolicy(id)) {
      return {
        ok: false,
        reason: 'dependency-blocked-by-policy',
        pluginName: entry.name,
        blockedDependency: id,
      }
    }
  }

  const projectPath = scope === 'user' ? undefined : getCwd()
  const prepared: PreparedPluginInstallation[] = []
  try {
    for (const id of resolution.closure) {
      const snapshot = depInfo.get(id)
      if (!snapshot) {
        throw new Error(`Dependency ${id} lost its marketplace generation`)
      }
      let localSourcePath: string | undefined
      if (isLocalPluginSource(snapshot.entry.source)) {
        localSourcePath = await resolveLocalMarketplacePluginSource(snapshot)
      }
      prepared.push(
        await preparePluginInstallation(
          snapshot,
          id,
          scope,
          projectPath,
          localSourcePath,
        ),
      )
    }
  } catch (error) {
    await cleanupPreparedInstallations(prepared, true)
    throw error
  }

  try {
    await commitPreparedPluginInstallations(prepared, settingSource)
  } catch (error) {
    if (
      error instanceof InstalledPluginsSiblingCommitError &&
      error.code === INSTALLED_PLUGINS_SIBLING_COMMIT_FAILED
    ) {
      return {
        ok: false,
        reason: 'settings-write-failed',
        message: errorMessage(error.siblingError),
      }
    }
    throw error
  }

  // 前台生命周期事件：原 clearAllCaches() 替换为 bump——其内部已做
  // clearAllCaches + 快照重建，并让其它进程在下一安全边界收敛。此处事务
  // （commitPreparedPluginInstallations）已返回、registry 锁已释放。
  bumpPluginLifecycleGeneration()
  const depNote = formatDependencyCountSuffix(
    resolution.closure.filter((id) => id !== pluginId),
  )
  return { ok: true, closure: resolution.closure, depNote }
}

export type InstallPluginResult =
  { success: true; message: string } | { success: false; error: string }

export type InstallPluginParams = {
  pluginId: string
  entry: PluginMarketplaceEntry
  marketplaceName: string
  scope?: 'user' | 'project' | 'local'
  trigger?: 'hint' | 'user'
}

export async function installPluginFromMarketplace({
  pluginId,
  entry,
  marketplaceName,
  scope = 'user',
  trigger = 'user',
}: InstallPluginParams): Promise<InstallPluginResult> {
  try {
    const pluginInfo = await getPluginById(pluginId)
    const result = await installResolvedPlugin({
      pluginId,
      entry: pluginInfo?.entry ?? entry,
      scope,
      marketplaceInstallLocation: pluginInfo?.marketplaceInstallLocation,
      marketplaceGenerationId: pluginInfo?.marketplaceGenerationId,
      marketplaceContentDigest: pluginInfo?.marketplaceContentDigest,
    })

    if (!result.ok) {
      switch (result.reason) {
        case 'local-source-no-location':
          return {
            success: false,
            error: `Cannot install local plugin "${result.pluginName}" without marketplace install location`,
          }
        case 'settings-write-failed':
          return {
            success: false,
            error: `Failed to update settings: ${result.message}`,
          }
        case 'resolution-failed':
          return {
            success: false,
            error: formatResolutionError(result.resolution),
          }
        case 'blocked-by-policy':
          return {
            success: false,
            error: `Plugin "${result.pluginName}" is blocked by your organization's policy and cannot be installed`,
          }
        case 'dependency-blocked-by-policy':
          return {
            success: false,
            error: `Cannot install "${result.pluginName}": dependency "${result.blockedDependency}" is blocked by your organization's policy`,
          }
      }
    }

    logEvent('tengu_plugin_installed', {
      _PROTO_plugin_name:
        entry.name as AnalyticsMetadata_I_VERIFIED_THIS_IS_PII_TAGGED,
      _PROTO_marketplace_name:
        marketplaceName as AnalyticsMetadata_I_VERIFIED_THIS_IS_PII_TAGGED,
      plugin_id: (isOfficialMarketplaceName(marketplaceName)
        ? pluginId
        : 'third-party') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      trigger:
        trigger as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      install_source: (trigger === 'hint'
        ? 'ui-suggestion'
        : 'ui-discover') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      ...buildPluginTelemetryFields(
        entry.name,
        marketplaceName,
        getManagedPluginNames(),
      ),
      ...(entry.version && {
        version:
          entry.version as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      }),
    })
    return {
      success: true,
      message: `✓ Installed ${entry.name}${result.depNote}. Run /reload-plugins to activate.`,
    }
  } catch (error) {
    logError(toError(error))
    return {
      success: false,
      error: `Failed to install: ${errorMessage(error)}`,
    }
  }
}
