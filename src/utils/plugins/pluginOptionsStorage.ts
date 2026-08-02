/**
 * Plugin option storage and substitution.
 *
 * Plugins declare user-configurable options in `manifest.userConfig` — a record
 * of field schemas matching `McpbUserConfigurationOption`. At enable time the
 * user is prompted for values. Storage splits by `sensitive`:
 *   - `sensitive: true`  → secureStorage (keychain on macOS, .credentials.json elsewhere)
 *   - everything else    → settings.json `pluginConfigs[pluginId].options`
 *
 * `loadPluginOptions` reads and merges both. The substitution helpers are also
 * here (moved from mcpPluginIntegration.ts) so hooks/LSP/skills don't all
 * import from MCP-specific code.
 */

import memoize from 'lodash-es/memoize.js'
import type { LoadedPlugin } from '../../types/plugin.js'
import { logForDebugging } from '../debug.js'
import { logError } from '../log.js'
import { getSecureStorage } from '../secureStorage/index.js'
import {
  refreshSettingsForSource,
  updateSettingsForSource,
} from '../settings/settings.js'
import {
  type UserConfigSchema,
  type UserConfigValues,
  validateUserConfig,
} from './mcpbHandler.js'
import { getPluginDataDir } from './pluginDirectories.js'

export type PluginOptionValues = UserConfigValues
export type PluginOptionSchema = UserConfigSchema
export type PluginOptionSnapshot = {
  values: PluginOptionValues
  storedSensitiveKeys: ReadonlySet<string>
}

/**
 * Canonical storage key for a plugin's options in both `settings.pluginConfigs`
 * and `secureStorage.pluginSecrets`. Today this is `plugin.source` — always
 * `"${name}@${marketplace}"` (pluginLoader.ts:1400). `plugin.repository` is
 * a backward-compat alias that's set to the same string (1401); don't use it
 * for storage. UI code that manually constructs `` `${name}@${marketplace}` ``
 * produces the same key by convention — see PluginOptionsFlow, ManagePlugins.
 *
 * Exists so there's exactly one place to change if the key format ever drifts.
 */
export function getPluginStorageId(plugin: LoadedPlugin): string {
  return plugin.source
}

/**
 * Load saved option values for a plugin, merging non-sensitive (from settings)
 * with sensitive (from secureStorage). SecureStorage wins on key collision.
 *
 * Memoized per-pluginId because hooks can fire per-tool-call and each call
 * would otherwise do a settings read + keychain spawn. Cache cleared via
 * `clearPluginOptionsCache` when settings change or plugins reload.
 */
export const loadPluginOptionsWithProvenance = memoize(
  (pluginId: string): PluginOptionSnapshot => {
    // Plugin options feed commands, hooks and MCP transports. Only the fresh
    // user-owned source may be paired with secureStorage secrets; a malicious
    // project/policy sibling must never be merged into this trust boundary.
    const settings = refreshSettingsForSource('userSettings')
    const nonSensitive =
      settings?.pluginConfigs?.[pluginId]?.options ??
      ({} as PluginOptionValues)

    // A synchronous read remains appropriate here: runtime measurement
    // confirmed this
    // plugin read surface did not appear in the login hot path; observed
    // keychain cold misses in that flow were ~12-19ms, while the user-visible
    // bill came from OAuth/profile network waits and explicit login wait
    // windows. Keep this synchronous until a future trace proves a plugin-load
    // hot-path cost, because making it async would cascade hooks, LSP, command,
    // agent, and getUnconfiguredOptions public APIs.
    const storage = getSecureStorage()
    const allSensitive = storage.read()?.pluginSecrets as
      | Record<string, PluginOptionValues>
      | undefined
    const sensitive = allSensitive?.[pluginId] ?? {}

    // secureStorage wins on collision — schema determines destination so
    // collision shouldn't happen, but if a user hand-edits settings.json we
    // trust the more secure source.
    return {
      values: { ...nonSensitive, ...sensitive },
      storedSensitiveKeys: new Set(Object.keys(sensitive)),
    }
  },
)

export const loadPluginOptions = memoize(
  (pluginId: string): PluginOptionValues =>
    loadPluginOptionsWithProvenance(pluginId).values,
)

export function clearPluginOptionsCache(): void {
  loadPluginOptions.cache?.clear?.()
  loadPluginOptionsWithProvenance.cache?.clear?.()
}

/**
 * Save option values, splitting by `schema[key].sensitive`. Non-sensitive go
 * to userSettings; sensitive go to secureStorage. Writes are skipped if nothing
 * in that category is present.
 *
 * Clears the load cache on success so the next `loadPluginOptions` sees fresh.
 */
export async function savePluginOptions(
  pluginId: string,
  values: PluginOptionValues,
  schema: PluginOptionSchema,
): Promise<void> {
  const nonSensitive: PluginOptionValues = {}
  const sensitive: PluginOptionValues = {}

  for (const [key, value] of Object.entries(values)) {
    if (schema[key]?.sensitive === true) {
      sensitive[key] = value
    } else {
      nonSensitive[key] = value
    }
  }

  // Scrub sets — see saveMcpServerUserConfig (mcpbHandler.ts) for the
  // rationale. Only keys in THIS save are scrubbed from the other store,
  // so partial reconfigures don't lose data.
  const sensitiveKeysInThisSave = new Set(Object.keys(sensitive))
  const nonSensitiveKeysInThisSave = new Set(Object.keys(nonSensitive))

  // secureStorage FIRST — if keychain fails, throw before touching
  // settings.json so old plaintext (if any) stays as fallback.
  //
  // T6-P1-AUTH-CALLERS — `pluginSecrets[pluginId]` is a SUBSET write of the
  // shared credential record, so it goes through mutateAsync (RFC §2.2): the
  // read-modify-write runs inside a process-level serialized critical section,
  // so two plugins configured concurrently can't clobber each other's
  // pluginSecrets entry (lost update). The readAsync() peek below only decides
  // whether to ENTER the critical section (so a non-sensitive-only save skips
  // the keychain write entirely); the mutator recomputes the scrub from the
  // in-section `current`, so the peek racing a concurrent writer is harmless
  // — the schema split already routes each key to exactly one store, the
  // scrub is defense-in-depth, not correctness.
  const storage = getSecureStorage()
  let needSecureWrite = Object.keys(sensitive).length > 0
  if (!needSecureWrite) {
    // Scrub-only path: a key flipped sensitive→non-sensitive needs its stale
    // value removed from secureStorage. Peek to avoid a keychain write when
    // there is nothing to scrub (the common non-sensitive-only save).
    const peek = (await storage.readAsync())?.pluginSecrets as
      | Record<string, PluginOptionValues>
      | undefined
    const existingForPlugin = peek?.[pluginId]
    if (existingForPlugin) {
      needSecureWrite = Object.keys(existingForPlugin).some(k =>
        nonSensitiveKeysInThisSave.has(k),
      )
    }
  }
  if (needSecureWrite) {
    const result = await storage.mutateAsync(current => {
      const allSecrets =
        (current.pluginSecrets as
          | Record<string, PluginOptionValues>
          | undefined) ?? {}
      const existingForPlugin = allSecrets[pluginId]
      const secureScrubbed = existingForPlugin
        ? Object.fromEntries(
            Object.entries(existingForPlugin).filter(
              ([k]) => !nonSensitiveKeysInThisSave.has(k),
            ),
          )
        : {}
      return {
        ...current,
        pluginSecrets: {
          ...allSecrets,
          [pluginId]: { ...secureScrubbed, ...sensitive },
        },
      }
    })
    if (!result.success) {
      const err = new Error(
        `Failed to save sensitive plugin options for ${pluginId} to secure storage`,
      )
      logError(err)
      throw err
    }
    if (result.warning) {
      logForDebugging(`Plugin secrets save warning: ${result.warning}`, {
        level: 'warn',
      })
    }
  }

  // settings.json AFTER secureStorage — scrub sensitive keys via explicit
  // undefined (mergeWith deletion pattern). Inspect only a fresh userSettings
  // snapshot; merged project/local/policy values are untrusted at this
  // persistence boundary.
  const settings = refreshSettingsForSource('userSettings')
  const existingInSettings = settings?.pluginConfigs?.[pluginId]?.options ?? {}
  const keysToScrubFromSettings = Object.keys(existingInSettings).filter(k =>
    sensitiveKeysInThisSave.has(k),
  )
  if (
    Object.keys(nonSensitive).length > 0 ||
    keysToScrubFromSettings.length > 0
  ) {
    const scrubbed = Object.fromEntries(
      keysToScrubFromSettings.map(k => [k, undefined]),
    ) as Record<string, undefined>
    // Send only the target leaf. updateSettingsForSource locks and rereads the
    // user file before merging, preserving concurrent siblings without ever
    // promoting values from an effective/merged settings snapshot.
    const result = updateSettingsForSource('userSettings', {
      pluginConfigs: {
        [pluginId]: {
          options: {
            ...nonSensitive,
            ...scrubbed,
          } as PluginOptionValues,
        },
      },
    })
    if (result.error) {
      logError(result.error)
      throw new Error(
        `Failed to save plugin options for ${pluginId}: ${result.error.message}`,
      )
    }
  }

  clearPluginOptionsCache()
}

/**
 * Delete all stored option values for a plugin — both the non-sensitive
 * `settings.pluginConfigs[pluginId]` entry and the sensitive
 * `secureStorage.pluginSecrets[pluginId]` entry.
 *
 * Call this when the LAST installation of a plugin is uninstalled (i.e.,
 * alongside `markPluginVersionOrphaned`). Don't call on every uninstall —
 * a plugin can be installed in multiple scopes and the user's config should
 * survive removing it from one scope while it remains in another.
 *
 * Best-effort: keychain write failure is logged but doesn't throw, since
 * the uninstall itself succeeded and we don't want to surface a confusing
 * "uninstall failed" message for a cleanup side-effect.
 */
export async function deletePluginOptions(pluginId: string): Promise<void> {
  // Settings side — also wipes per-server MCP activation and the legacy
  // mcpServers sub-key (same story: orphaned on uninstall).
  //
  // Use `undefined` (not `delete`) because `updateSettingsForSource` merges
  // via `mergeWith` — absent keys are ignored, only `undefined` triggers
  // removal. Cast is deliberate (CRABCODE.md's 10% case): adding z.undefined()
  // to the schema instead (like enabledPlugins:466 does) leaks
  // `| {[k: string]: unknown}` into the public SDK type, which subsumes the
  // real object arm and kills excess-property checks for SDK consumers. The
  // mergeWith-deletion contract is internal plumbing — it shouldn't shape
  // the Zod schema. enabledPlugins gets away with it only because its other
  // arms (string[] | boolean) are non-objects that stay distinct.
  const settings = refreshSettingsForSource('userSettings')
  type PluginConfigs = NonNullable<
    NonNullable<typeof settings>['pluginConfigs']
  >
  if (settings?.pluginConfigs?.[pluginId]) {
    // Partial<Record<K,V>> = Record<K, V | undefined> — gives us the widening
    // for the undefined value, and Partial-of-X overlaps with X so the cast
    // is a narrowing TS accepts (same approach as marketplaceManager.ts:1795).
    const pluginConfigs: Partial<PluginConfigs> = { [pluginId]: undefined }
    const { error } = updateSettingsForSource('userSettings', {
      pluginConfigs: pluginConfigs as PluginConfigs,
    })
    if (error) {
      logForDebugging(
        `deletePluginOptions: failed to clear settings.pluginConfigs[${pluginId}]: ${error.message}`,
        { level: 'warn' },
      )
    }
  }

  // Secure storage side — delete both the top-level pluginSecrets[pluginId]
  // and any per-server composite keys `${pluginId}/${server}` (from
  // saveMcpServerUserConfig's sensitive split). `/` prefix match is safe:
  // plugin IDs are `name@marketplace`, never contain `/`, so
  // startsWith(`${id}/`) can't false-positive on a different plugin.
  //
  // T6-P1-AUTH-CALLERS — subset delete of the shared credential record →
  // mutateAsync (RFC §2.2): the read-modify-write runs in a process-level
  // serialized critical section so a concurrent secureStorage writer can't be
  // clobbered. The readAsync() peek only decides whether to enter the section
  // (skip the keychain write when the plugin had no stored secrets); the
  // mutator recomputes surviving entries from the in-section `current`.
  const storage = getSecureStorage()
  const prefix = `${pluginId}/`
  const existingSecrets = (await storage.readAsync())?.pluginSecrets as
    | Record<string, unknown>
    | undefined
  if (
    existingSecrets !== undefined &&
    Object.keys(existingSecrets).some(
      k => k === pluginId || k.startsWith(prefix),
    )
  ) {
    const result = await storage.mutateAsync(current => {
      const secrets = current.pluginSecrets as
        | Record<string, unknown>
        | undefined
      if (secrets === undefined) {
        return current
      }
      const survivingEntries = Object.entries(secrets).filter(
        ([k]) => k !== pluginId && !k.startsWith(prefix),
      )
      return {
        ...current,
        pluginSecrets:
          survivingEntries.length > 0
            ? Object.fromEntries(survivingEntries)
            : undefined,
      }
    })
    if (!result.success) {
      logForDebugging(
        `deletePluginOptions: failed to clear pluginSecrets for ${pluginId} from keychain`,
        { level: 'warn' },
      )
    }
  }

  clearPluginOptionsCache()
}

/**
 * Find option keys whose saved values don't satisfy the schema — i.e., what to
 * prompt for. Returns the schema slice for those keys, or empty if everything
 * validates. Empty manifest.userConfig → empty result.
 *
 * Used by PluginOptionsFlow to decide whether to show the prompt after enable.
 */
export function getUnconfiguredOptions(
  plugin: LoadedPlugin,
): PluginOptionSchema {
  const manifestSchema = plugin.manifest.userConfig
  if (!manifestSchema || Object.keys(manifestSchema).length === 0) {
    return {}
  }

  const saved = loadPluginOptions(getPluginStorageId(plugin))
  const validation = validateUserConfig(saved, manifestSchema)
  if (validation.valid) {
    return {}
  }

  // Return only the fields that failed. validateUserConfig reports errors as
  // strings keyed by title/key — simpler to just re-check each field here than
  // parse error strings.
  const unconfigured: PluginOptionSchema = {}
  for (const [key, fieldSchema] of Object.entries(manifestSchema)) {
    const single = validateUserConfig(
      { [key]: saved[key] } as PluginOptionValues,
      { [key]: fieldSchema },
    )
    if (!single.valid) {
      unconfigured[key] = fieldSchema
    }
  }
  return unconfigured
}

/**
 * Substitute ${CRABCODE_PLUGIN_ROOT} and ${CRABCODE_PLUGIN_DATA} with their paths.
 * On Windows, normalizes backslashes to forward slashes so shell commands
 * don't interpret them as escape characters.
 *
 * ${CRABCODE_PLUGIN_ROOT} — version-scoped install dir (recreated on update)
 * ${CRABCODE_PLUGIN_DATA} — persistent state dir (survives updates)
 *
 * Both patterns use the function-replacement form of .replace(): ROOT so
 * `$`-patterns in NTFS paths ($$, $', $`, $&) aren't interpreted; DATA so
 * getPluginDataDir (which lazily mkdirs) only runs when actually present.
 *
 * Used in MCP/LSP server command/args/env, hook commands, skill/agent content.
 */
export function substitutePluginVariables(
  value: string,
  plugin: { path: string; source?: string },
): string {
  const normalize = (p: string) =>
    process.platform === 'win32' ? p.replace(/\\/g, '/') : p
  let out = value.replace(/\$\{CRABCODE_PLUGIN_ROOT\}/g, () =>
    normalize(plugin.path),
  )
  // source can be absent (e.g. hooks where pluginRoot is a skill root without
  // a plugin context). In that case ${CRABCODE_PLUGIN_DATA} is left literal.
  if (plugin.source) {
    const source = plugin.source
    out = out.replace(/\$\{CRABCODE_PLUGIN_DATA\}/g, () =>
      normalize(getPluginDataDir(source)),
    )
  }
  return out
}

/**
 * Substitute ${user_config.KEY} with saved option values.
 *
 * Throws on missing keys — callers pass this only after `validateUserConfig`
 * succeeded, so a miss here means a plugin references a key it never declared
 * in its schema. That's a plugin authoring bug; failing loud surfaces it.
 *
 * Use `substituteUserConfigInContent` for skill/agent prose — it handles
 * missing keys and sensitive-filtering instead of throwing.
 */
export function substituteUserConfigVariables(
  value: string,
  userConfig: PluginOptionValues,
): string {
  return value.replace(/\$\{user_config\.([^}]+)\}/g, (_match, key) => {
    const configValue = userConfig[key]
    if (configValue === undefined) {
      throw new Error(
        `Missing required user configuration value: ${key}. ` +
          `This should have been validated before variable substitution.`,
      )
    }
    return String(configValue)
  })
}

/**
 * Content-safe variant for skill/agent prose. Differences from
 * `substituteUserConfigVariables`:
 *
 *   - Sensitive-marked keys substitute to a descriptive placeholder instead of
 *     the actual value — skill/agent content goes to the model prompt, and
 *     we don't put secrets in the model's context.
 *   - Unknown keys stay literal (no throw) — matches how `${VAR}` env refs
 *     behave today when the var is unset.
 *
 * A ref to a sensitive key produces obvious-looking output so plugin authors
 * notice and move the ref into a hook/MCP env instead.
 */
export function substituteUserConfigInContent(
  content: string,
  options: PluginOptionValues,
  schema: PluginOptionSchema,
): string {
  return content.replace(/\$\{user_config\.([^}]+)\}/g, (match, key) => {
    if (schema[key]?.sensitive === true) {
      return `[sensitive option '${key}' not available in skill content]`
    }
    const value = options[key]
    if (value === undefined) {
      return match
    }
    return String(value)
  })
}
