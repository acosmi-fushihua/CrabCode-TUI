/**
 * Background plugin autoupdate functionality
 *
 * At startup, this module:
 * 1. First updates marketplaces that have autoUpdate enabled
 * 2. Then checks all installed plugins from those marketplaces and updates them
 *
 * Updates are non-inplace (disk-only), requiring a restart to take effect.
 * Official Acosmi marketplaces have autoUpdate enabled by default,
 * but users can disable it per-marketplace.
 */

import { updatePluginOp } from '../../services/plugins/pluginOperations.js'
import { shouldSkipPluginAutoupdate } from '../config.js'
import { logForDebugging } from '../debug.js'
import { errorMessage } from '../errors.js'
import { logError } from '../log.js'
import {
  getPendingUpdatesDetails,
  hasPendingUpdates,
  isInstallationRelevantToCurrentProject,
  loadInstalledPluginsFromDisk,
} from './installedPluginsManager.js'
import {
  getDeclaredMarketplaces,
  loadKnownMarketplacesConfig,
  refreshMarketplace,
  type DeclaredMarketplace,
  type KnownMarketplacesConfig,
} from './marketplaceManager.js'
import { parsePluginIdentifier } from './pluginIdentifier.js'
import { isMarketplaceAutoUpdate, type PluginScope } from './schemas.js'

/**
 * Callback type for notifying when plugins have been updated
 */
export type PluginAutoUpdateCallback = (updatedPlugins: string[]) => void

// Store callback for plugin update notifications
let pluginUpdateCallback: PluginAutoUpdateCallback | null = null

// Store pending updates that occurred before callback was registered
// This handles the race condition where updates complete before REPL mounts
let pendingNotification: string[] | null = null

/**
 * Register a callback to be notified when plugins are auto-updated.
 * This is used by the REPL to show restart notifications.
 *
 * If plugins were already updated before the callback was registered,
 * the callback will be invoked immediately with the pending updates.
 */
export function onPluginsAutoUpdated(
  callback: PluginAutoUpdateCallback,
): () => void {
  pluginUpdateCallback = callback

  // If there are pending updates that happened before registration, deliver them now
  if (pendingNotification !== null && pendingNotification.length > 0) {
    callback(pendingNotification)
    pendingNotification = null
  }

  return () => {
    pluginUpdateCallback = null
  }
}

/**
 * Check if pending updates came from autoupdate (for notification purposes).
 * Returns the list of plugin names that have pending updates.
 */
export function getAutoUpdatedPluginNames(): string[] {
  if (!hasPendingUpdates()) {
    return []
  }
  return getPendingUpdatesDetails().map(
    d => parsePluginIdentifier(d.pluginId).name,
  )
}

/**
 * Get the set of marketplaces that have autoUpdate enabled.
 * Returns the marketplace names that should be auto-updated.
 */
export function selectAutoUpdateEnabledMarketplaceNames(
  config: KnownMarketplacesConfig,
  declared: Record<string, DeclaredMarketplace>,
): Set<string> {
  const enabled = new Set<string>()

  for (const [name, entry] of Object.entries(config)) {
    // Settings-declared autoUpdate takes precedence over JSON state
    const declaredAutoUpdate = declared[name]?.autoUpdate
    const autoUpdate =
      declaredAutoUpdate !== undefined
        ? declaredAutoUpdate
        : isMarketplaceAutoUpdate(name, entry)
    if (autoUpdate) {
      // Registry keys are the canonical identity accepted by the
      // case-sensitive refresh path. Normalize only when matching installed
      // plugin IDs after refresh succeeds.
      enabled.add(name)
    }
  }

  return enabled
}

async function getAutoUpdateEnabledMarketplaces(): Promise<Set<string>> {
  return selectAutoUpdateEnabledMarketplaceNames(
    await loadKnownMarketplacesConfig(),
    getDeclaredMarketplaces(),
  )
}

export type MarketplacePluginUpdateFailure = {
  pluginId: string
  scope: PluginScope
  projectPath?: string
  message: string
}

export type MarketplacePluginUpdateReport = {
  matchedInstallations: number
  updatedPluginIds: string[]
  failures: MarketplacePluginUpdateFailure[]
}

/**
 * Explicit marketplace-update UIs must surface installed-content failures.
 * Catalog commits and successful sibling updates remain durable, but callers
 * must not replace a partial result with a pure-success message.
 */
export function assertMarketplacePluginUpdatesSucceeded(
  report: MarketplacePluginUpdateReport,
): void {
  if (report.failures.length === 0) return

  const details = report.failures
    .map(failure => {
      const scope = failure.projectPath
        ? `${failure.scope}:${failure.projectPath}`
        : failure.scope
      return `${failure.pluginId} [${scope}]: ${failure.message}`
    })
    .join('; ')
  const scopeLabel = report.failures.length === 1 ? 'scope' : 'scopes'
  throw new Error(
    `Marketplace catalog refresh committed, but ${report.failures.length} installed plugin ${scopeLabel} failed to update: ${details}`,
  )
}

type PluginUpdateReport = Omit<
  MarketplacePluginUpdateReport,
  'updatedPluginIds'
> & {
  updated: boolean
}

/** Update every relevant installed scope and retain every failure. */
async function updatePluginWithReport(
  pluginId: string,
  installations: Array<{ scope: PluginScope; projectPath?: string }>,
): Promise<PluginUpdateReport> {
  let wasUpdated = false
  const failures: MarketplacePluginUpdateFailure[] = []

  for (const { scope, projectPath } of installations) {
    try {
      const result = await updatePluginOp(pluginId, scope)

      if (result.success && !result.alreadyUpToDate) {
        wasUpdated = true
        logForDebugging(
          `Plugin autoupdate: updated ${pluginId} from ${result.oldVersion} to ${result.newVersion}`,
        )
      } else if (!result.alreadyUpToDate) {
        failures.push({
          pluginId,
          scope,
          ...(projectPath && { projectPath }),
          message: result.message,
        })
        logForDebugging(
          `Plugin autoupdate: failed to update ${pluginId}: ${result.message}`,
          { level: 'warn' },
        )
      }
    } catch (error) {
      failures.push({
        pluginId,
        scope,
        ...(projectPath && { projectPath }),
        message: errorMessage(error),
      })
      logForDebugging(
        `Plugin autoupdate: error updating ${pluginId}: ${errorMessage(error)}`,
        { level: 'warn' },
      )
    }
  }

  return {
    matchedInstallations: installations.length,
    updated: wasUpdated,
    failures,
  }
}

/**
 * Failure-reporting mode for explicit writes. Unlike background
 * autoupdate, callers can distinguish "nothing installed" from a partially or
 * wholly failed installed-content rewrite and must not report marketplace
 * success while `failures` is non-empty.
 */
export async function updatePluginsForMarketplacesStrict(
  marketplaceNames: Set<string>,
): Promise<MarketplacePluginUpdateReport> {
  const installedPlugins = loadInstalledPluginsFromDisk()
  const work = Object.entries(installedPlugins.plugins).flatMap(
    ([pluginId, installations]) => {
      const { marketplace } = parsePluginIdentifier(pluginId)
      if (!marketplace || !marketplaceNames.has(marketplace.toLowerCase())) {
        return []
      }
      const relevantInstallations = installations.filter(
        isInstallationRelevantToCurrentProject,
      )
      return relevantInstallations.length > 0
        ? [{ pluginId, installations: relevantInstallations }]
        : []
    },
  )
  const reports = await Promise.all(
    work.map(({ pluginId, installations }) =>
      updatePluginWithReport(pluginId, installations),
    ),
  )
  return {
    matchedInstallations: reports.reduce(
      (count, report) => count + report.matchedInstallations,
      0,
    ),
    updatedPluginIds: work.flatMap(({ pluginId }, index) =>
      reports[index]?.updated ? [pluginId] : [],
    ),
    failures: reports.flatMap(report => report.failures),
  }
}

/**
 * Update all project-relevant installed plugins from the given marketplaces.
 *
 * Iterates installed_plugins.json, filters to plugins whose marketplace is in
 * the set, further filters each plugin's installations to those relevant to
 * the current project (user/managed scope, or project/local scope matching
 * cwd — see isInstallationRelevantToCurrentProject), then calls updatePluginOp
 * per installation. Already-up-to-date plugins are silently skipped.
 *
 * Called by:
 * - updatePlugins() below — background autoupdate path (autoUpdate-enabled
 *   marketplaces only; third-party marketplaces default autoUpdate: false)
 * - ManageMarketplaces.tsx applyChanges() — user-initiated /plugin marketplace
 *   update. Before #29512 this path only called refreshMarketplace() (git
 *   pull on the marketplace clone), so the loader would create the new
 *   version cache dir but installed_plugins.json stayed on the old version,
 *   and the orphan GC stamped the NEW dir with .orphaned_at on next startup.
 *
 * @param marketplaceNames - lowercase marketplace names to update plugins from
 * @returns plugin IDs that were actually updated (not already up-to-date)
 */
export async function updatePluginsForMarketplaces(
  marketplaceNames: Set<string>,
): Promise<string[]> {
  const report = await updatePluginsForMarketplacesStrict(marketplaceNames)
  if (report.failures.length > 0) {
    logForDebugging(
      `Plugin autoupdate: ${report.failures.length} installed scope update(s) failed`,
      { level: 'warn' },
    )
  }
  return report.updatedPluginIds
}

/**
 * Update plugins from marketplaces that have autoUpdate enabled.
 * Returns the list of plugin IDs that were updated.
 */
async function updatePlugins(
  autoUpdateEnabledMarketplaces: Set<string>,
): Promise<string[]> {
  return updatePluginsForMarketplaces(autoUpdateEnabledMarketplaces)
}

/**
 * Auto-update marketplaces and plugins in the background.
 *
 * This function:
 * 1. Checks which marketplaces have autoUpdate enabled
 * 2. Refreshes only those marketplaces (git pull/re-download)
 * 3. Updates installed plugins from those marketplaces
 * 4. If any plugins were updated, notifies via the registered callback
 *
 * Official Acosmi marketplaces have autoUpdate enabled by default,
 * but users can disable it per-marketplace in the UI.
 *
 * This function runs silently without blocking user interaction.
 * Called from main.tsx during startup as a background job.
 */
export function autoUpdateMarketplacesAndPluginsInBackground(): void {
  void (async () => {
    if (shouldSkipPluginAutoupdate()) {
      logForDebugging('Plugin autoupdate: skipped (auto-updater disabled)')
      return
    }

    try {
      // Get marketplaces with autoUpdate enabled
      const autoUpdateEnabledMarketplaces =
        await getAutoUpdateEnabledMarketplaces()

      if (autoUpdateEnabledMarketplaces.size === 0) {
        return
      }

      // Refresh only marketplaces with autoUpdate enabled
      const marketplaceNames = Array.from(autoUpdateEnabledMarketplaces)
      const refreshResults = await Promise.allSettled(
        marketplaceNames.map(name =>
          refreshMarketplace(name, undefined, {
            disableCredentialHelper: true,
          }),
        ),
      )

      // Only move installed content for catalogs whose refresh actually
      // committed. The prior inner catch made every allSettled result look
      // fulfilled and could silently update against an old generation.
      const refreshedMarketplaces = new Set<string>()
      for (const [index, result] of refreshResults.entries()) {
        const name = marketplaceNames[index]!
        if (result.status === 'fulfilled') {
          refreshedMarketplaces.add(name.toLowerCase())
        } else {
          logForDebugging(
            `Plugin autoupdate: failed to refresh marketplace ${name}: ${errorMessage(result.reason)}`,
            { level: 'warn' },
          )
        }
      }

      if (refreshedMarketplaces.size === 0) return
      logForDebugging('Plugin autoupdate: checking installed plugins')
      const updatedPlugins = await updatePlugins(refreshedMarketplaces)

      if (updatedPlugins.length > 0) {
        if (pluginUpdateCallback) {
          // Callback is already registered, invoke it immediately
          pluginUpdateCallback(updatedPlugins)
        } else {
          // Callback not yet registered (REPL not mounted), store for later delivery
          pendingNotification = updatedPlugins
        }
      }
    } catch (error) {
      logError(error)
    }
  })()
}
