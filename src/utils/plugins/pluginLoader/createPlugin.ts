/**
 * Plugin creation from directory path.
 *
 * Assembles a LoadedPlugin by scanning plugin directory structure,
 * loading manifests, hooks, settings, and detecting components.
 */

import { readFile } from 'fs/promises'
import { resolve } from 'path'
import type {
  LoadedPlugin,
  PluginComponent,
  PluginError,
  PluginManifest,
} from '../../../types/plugin.js'
import { logForDebugging } from '../../debug.js'
import { errorMessage, isFsInaccessible, toError } from '../../errors.js'
import { pathExists } from '../../file.js'
import { lazySchema } from '../../lazySchema.js'
import { logError } from '../../log.js'
import type { HooksSettings } from '../../settings/types.js'
import { SettingsSchema } from '../../settings/types.js'
import { jsonParse } from '../../slowOperations.js'
import {
  PluginPathSecurityError,
  readCanonicalPluginTextFile,
  resolvePluginComponentPath,
} from '../pluginPathSecurity.js'
import {
  type CommandMetadata,
  PluginHooksSchema,
  PluginManifestSchema,
} from '../schemas.js'

/**
 * Loads and validates a plugin manifest from a JSON file.
 *
 * The manifest provides metadata about the plugin including name, version,
 * description, author, and other optional fields. If no manifest exists,
 * a minimal one is created to allow the plugin to function.
 *
 * Unknown keys in the manifest are silently stripped (PluginManifestSchema
 * uses zod's default strip behavior, not .strict()). Type mismatches and
 * other validation errors still fail.
 *
 * Behavior:
 * - Missing file: Creates default with provided name and source
 * - Invalid JSON: Throws error with parse details
 * - Schema validation failure: Throws error with validation details
 *
 * @param manifestPath - Full path to the plugin.json file
 * @param pluginName - Name to use in default manifest (e.g., "my-plugin")
 * @param source - Source description for default manifest (e.g., "git:repo" or ".crabcode-plugin/name")
 * @returns A valid PluginManifest object (either loaded or default)
 * @throws Error if manifest exists but is invalid (corrupt JSON or schema validation failure)
 */
export async function loadPluginManifest(
  manifestPath: string,
  pluginName: string,
  source: string,
  pluginRoot?: string,
): Promise<PluginManifest> {
  // Check if manifest file exists
  // If not, create a minimal manifest to allow plugin to function
  if (!(await pathExists(manifestPath))) {
    // Return default manifest with provided name and source
    return {
      name: pluginName,
      description: `Plugin from ${source}`,
    }
  }

  try {
    // Read and parse the manifest JSON file
    const content = pluginRoot
      ? await readCanonicalPluginTextFile(
          pluginRoot,
          manifestPath,
          'plugin manifest',
        )
      : await readFile(manifestPath, { encoding: 'utf-8' })
    const parsedJson = jsonParse(content)

    // Validate against the PluginManifest schema
    const result = PluginManifestSchema().safeParse(parsedJson)

    if (result.success) {
      // Valid manifest - return the validated data
      return result.data
    }

    // Schema validation failed but JSON was valid
    const errors = result.error.issues
      .map(err =>
        err.path.length > 0
          ? `${err.path.join('.')}: ${err.message}`
          : err.message,
      )
      .join(', ')

    logForDebugging(
      `Plugin ${pluginName} has an invalid manifest file at ${manifestPath}. Validation errors: ${errors}`,
      { level: 'error' },
    )

    throw new Error(
      `Plugin ${pluginName} has an invalid manifest file at ${manifestPath}.\n\nValidation errors: ${errors}`,
    )
  } catch (error) {
    // Check if this is the error we just threw (validation error)
    if (
      error instanceof Error &&
      error.message.includes('invalid manifest file')
    ) {
      throw error
    }

    // JSON parsing failed or file read error
    const errorMsg = errorMessage(error)

    logForDebugging(
      `Plugin ${pluginName} has a corrupt manifest file at ${manifestPath}. Parse error: ${errorMsg}`,
      { level: 'error' },
    )

    throw new Error(
      `Plugin ${pluginName} has a corrupt manifest file at ${manifestPath}.\n\nJSON parse error: ${errorMsg}`,
    )
  }
}

/**
 * Loads and validates plugin hooks configuration from a JSON file.
 * IMPORTANT: Only call this when the hooks file is expected to exist.
 *
 * @param hooksConfigPath - Full path to the hooks.json file
 * @param pluginName - Plugin name for error messages
 * @returns Validated HooksSettings
 * @throws Error if file doesn't exist or is invalid
 */
async function loadPluginHooks(
  hooksConfigPath: string,
  pluginName: string,
  pluginRoot: string,
): Promise<HooksSettings> {
  if (!(await pathExists(hooksConfigPath))) {
    throw new Error(
      `Hooks file not found at ${hooksConfigPath} for plugin ${pluginName}. If the manifest declares hooks, the file must exist.`,
    )
  }

  const content = await readCanonicalPluginTextFile(
    pluginRoot,
    hooksConfigPath,
    'hooks config',
  )
  const rawHooksConfig = jsonParse(content)

  // The hooks.json file has a wrapper structure with description and hooks
  // Use PluginHooksSchema to validate and extract the hooks property
  const validatedPluginHooks = PluginHooksSchema().parse(rawHooksConfig)

  return validatedPluginHooks.hooks as HooksSettings
}

export function recordPluginComponentPathFailure(
  errors: PluginError[],
  error: unknown,
  details: {
    pluginPath: string
    requestedPath: string
    pluginName: string
    source: string
    component: PluginComponent
  },
): void {
  const rejectedPath = resolve(details.pluginPath, details.requestedPath)
  const reason = errorMessage(error)
  logForDebugging(
    `Rejected ${details.component} path ${details.requestedPath} for ${details.pluginName}: ${reason}`,
    { level: 'warn' },
  )
  logError(toError(error))
  if (
    error instanceof PluginPathSecurityError &&
    error.reason === 'path-missing'
  ) {
    errors.push({
      type: 'path-not-found',
      source: details.source,
      plugin: details.pluginName,
      path: rejectedPath,
      component: details.component,
    })
    return
  }
  errors.push({
    type: 'component-load-failed',
    source: details.source,
    plugin: details.pluginName,
    component: details.component,
    path: rejectedPath,
    reason,
  })
}

async function resolveOptionalComponentPath(
  pluginPath: string,
  relativePath: string,
  pluginName: string,
  source: string,
  component: PluginComponent,
  errors: PluginError[],
): Promise<string | undefined> {
  try {
    return await resolvePluginComponentPath(pluginPath, relativePath, {
      component,
    })
  } catch (error) {
    if (
      error instanceof PluginPathSecurityError &&
      error.reason === 'path-missing'
    ) {
      return undefined
    }
    recordPluginComponentPathFailure(errors, error, {
      pluginPath,
      requestedPath: relativePath,
      pluginName,
      source,
      component,
    })
    return undefined
  }
}

/**
 * Validate plugin component paths using canonical realpath containment.
 *
 * This helper parallelizes the pathExists checks (the expensive async part) while
 * preserving deterministic error/log ordering by iterating results sequentially.
 *
 * @param relPaths - Relative paths from the manifest/marketplace entry to validate
 * @param pluginPath - Plugin root directory to resolve relative paths against
 * @param pluginName - Plugin name for error messages
 * @param source - Source identifier for PluginError records
 * @param component - Which component these paths belong to (for error records)
 * @param componentLabel - Human-readable label for log messages (e.g. "Agent", "Skill")
 * @param contextLabel - Where the path came from, for log messages
 * @param errors - Error array to push path-not-found errors into (mutated)
 * @returns Array of full paths that exist on disk, in original order
 */
export async function validatePluginPaths(
  relPaths: string[],
  pluginPath: string,
  pluginName: string,
  source: string,
  component: PluginComponent,
  componentLabel: string,
  contextLabel: string,
  errors: PluginError[],
): Promise<string[]> {
  const checks = await Promise.all(
    relPaths.map(async relPath => {
      try {
        const fullPath = await resolvePluginComponentPath(
          pluginPath,
          relPath,
          { component },
        )
        return { relPath, fullPath, error: undefined }
      } catch (error) {
        return { relPath, fullPath: resolve(pluginPath, relPath), error }
      }
    }),
  )
  // Process results in original order to keep error/log ordering deterministic
  const validPaths: string[] = []
  for (const { relPath, fullPath, error } of checks) {
    if (!error) {
      validPaths.push(fullPath)
    } else {
      logForDebugging(
        `${componentLabel} path ${relPath} ${contextLabel} rejected at ${fullPath} for ${pluginName}`,
        { level: 'warn' },
      )
      recordPluginComponentPathFailure(errors, error, {
        pluginPath,
        requestedPath: relPath,
        pluginName,
        source,
        component,
      })
    }
  }
  return validPaths
}

/**
 * Creates a LoadedPlugin object from a plugin directory path.
 *
 * This is the central function that assembles a complete plugin representation
 * by scanning the plugin directory structure and loading all components.
 *
 * @param pluginPath - Absolute path to the plugin directory
 * @param source - Source identifier (e.g., "git:repo", ".crabcode-plugin/my-plugin")
 * @param enabled - Initial enabled state (may be overridden by settings)
 * @param fallbackName - Name to use if manifest doesn't specify one
 * @param strict - When true, adds errors for duplicate hook files (default: true)
 * @returns Object containing the LoadedPlugin and any errors encountered
 */
export async function createPluginFromPath(
  pluginPath: string,
  source: string,
  enabled: boolean,
  fallbackName: string,
  strict = true,
): Promise<{ plugin: LoadedPlugin; errors: PluginError[] }> {
  const errors: PluginError[] = []
  // Canonicalize the root once so later consumers do not re-follow a
  // --plugin-dir/cache-root symlink that can be retargeted after discovery.
  const canonicalPluginPath = await resolvePluginComponentPath(
    pluginPath,
    '.',
    { component: 'plugin root' },
  )
  pluginPath = canonicalPluginPath

  // Step 1: Load or create the plugin manifest
  const manifestPath = await resolvePluginComponentPath(
    canonicalPluginPath,
    '.crabcode-plugin/plugin.json',
    { mustExist: false, component: 'plugin manifest' },
  )
  const manifest = await loadPluginManifest(
    manifestPath,
    fallbackName,
    source,
    canonicalPluginPath,
  )

  // Step 2: Create the base plugin object
  const plugin: LoadedPlugin = {
    name: manifest.name,
    manifest,
    path: canonicalPluginPath,
    source,
    repository: source,
    enabled,
  }

  // Step 3: Auto-detect optional directories through the same realpath gate.
  const [commandsPath, agentsPath, skillsPath, outputStylesPath] =
    await Promise.all([
      !manifest.commands
        ? resolveOptionalComponentPath(
            canonicalPluginPath,
            'commands',
            manifest.name,
            source,
            'commands',
            errors,
          )
        : undefined,
      !manifest.agents
        ? resolveOptionalComponentPath(
            canonicalPluginPath,
            'agents',
            manifest.name,
            source,
            'agents',
            errors,
          )
        : undefined,
      !manifest.skills
        ? resolveOptionalComponentPath(
            canonicalPluginPath,
            'skills',
            manifest.name,
            source,
            'skills',
            errors,
          )
        : undefined,
      !manifest.outputStyles
        ? resolveOptionalComponentPath(
            canonicalPluginPath,
            'output-styles',
            manifest.name,
            source,
            'output-styles',
            errors,
          )
        : undefined,
    ])

  if (commandsPath) {
    plugin.commandsPath = commandsPath
  }

  // Step 3a: Process additional command paths from manifest
  if (manifest.commands) {
    // Check if it's an object mapping (record of command name -> metadata)
    const firstValue = Object.values(manifest.commands)[0]
    if (
      typeof manifest.commands === 'object' &&
      !Array.isArray(manifest.commands) &&
      firstValue &&
      typeof firstValue === 'object' &&
      ('source' in firstValue || 'content' in firstValue)
    ) {
      // Object mapping format: { "about": { "source": "./README.md", ... } }
      const commandsMetadata: Record<string, CommandMetadata> = {}
      const validPaths: string[] = []

      const entries = Object.entries(manifest.commands)
      const checks = await Promise.all(
        entries.map(async ([commandName, metadata]) => {
          if (!metadata || typeof metadata !== 'object') {
            return { commandName, metadata, kind: 'skip' as const }
          }
          if (metadata.source) {
            try {
              const fullPath = await resolvePluginComponentPath(
                pluginPath,
                metadata.source,
                { component: 'commands' },
              )
              return {
                commandName,
                metadata,
                kind: 'source' as const,
                fullPath,
                error: undefined,
              }
            } catch (error) {
              return {
                commandName,
                metadata,
                kind: 'source' as const,
                fullPath: resolve(pluginPath, metadata.source),
                error,
              }
            }
          }
          if (metadata.content) {
            return { commandName, metadata, kind: 'content' as const }
          }
          return { commandName, metadata, kind: 'skip' as const }
        }),
      )
      for (const check of checks) {
        if (check.kind === 'skip') continue
        if (check.kind === 'content') {
          commandsMetadata[check.commandName] = check.metadata
          continue
        }
        // kind === 'source'
        if (!check.error) {
          validPaths.push(check.fullPath)
          commandsMetadata[check.commandName] = check.metadata
        } else {
          recordPluginComponentPathFailure(errors, check.error, {
            pluginPath,
            requestedPath: check.metadata.source!,
            pluginName: manifest.name,
            source,
            component: 'commands',
          })
        }
      }

      if (validPaths.length > 0) {
        plugin.commandsPaths = validPaths
      }
      if (Object.keys(commandsMetadata).length > 0) {
        plugin.commandsMetadata = commandsMetadata
      }
    } else {
      // Path or array of paths format
      const commandPaths = Array.isArray(manifest.commands)
        ? manifest.commands
        : [manifest.commands]

      const stringCommandPaths = commandPaths.filter(
        (cmdPath): cmdPath is string => {
          if (typeof cmdPath === 'string') return true
          logForDebugging(
            `Unexpected command format in manifest for ${manifest.name}`,
            { level: 'error' },
          )
          return false
        },
      )
      const validPaths = await validatePluginPaths(
        stringCommandPaths,
        pluginPath,
        manifest.name,
        source,
        'commands',
        'Command',
        'specified in manifest but',
        errors,
      )

      if (validPaths.length > 0) {
        plugin.commandsPaths = validPaths
      }
    }
  }

  // Step 4: Register agents directory if detected
  if (agentsPath) {
    plugin.agentsPath = agentsPath
  }

  // Step 4a: Process additional agent paths from manifest
  if (manifest.agents) {
    const agentPaths = Array.isArray(manifest.agents)
      ? manifest.agents
      : [manifest.agents]

    const validPaths = await validatePluginPaths(
      agentPaths,
      pluginPath,
      manifest.name,
      source,
      'agents',
      'Agent',
      'specified in manifest but',
      errors,
    )

    if (validPaths.length > 0) {
      plugin.agentsPaths = validPaths
    }
  }

  // Step 4b: Register skills directory if detected
  if (skillsPath) {
    plugin.skillsPath = skillsPath
  }

  // Step 4c: Process additional skill paths from manifest
  if (manifest.skills) {
    const skillPaths = Array.isArray(manifest.skills)
      ? manifest.skills
      : [manifest.skills]

    const validPaths = await validatePluginPaths(
      skillPaths,
      pluginPath,
      manifest.name,
      source,
      'skills',
      'Skill',
      'specified in manifest but',
      errors,
    )

    if (validPaths.length > 0) {
      plugin.skillsPaths = validPaths
    }
  }

  // Step 4d: Register output-styles directory if detected
  if (outputStylesPath) {
    plugin.outputStylesPath = outputStylesPath
  }

  // Step 4e: Process additional output style paths from manifest
  if (manifest.outputStyles) {
    const outputStylePaths = Array.isArray(manifest.outputStyles)
      ? manifest.outputStyles
      : [manifest.outputStyles]

    const validPaths = await validatePluginPaths(
      outputStylePaths,
      pluginPath,
      manifest.name,
      source,
      'output-styles',
      'Output style',
      'specified in manifest but',
      errors,
    )

    if (validPaths.length > 0) {
      plugin.outputStylesPaths = validPaths
    }
  }

  // Step 5: Load hooks configuration
  let mergedHooks: HooksSettings | undefined
  const loadedHookPaths = new Set<string>() // Track loaded hook files

  // Load from standard hooks/hooks.json if it exists
  let standardHooksPath: string | undefined
  try {
    standardHooksPath = await resolvePluginComponentPath(
      pluginPath,
      'hooks/hooks.json',
      { component: 'default hooks config' },
    )
  } catch (error) {
    if (
      !(error instanceof PluginPathSecurityError) ||
      error.reason !== 'path-missing'
    ) {
      const reason = errorMessage(error)
      logError(toError(error))
      errors.push({
        type: 'hook-load-failed',
        source,
        plugin: manifest.name,
        hookPath: resolve(pluginPath, 'hooks/hooks.json'),
        reason,
      })
    }
  }
  if (standardHooksPath) {
    try {
      mergedHooks = await loadPluginHooks(
        standardHooksPath,
        manifest.name,
        pluginPath,
      )
      loadedHookPaths.add(standardHooksPath)
      logForDebugging(
        `Loaded hooks from standard location for plugin ${manifest.name}: ${standardHooksPath}`,
      )
    } catch (error) {
      const errorMsg = errorMessage(error)
      logForDebugging(
        `Failed to load hooks for ${manifest.name}: ${errorMsg}`,
        {
          level: 'error',
        },
      )
      logError(toError(error))
      errors.push({
        type: 'hook-load-failed',
        source,
        plugin: manifest.name,
        hookPath: standardHooksPath,
        reason: errorMsg,
      })
    }
  }

  // Load and merge hooks from manifest.hooks if specified
  if (manifest.hooks) {
    const manifestHooksArray = Array.isArray(manifest.hooks)
      ? manifest.hooks
      : [manifest.hooks]

    for (const hookSpec of manifestHooksArray) {
      if (typeof hookSpec === 'string') {
        let hookFilePath: string
        try {
          hookFilePath = await resolvePluginComponentPath(
            pluginPath,
            hookSpec,
            { component: 'hooks' },
          )
        } catch (error) {
          const reason = errorMessage(error)
          logError(toError(error))
          errors.push({
            type: 'hook-load-failed',
            source,
            plugin: manifest.name,
            hookPath: resolve(pluginPath, hookSpec),
            reason,
          })
          continue
        }

        const normalizedPath = hookFilePath

        if (loadedHookPaths.has(normalizedPath)) {
          logForDebugging(
            `Skipping duplicate hooks file for plugin ${manifest.name}: ${hookSpec} ` +
              `(resolves to already-loaded file: ${normalizedPath})`,
          )
          if (strict) {
            const errorMsg = `Duplicate hooks file detected: ${hookSpec} resolves to already-loaded file ${normalizedPath}. The standard hooks/hooks.json is loaded automatically, so manifest.hooks should only reference additional hook files.`
            logError(new Error(errorMsg))
            errors.push({
              type: 'hook-load-failed',
              source,
              plugin: manifest.name,
              hookPath: hookFilePath,
              reason: errorMsg,
            })
          }
          continue
        }

        try {
          const additionalHooks = await loadPluginHooks(
            hookFilePath,
            manifest.name,
            pluginPath,
          )
          try {
            mergedHooks = mergeHooksSettings(mergedHooks, additionalHooks)
            loadedHookPaths.add(normalizedPath)
            logForDebugging(
              `Loaded and merged hooks from manifest for plugin ${manifest.name}: ${hookSpec}`,
            )
          } catch (mergeError) {
            const mergeErrorMsg = errorMessage(mergeError)
            logForDebugging(
              `Failed to merge hooks from ${hookSpec} for ${manifest.name}: ${mergeErrorMsg}`,
              { level: 'error' },
            )
            logError(toError(mergeError))
            errors.push({
              type: 'hook-load-failed',
              source,
              plugin: manifest.name,
              hookPath: hookFilePath,
              reason: `Failed to merge: ${mergeErrorMsg}`,
            })
          }
        } catch (error) {
          const errorMsg = errorMessage(error)
          logForDebugging(
            `Failed to load hooks from ${hookSpec} for ${manifest.name}: ${errorMsg}`,
            { level: 'error' },
          )
          logError(toError(error))
          errors.push({
            type: 'hook-load-failed',
            source,
            plugin: manifest.name,
            hookPath: hookFilePath,
            reason: errorMsg,
          })
        }
      } else if (typeof hookSpec === 'object') {
        mergedHooks = mergeHooksSettings(mergedHooks, hookSpec as HooksSettings)
      }
    }
  }

  if (mergedHooks) {
    plugin.hooksConfig = mergedHooks
  }

  // Step 6: Load plugin settings
  const pluginSettings = await loadPluginSettings(
    pluginPath,
    manifest,
    errors,
    source,
  )
  if (pluginSettings) {
    plugin.settings = pluginSettings
  }

  return { plugin, errors }
}

/**
 * Schema derived from SettingsSchema that only keeps keys plugins are allowed to set.
 * Uses .strip() so unknown keys are silently removed during parsing.
 */
const PluginSettingsSchema = lazySchema(() =>
  SettingsSchema()
    .pick({
      agent: true,
    })
    .strip(),
)

/**
 * Parse raw settings through PluginSettingsSchema, returning only allowlisted keys.
 * Returns undefined if parsing fails or all keys are filtered out.
 */
function parsePluginSettings(
  raw: Record<string, unknown>,
): Record<string, unknown> | undefined {
  const result = PluginSettingsSchema().safeParse(raw)
  if (!result.success) {
    return undefined
  }
  const data = result.data
  if (Object.keys(data).length === 0) {
    return undefined
  }
  return data
}

/**
 * Load plugin settings from settings.json file or manifest.settings.
 * settings.json takes priority over manifest.settings when both exist.
 * Only allowlisted keys are included in the result.
 */
async function loadPluginSettings(
  pluginPath: string,
  manifest: PluginManifest,
  errors: PluginError[],
  source: string,
): Promise<Record<string, unknown> | undefined> {
  // Try loading settings.json from the plugin directory
  try {
    const settingsJsonPath = await resolvePluginComponentPath(
      pluginPath,
      'settings.json',
      { component: 'default plugin settings' },
    )
    const content = await readCanonicalPluginTextFile(
      pluginPath,
      settingsJsonPath,
      'default plugin settings',
    )
    const parsed = jsonParse(content)
    if (isRecord(parsed)) {
      const filtered = parsePluginSettings(parsed)
      if (filtered) {
        logForDebugging(
          `Loaded settings from settings.json for plugin ${manifest.name}`,
        )
        return filtered
      }
    }
  } catch (e: unknown) {
    if (e instanceof PluginPathSecurityError) {
      if (e.reason !== 'path-missing') {
        recordPluginComponentPathFailure(errors, e, {
          pluginPath,
          requestedPath: 'settings.json',
          pluginName: manifest.name,
          source,
          component: 'settings',
        })
      }
    } else if (!isFsInaccessible(e)) {
      // Preserve the existing parse/schema fallback and logging behavior.
      logForDebugging(
        `Failed to parse settings.json for plugin ${manifest.name}: ${e}`,
        { level: 'warn' },
      )
    }
  }

  // Fall back to manifest.settings
  if (manifest.settings) {
    const filtered = parsePluginSettings(
      manifest.settings as Record<string, unknown>,
    )
    if (filtered) {
      logForDebugging(
        `Loaded settings from manifest for plugin ${manifest.name}`,
      )
      return filtered
    }
  }

  return undefined
}

/**
 * Merge two HooksSettings objects
 */
export function mergeHooksSettings(
  base: HooksSettings | undefined,
  additional: HooksSettings,
): HooksSettings {
  if (!base) {
    return additional
  }

  const merged = { ...base }

  for (const [event, matchers] of Object.entries(additional)) {
    if (!merged[event as keyof HooksSettings]) {
      merged[event as keyof HooksSettings] = matchers
    } else {
      merged[event as keyof HooksSettings] = [
        ...(merged[event as keyof HooksSettings] || []),
        ...matchers,
      ]
    }
  }

  return merged
}

/**
 * Type predicate: check if a value is a non-null, non-array object (i.e., a record).
 */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
