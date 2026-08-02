const PLUGIN_ID_COMPONENT = /^[a-z0-9][-a-z0-9._]*$/i

export type ExactMarketplacePluginId = {
  pluginId: string
  pluginName: string
  marketplaceName: string
}

export type InstalledPluginIdentityResolution<T> =
  | (ExactMarketplacePluginId & {
      status: 'resolved'
      installations: T[]
    })
  | { status: 'not-found' }
  | { status: 'ambiguous'; candidates: string[] }
  | { status: 'invalid' }

/**
 * Parse only the canonical plugin@marketplace form accepted by the
 * installed-plugin and settings registries. In particular, this rejects bare
 * names and multiple separators instead of letting callers guess between two
 * marketplaces that publish the same plugin name.
 */
export function parseExactMarketplacePluginId(
  value: string,
): ExactMarketplacePluginId | null {
  if (value !== value.trim()) return null
  const parts = value.split('@')
  if (parts.length !== 2) return null
  const [pluginName, marketplaceName] = parts
  if (
    !PLUGIN_ID_COMPONENT.test(pluginName ?? '') ||
    !PLUGIN_ID_COMPONENT.test(marketplaceName ?? '')
  ) {
    return null
  }
  return { pluginId: value, pluginName: pluginName!, marketplaceName: marketplaceName! }
}

/** Resolve an installed record by exact key only; never fall back by name. */
export function resolveExactInstalledPluginIdentity<T>(
  value: string,
  installedPlugins: Record<string, T[]>,
): (ExactMarketplacePluginId & { installations: T[] }) | null {
  const identity = parseExactMarketplacePluginId(value)
  if (!identity) return null
  const installations = installedPlugins[identity.pluginId]
  if (!installations || installations.length === 0) return null
  return { ...identity, installations }
}

/**
 * Resolve the shared CLI/TUI uninstall argument without guessing:
 *
 * - a fully-qualified ID is always looked up by its exact registry key;
 * - a bare name is compatible only when exactly one installed, fully-qualified
 *   registry key has that name;
 * - multiple matching marketplaces are reported as ambiguous.
 *
 * Structured callers never use the bare-name branch because the runtime
 * protocol boundaries require a fully-qualified ID before calling the shared
 * operation.
 */
export function resolveInstalledPluginIdentityForUninstall<T>(
  value: string,
  installedPlugins: Record<string, T[]>,
): InstalledPluginIdentityResolution<T> {
  const exactIdentity = parseExactMarketplacePluginId(value)
  if (exactIdentity) {
    const installations = installedPlugins[exactIdentity.pluginId]
    if (!installations || installations.length === 0) {
      return { status: 'not-found' }
    }
    return { status: 'resolved', ...exactIdentity, installations }
  }

  if (
    value !== value.trim() ||
    value.includes('@') ||
    !PLUGIN_ID_COMPONENT.test(value)
  ) {
    return { status: 'invalid' }
  }

  const matches = Object.entries(installedPlugins)
    .flatMap(([pluginId, installations]) => {
      if (installations.length === 0) return []
      const identity = parseExactMarketplacePluginId(pluginId)
      return identity?.pluginName === value
        ? [{ ...identity, installations }]
        : []
    })
    .sort((left, right) =>
      left.pluginId < right.pluginId
        ? -1
        : left.pluginId > right.pluginId
          ? 1
          : 0,
    )

  if (matches.length === 0) return { status: 'not-found' }
  if (matches.length > 1) {
    return {
      status: 'ambiguous',
      candidates: matches.map(match => match.pluginId),
    }
  }
  return { status: 'resolved', ...matches[0]! }
}
