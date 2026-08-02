import type { McpbManifestAny as McpbManifest } from '@acosmi-ai/mcpb'
import axios from 'axios'
import { createHash, randomBytes } from 'crypto'
import { basename, dirname, join } from 'path'
import { isDeepStrictEqual } from 'util'
import type { McpServerConfig } from '../../services/mcp/types.js'
import { logForDebugging } from '../debug.js'
import { parseAndValidateManifestFromBytes } from '../dxt/helpers.js'
import { parseZipModes, unzipFile } from '../dxt/zip.js'
import { errorMessage, getErrnoCode, isENOENT, toError } from '../errors.js'
import { getFsImplementation } from '../fsOperations.js'
import { lock } from '../lockfile.js'
import { logError } from '../log.js'
import { getSecureStorage } from '../secureStorage/index.js'
import {
  refreshSettingsForSource,
  updateSettingsForSource,
} from '../settings/settings.js'
import { jsonParse, jsonStringify } from '../slowOperations.js'
import { getSystemDirectories } from '../systemDirectories.js'
import { classifyFetchError, logPluginFetch } from './fetchTelemetry.js'
import {
  readCanonicalPluginFileBytes,
  readCanonicalPluginTextFile,
  revalidatePluginPath,
  resolveInternalPluginPath,
  resolvePluginComponentPath,
  writeCanonicalPluginFile,
} from './pluginPathSecurity.js'
/**
 * User configuration values for MCPB
 */
export type UserConfigValues = Record<
  string,
  string | number | boolean | string[]
>

/**
 * User configuration schema from DXT manifest
 */
type McpbUserConfigurationOption = NonNullable<
  McpbManifest['user_config']
>[string]

export type UserConfigSchema = Record<string, McpbUserConfigurationOption>

/**
 * Exact package generation observed by a configuration dialog. A save may
 * only reuse secret values when the source still resolves to this same
 * content, manifest identity, and author schema.
 */
export type McpbConfigSnapshot = {
  contentHash: string
  manifestName: string
  schema: UserConfigSchema
}

/**
 * Result of loading an MCPB file (success case)
 */
export type McpbLoadResult = {
  manifest: McpbManifest
  mcpConfig: McpServerConfig
  extractedPath: string
  contentHash: string
}

/**
 * Result when MCPB needs user configuration
 */
export type McpbNeedsConfigResult = {
  status: 'needs-config'
  manifest: McpbManifest
  extractedPath: string
  contentHash: string
  configSchema: UserConfigSchema
  existingConfig: UserConfigValues
  existingSecureKeys: string[]
  validationErrors: string[]
}

/**
 * Metadata stored for each cached MCPB
 */
export type McpbCacheMetadata = {
  source: string
  contentHash: string
  extractedPath: string
  cachedAt: string
  lastChecked: string
}

type McpbCacheMaterialization = {
  manifest: McpbManifest
  metadata: McpbCacheMetadata
}

/**
 * Progress callback for download and extraction operations
 */
export type ProgressCallback = (status: string) => void

/**
 * Check if a source string is an MCPB file reference
 */
export function isMcpbSource(source: string): boolean {
  return source.endsWith('.mcpb') || source.endsWith('.dxt')
}

/**
 * Check if a source is a URL
 */
function isUrl(source: string): boolean {
  return source.startsWith('http://') || source.startsWith('https://')
}

/**
 * Generate content hash for an MCPB file
 */
function generateContentHash(data: Uint8Array): string {
  return createHash('sha256').update(data).digest('hex')
}

/**
 * Get cache directory for MCPB files
 */
async function getMcpbCacheDir(pluginPath: string): Promise<string> {
  return resolvePluginComponentPath(pluginPath, '.mcpb-cache', {
    mustExist: false,
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'MCPB cache directory',
  })
}

/**
 * Get metadata file path for cached MCPB
 */
export function getMcpbSourceCacheKey(source: string): string {
  return createHash('sha256').update(source).digest('hex')
}

function getMetadataPath(cacheDir: string, source: string): string {
  return join(cacheDir, `${getMcpbSourceCacheKey(source)}.metadata.json`)
}

/**
 * One in-process cache-materialization flight per canonical plugin cache and
 * exact source. User configuration is deliberately outside this Promise:
 * concurrent callers may be opening different configuration flows, but they
 * must still share one download/extraction generation.
 */
const mcpbCacheMaterializationFlights = new Map<
  string,
  Promise<McpbCacheMaterialization>
>()

function cacheMaterializationKey(cacheDir: string, source: string): string {
  return `${cacheDir}\0${source}`
}

function temporaryCachePath(finalPath: string): string {
  return `${finalPath}.tmp.${process.pid}.${Date.now()}.${randomBytes(6).toString('hex')}`
}

/** Publish a cache file through an exclusive same-directory temporary file. */
async function atomicWriteMcpbCacheFile(
  cacheDir: string,
  finalPath: string,
  data: string | Uint8Array,
  component: string,
  assertBeforeCommit?: () => void,
): Promise<void> {
  const fs = getFsImplementation()
  finalPath = await resolveInternalPluginPath(cacheDir, finalPath, {
    mustExist: false,
    rejectSymlinks: true,
    rejectRoot: true,
    component,
  })
  const tempPath = await resolveInternalPluginPath(
    cacheDir,
    temporaryCachePath(finalPath),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: `${component} temporary file`,
    },
  )
  try {
    await writeCanonicalPluginFile(
      cacheDir,
      tempPath,
      data,
      `${component} temporary file`,
      { exclusive: true },
    )
    await revalidatePluginPath(
      cacheDir,
      dirname(finalPath),
      `${component} directory`,
    )
    assertBeforeCommit?.()
    // The rename is the publication point. Metadata is published last, so a
    // reader can never observe metadata that names a partial generation.
    await fs.rename(tempPath, finalPath)
  } catch (error) {
    await fs.rm(tempPath, { force: true }).catch(() => {})
    throw error
  }
}

/**
 * Compose the secureStorage key for a per-server secret bucket.
 * `pluginSecrets` is a flat map — per-server secrets share it with top-level
 * plugin options (pluginOptionsStorage.ts) using a `${pluginId}/${server}`
 * composite key. `/` can't appear in plugin IDs (`name@marketplace`) or
 * server names (MCP identifier constraints), so it's unambiguous. Keeps the
 * SecureStorageData schema unchanged and the single-keychain-entry size
 * budget (~2KB stdin-safe, see INC-3028) shared across all plugin secrets.
 */
function serverSecretsKey(pluginId: string, serverName: string): string {
  return `${pluginId}/${serverName}`
}

/**
 * Load user configuration for an MCP server, merging non-sensitive values
 * (from settings.json) with sensitive values (from secureStorage keychain).
 * secureStorage wins on collision — schema determines destination so
 * collision shouldn't happen, but if a user hand-edits settings.json we
 * trust the more secure source.
 *
 * Returns null only if NEITHER source has anything — callers skip
 * ${user_config.X} substitution in that case.
 *
 * @param pluginId - Plugin identifier in "plugin@marketplace" format
 * @param serverName - MCP server name from DXT manifest
 */
export function loadMcpServerUserConfig(
  pluginId: string,
  serverName: string,
): UserConfigValues | null {
  return loadMcpServerUserConfigWithProvenance(pluginId, serverName).values
}

export type McpServerUserConfigSnapshot = {
  values: UserConfigValues | null
  storedSensitiveKeys: ReadonlySet<string>
}

/** Read values and their secure-storage provenance from one keychain snapshot. */
export function loadMcpServerUserConfigWithProvenance(
  pluginId: string,
  serverName: string,
): McpServerUserConfigSnapshot {
  try {
    // Plugin-authored MCP configuration is an execution input. Read only the
    // user's own fresh settings snapshot: project/local/policy layers must not
    // inject an endpoint or other non-secret value next to a user-owned secret.
    const settings = refreshSettingsForSource('userSettings')
    const nonSensitive =
      settings?.pluginConfigs?.[pluginId]?.mcpServers?.[serverName]

    // A synchronous read remains appropriate here: runtime measurement
    // confirmed this MCP config read
    // surface did not appear in the login hot path; observed keychain cold
    // misses in that flow were ~12-19ms, not the user-visible 2-3s bill.
    // Migrating this read would still cascade mcpPluginIntegration's sync
    // public APIs and loadMcpbFile's cached path, so keep it synchronous until
    // a future trace proves a plugin-load hot-path cost.
    const allSensitive = getSecureStorage().read()?.pluginSecrets as
      | Record<string, UserConfigValues>
      | undefined
    const sensitive = allSensitive?.[serverSecretsKey(pluginId, serverName)]

    if (!nonSensitive && !sensitive) {
      return { values: null, storedSensitiveKeys: new Set() }
    }

    logForDebugging(
      `Loaded user config for ${pluginId}/${serverName} (settings + secureStorage)`,
    )
    return {
      values: { ...nonSensitive, ...sensitive },
      storedSensitiveKeys: new Set(Object.keys(sensitive ?? {})),
    }
  } catch (error) {
    const errorObj = toError(error)
    logError(errorObj)
    logForDebugging(
      `Failed to load user config for ${pluginId}/${serverName}: ${error}`,
      { level: 'error' },
    )
    return { values: null, storedSensitiveKeys: new Set() }
  }
}

/**
 * Save user configuration for an MCP server, splitting by `schema[key].sensitive`.
 * Mirrors savePluginOptions (pluginOptionsStorage.ts:90) for top-level options:
 *   - `sensitive: true` → secureStorage (keychain on macOS, .credentials.json 0600 elsewhere)
 *   - everything else   → settings.json pluginConfigs[pluginId].mcpServers[serverName]
 *
 * Without this split, per-channel `sensitive: true` was a false sense of
 * security — the dialog masked the input but the save went to plaintext
 * settings.json anyway. H1 #3617646 (Telegram/Discord bot tokens in
 * world-readable .env) surfaced this as the gap to close.
 *
 * Writes are skipped if nothing in that category is present.
 *
 * @param pluginId - Plugin identifier in "plugin@marketplace" format
 * @param serverName - MCP server name from DXT manifest
 * @param config - User configuration values
 * @param schema - The userConfig schema for this server (manifest.user_config
 *   or channels[].userConfig) — drives the sensitive/non-sensitive split
 */
export async function saveMcpServerUserConfig(
  pluginId: string,
  serverName: string,
  config: UserConfigValues,
  schema: UserConfigSchema,
): Promise<void> {
  try {
    const nonSensitive: UserConfigValues = {}
    const sensitive: UserConfigValues = {}

    for (const [key, value] of Object.entries(config)) {
      if (schema[key]?.sensitive === true) {
        sensitive[key] = value
      } else {
        nonSensitive[key] = value
      }
    }

    // Scrub ONLY keys we're writing in this call. Covers both directions
    // across schema-version flips:
    //  - sensitive→secureStorage ⇒ remove stale plaintext from settings.json
    //  - nonSensitive→settings.json ⇒ remove stale entry from secureStorage
    //    (otherwise loadMcpServerUserConfig's {...nonSensitive, ...sensitive}
    //    would let the stale secureStorage value win on next read)
    // Partial `config` (user only re-enters one field) leaves other fields
    // untouched in BOTH stores — defense-in-depth against future callers.
    const sensitiveKeysInThisSave = new Set(Object.keys(sensitive))
    const nonSensitiveKeysInThisSave = new Set(Object.keys(nonSensitive))

    // Sensitive → secureStorage FIRST. If this fails (keychain locked,
    // .credentials.json perms), throw before touching settings.json — the
    // old plaintext stays as a fallback instead of losing BOTH copies.
    //
    // Also scrub non-sensitive keys from secureStorage — schema flipped
    // sensitive→false and they're being written to settings.json now. Without
    // this, loadMcpServerUserConfig's merge would let the stale secureStorage
    // value win on next read.
    //
    // T6-P1-AUTH-CALLERS — `pluginSecrets[<plugin/server>]` is a SUBSET write
    // of the shared credential record → mutateAsync (RFC §2.2): the
    // read-modify-write runs inside a process-level serialized critical
    // section, so two servers configured concurrently can't clobber each
    // other's bucket (lost update). The readAsync() peek only decides whether
    // to enter the section (a non-sensitive-only save skips the keychain
    // write); the mutator recomputes the scrub from the in-section `current`.
    const storage = getSecureStorage()
    const k = serverSecretsKey(pluginId, serverName)
    let needSecureWrite = Object.keys(sensitive).length > 0
    if (!needSecureWrite) {
      // Scrub-only path: a key flipped sensitive→non-sensitive needs its stale
      // value pulled from secureStorage. Peek to skip the keychain write when
      // there is nothing to scrub.
      const peek = (await storage.readAsync())?.pluginSecrets as
        | Record<string, UserConfigValues>
        | undefined
      const existingForKey = peek?.[k]
      if (existingForKey) {
        needSecureWrite = Object.keys(existingForKey).some(key =>
          nonSensitiveKeysInThisSave.has(key),
        )
      }
    }
    if (needSecureWrite) {
      // mutateAsync runs the mutator exactly once inside the critical section,
      // so capturing the scrub count via this closure for the log below is safe.
      let scrubbedKeyCount = 0
      const result = await storage.mutateAsync(current => {
        const allSecrets =
          (current.pluginSecrets as
            | Record<string, UserConfigValues>
            | undefined) ?? {}
        const existingForKey = allSecrets[k]
        const secureScrubbed = existingForKey
          ? Object.fromEntries(
              Object.entries(existingForKey).filter(
                ([key]) => !nonSensitiveKeysInThisSave.has(key),
              ),
            )
          : {}
        if (existingForKey) {
          scrubbedKeyCount =
            Object.keys(existingForKey).length -
            Object.keys(secureScrubbed).length
        }
        // secureStorage keyvault is a flat object — direct replace, no merge
        // semantics to worry about (unlike settings.json's mergeWith).
        return {
          ...current,
          pluginSecrets: {
            ...allSecrets,
            [k]: { ...secureScrubbed, ...sensitive },
          },
        }
      })
      if (!result.success) {
        throw new Error(
          `Failed to save sensitive config to secure storage for ${k}`,
        )
      }
      if (result.warning) {
        logForDebugging(`Server secrets save warning: ${result.warning}`, {
          level: 'warn',
        })
      }
      if (scrubbedKeyCount > 0) {
        logForDebugging(
          `saveMcpServerUserConfig: scrubbed ${scrubbedKeyCount} stale non-sensitive key(s) from secureStorage for ${k}`,
        )
      }
    }

    // Non-sensitive → settings.json. Write whenever there are new non-sensitive
    // values OR existing plaintext sensitive values to scrub — so reconfiguring
    // a sensitive-only schema still cleans up the old settings.json. Runs
    // AFTER the secureStorage write succeeded, so the scrub can't leave you
    // with zero copies of the secret.
    //
    // updateSettingsForSource does mergeWith(diskSettings, ourSettings, ...)
    // which PRESERVES destination keys absent from source — so simply omitting
    // sensitive keys doesn't scrub them, the disk copy merges back in. Instead:
    // set each sensitive key to explicit `undefined` — mergeWith (with the
    // customizer at settings.ts:349) treats explicit undefined as a delete.
    // Scrub decisions come from a fresh raw userSettings snapshot, never the
    // merged effective settings. Otherwise a repository-controlled project
    // sibling could be copied into the user's persistent trust store.
    const settings = refreshSettingsForSource('userSettings')
    const existingInSettings =
      settings?.pluginConfigs?.[pluginId]?.mcpServers?.[serverName] ?? {}
    const keysToScrubFromSettings = Object.keys(existingInSettings).filter(k =>
      sensitiveKeysInThisSave.has(k),
    )
    if (
      Object.keys(nonSensitive).length > 0 ||
      keysToScrubFromSettings.length > 0
    ) {
      // Build the scrub-via-undefined map. The UserConfigValues type doesn't
      // include undefined, but updateSettingsForSource's mergeWith customizer
      // needs explicit undefined to delete — cast is deliberate internal
      // plumbing (same rationale as deletePluginOptions in
      // pluginOptionsStorage.ts:184, see CRABCODE.md's 10% case).
      const scrubbed = Object.fromEntries(
        keysToScrubFromSettings.map(k => [k, undefined]),
      ) as Record<string, undefined>
      // Persist only the leaf patch. updateSettingsForSource takes its own
      // cross-process lock and merges this into a fresh disk snapshot; sending
      // the whole previously-read object would reintroduce a lost-update and
      // scope-promotion boundary.
      const result = updateSettingsForSource('userSettings', {
        pluginConfigs: {
          [pluginId]: {
            mcpServers: {
              [serverName]: {
                ...nonSensitive,
                ...scrubbed,
              } as UserConfigValues,
            },
          },
        },
      })
      if (result.error) {
        throw result.error
      }
      if (keysToScrubFromSettings.length > 0) {
        logForDebugging(
          `saveMcpServerUserConfig: scrubbed ${keysToScrubFromSettings.length} plaintext sensitive key(s) from settings.json for ${pluginId}/${serverName}`,
        )
      }
    }

    logForDebugging(
      `Saved user config for ${pluginId}/${serverName} (${Object.keys(nonSensitive).length} non-sensitive, ${Object.keys(sensitive).length} sensitive)`,
    )
  } catch (error) {
    const errorObj = toError(error)
    logError(errorObj)
    throw new Error(
      `Failed to save user configuration for ${pluginId}/${serverName}: ${errorObj.message}`,
    )
  }
}

/**
 * Validate user configuration values against DXT user_config schema
 */
export function validateUserConfig(
  values: UserConfigValues,
  schema: UserConfigSchema,
): { valid: boolean; errors: string[] } {
  const errors: string[] = []

  // Check each field in the schema
  for (const [key, fieldSchema] of Object.entries(schema)) {
    const value = values[key]

    // Check required fields
    if (
      fieldSchema.required &&
      (value === undefined ||
        value === '' ||
        (Array.isArray(value) && value.length === 0))
    ) {
      errors.push(`${fieldSchema.title || key} is required but not provided`)
      continue
    }

    // Skip validation for optional fields that aren't provided
    if (value === undefined || value === '') {
      continue
    }

    // Type validation
    if (fieldSchema.type === 'string') {
      if (Array.isArray(value)) {
        // String arrays are allowed if multiple: true
        if (!fieldSchema.multiple) {
          errors.push(
            `${fieldSchema.title || key} must be a string, not an array`,
          )
        } else if (!value.every(v => typeof v === 'string')) {
          errors.push(`${fieldSchema.title || key} must be an array of strings`)
        }
      } else if (typeof value !== 'string') {
        errors.push(`${fieldSchema.title || key} must be a string`)
      }
    } else if (fieldSchema.type === 'number' && typeof value !== 'number') {
      errors.push(`${fieldSchema.title || key} must be a number`)
    } else if (fieldSchema.type === 'boolean' && typeof value !== 'boolean') {
      errors.push(`${fieldSchema.title || key} must be a boolean`)
    } else if (
      (fieldSchema.type === 'file' || fieldSchema.type === 'directory') &&
      typeof value !== 'string'
    ) {
      errors.push(`${fieldSchema.title || key} must be a path string`)
    }

    // Number range validation
    if (fieldSchema.type === 'number' && typeof value === 'number') {
      if (fieldSchema.min !== undefined && value < fieldSchema.min) {
        errors.push(
          `${fieldSchema.title || key} must be at least ${fieldSchema.min}`,
        )
      }
      if (fieldSchema.max !== undefined && value > fieldSchema.max) {
        errors.push(
          `${fieldSchema.title || key} must be at most ${fieldSchema.max}`,
        )
      }
    }
  }

  return { valid: errors.length === 0, errors }
}

/**
 * Generate MCP server configuration from DXT manifest
 */
async function generateMcpConfig(
  manifest: McpbManifest,
  extractedPath: string,
  userConfig: UserConfigValues = {},
): Promise<McpServerConfig> {
  // Lazy import: @acosmi-ai/mcpb barrel pulls in zod v3 schemas (~700KB of
  // bound closures). See dxt/helpers.ts for details.
  const { getMcpConfigForManifest } = await import('@acosmi-ai/mcpb')
  const mcpConfig = await getMcpConfigForManifest({
    manifest,
    extensionPath: extractedPath,
    systemDirs: getSystemDirectories(),
    userConfig,
    pathSeparator: '/',
  })

  if (!mcpConfig) {
    const error = new Error(
      `Failed to generate MCP server configuration from manifest "${manifest.name}"`,
    )
    logError(error)
    throw error
  }

  return mcpConfig as McpServerConfig
}

/**
 * Load cache metadata for an MCPB source
 */
async function loadCacheMetadata(
  cacheDir: string,
  source: string,
  pluginPath: string,
): Promise<McpbCacheMetadata | null> {
  cacheDir = await resolveInternalPluginPath(pluginPath, cacheDir, {
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'MCPB cache directory',
  })
  const metadataPath = await resolveInternalPluginPath(
    cacheDir,
    getMetadataPath(cacheDir, source),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB cache metadata',
    },
  )

  try {
    const content = await readCanonicalPluginTextFile(
      cacheDir,
      metadataPath,
      'MCPB cache metadata',
    )
    const parsed = jsonParse(content) as unknown
    if (!parsed || typeof parsed !== 'object') return null
    const metadata = parsed as McpbCacheMetadata
    if (
      typeof metadata.source !== 'string' ||
      typeof metadata.contentHash !== 'string' ||
      typeof metadata.extractedPath !== 'string' ||
      typeof metadata.cachedAt !== 'string' ||
      typeof metadata.lastChecked !== 'string'
    ) {
      return null
    }
    const sourceKey = getMcpbSourceCacheKey(source)
    const expectedExtractionNames = new Set([
      metadata.contentHash,
      `${sourceKey}-${metadata.contentHash}`,
    ])
    if (
      metadata.source !== source ||
      !/^[0-9a-f]{64}$/.test(metadata.contentHash) ||
      !expectedExtractionNames.has(basename(metadata.extractedPath)) ||
      !Number.isFinite(Date.parse(metadata.cachedAt)) ||
      !Number.isFinite(Date.parse(metadata.lastChecked))
    ) {
      logForDebugging(
        'Rejected incoherent MCPB cache metadata (source/hash/path generation mismatch)',
        { level: 'warn' },
      )
      return null
    }
    metadata.extractedPath = await resolveInternalPluginPath(
      cacheDir,
      metadata.extractedPath,
      {
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'MCPB cached extraction',
      },
    )
    return metadata
  } catch (error) {
    const code = getErrnoCode(error)
    if (code === 'ENOENT') return null
    const errorObj = toError(error)
    logError(errorObj)
    logForDebugging(`Failed to load MCPB cache metadata: ${error}`, {
      level: 'error',
    })
    return null
  }
}

/**
 * Save cache metadata for an MCPB source
 */
async function saveCacheMetadata(
  cacheDir: string,
  source: string,
  metadata: McpbCacheMetadata,
  pluginPath: string,
  assertBeforeCommit?: () => void,
): Promise<void> {
  cacheDir = await resolveInternalPluginPath(pluginPath, cacheDir, {
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'MCPB cache directory',
  })
  metadata.extractedPath = await resolveInternalPluginPath(
    cacheDir,
    metadata.extractedPath,
    {
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB cached extraction',
    },
  )
  const metadataPath = await resolveInternalPluginPath(
    cacheDir,
    getMetadataPath(cacheDir, source),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB cache metadata',
    },
  )

  await getFsImplementation().mkdir(cacheDir)
  await atomicWriteMcpbCacheFile(
    cacheDir,
    metadataPath,
    jsonStringify(metadata, null, 2),
    'MCPB cache metadata',
    assertBeforeCommit,
  )
}

/**
 * Download MCPB file from URL
 */
async function downloadMcpb(
  url: string,
  onProgress?: ProgressCallback,
): Promise<Uint8Array> {
  logForDebugging(`Downloading MCPB from ${url}`)
  if (onProgress) {
    onProgress(`Downloading ${url}...`)
  }

  const started = performance.now()
  let fetchTelemetryFired = false
  try {
    const response = await axios.get(url, {
      timeout: 120000, // 2 minute timeout
      responseType: 'arraybuffer',
      maxRedirects: 5, // Follow redirects (like curl -L)
      onDownloadProgress: progressEvent => {
        if (progressEvent.total && onProgress) {
          const percent = Math.round(
            (progressEvent.loaded / progressEvent.total) * 100,
          )
          onProgress(`Downloading... ${percent}%`)
        }
      },
    })

    const data = new Uint8Array(response.data)
    // Fire telemetry before writeFile — the event measures the network
    // fetch, not disk I/O. A writeFile EACCES would otherwise match
    // classifyFetchError's /permission denied/ → misreport as auth.
    logPluginFetch('mcpb', url, 'success', performance.now() - started)
    fetchTelemetryFired = true

    logForDebugging(`Downloaded ${data.length} bytes from ${url}`)
    if (onProgress) {
      onProgress('Download complete')
    }

    return data
  } catch (error) {
    if (!fetchTelemetryFired) {
      logPluginFetch(
        'mcpb',
        url,
        'failure',
        performance.now() - started,
        classifyFetchError(error),
      )
    }
    const errorMsg = errorMessage(error)
    const fullError = new Error(
      `Failed to download MCPB file from ${url}: ${errorMsg}`,
    )
    logError(fullError)
    throw fullError
  }
}

/**
 * Extract MCPB file and write contents to extraction directory.
 *
 * @param modes - name→mode map from `parseZipModes`. MCPB bundles can ship
 *   native MCP server binaries, so preserving the exec bit matters here.
 */
export async function extractMcpbContents(
  unzipped: Record<string, Uint8Array>,
  extractPath: string,
  modes: Record<string, number>,
  onProgress?: ProgressCallback,
  containmentRoot: string = dirname(extractPath),
): Promise<void> {
  if (onProgress) {
    onProgress('Extracting files...')
  }

  const safeExtractPath = await resolveInternalPluginPath(
    containmentRoot,
    extractPath,
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB extraction directory',
    },
  )
  await getFsImplementation().mkdir(safeExtractPath)
  await revalidatePluginPath(
    containmentRoot,
    safeExtractPath,
    'MCPB extraction directory',
  )

  // Write all files. Filter directory entries from the count so progress
  // messages use the same denominator as filesWritten (which skips them).
  let filesWritten = 0
  const entries = Object.entries(unzipped).filter(([k]) => !k.endsWith('/'))
  const totalFiles = entries.length

  for (const [filePath, fileData] of entries) {
    // Directory entries (common in zip -r, Python zipfile, Java ZipOutputStream)
    // are filtered above — writeFile would create `bin/` as an empty regular
    // file, then mkdir for `bin/server` would fail with ENOTDIR. The
    // mkdir(dirname(fullPath)) below creates parent dirs implicitly.

    const activeExtractRoot = await resolveInternalPluginPath(
      containmentRoot,
      safeExtractPath,
      {
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'MCPB extraction directory',
      },
    )
    const fullPath = await resolvePluginComponentPath(
      activeExtractRoot,
      filePath,
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'MCPB archive entry',
      },
    )
    const dir = dirname(fullPath)

    // Ensure directory exists (recursive handles already-existing)
    if (dir !== activeExtractRoot) {
      await getFsImplementation().mkdir(dir)
      await revalidatePluginPath(
        activeExtractRoot,
        dir,
        'MCPB archive directory',
      )
    }

    const mode = modes[filePath]
    const executableMode = mode && mode & 0o111 ? mode & 0o777 : undefined
    await writeCanonicalPluginFile(
      activeExtractRoot,
      fullPath,
      fileData,
      'MCPB archive entry',
      {
        mode: executableMode,
        bestEffortMode: executableMode !== undefined,
      },
    )

    filesWritten++
    if (onProgress && filesWritten % 10 === 0) {
      onProgress(`Extracted ${filesWritten}/${totalFiles} files`)
    }
  }

  logForDebugging(`Extracted ${filesWritten} files to ${safeExtractPath}`)
  if (onProgress) {
    onProgress(`Extraction complete (${filesWritten} files)`)
  }
}

async function cachedMcpbChanged(
  metadata: McpbCacheMetadata,
  source: string,
  pluginPath: string,
): Promise<boolean> {
  const fs = getFsImplementation()
  try {
    const extraction = await fs.stat(metadata.extractedPath)
    if (!extraction.isDirectory()) return true
  } catch (error) {
    logForDebugging(
      `MCPB extraction path is unavailable: ${metadata.extractedPath}: ${error}`,
      { level: getErrnoCode(error) === 'ENOENT' ? 'warn' : 'error' },
    )
    return true
  }

  // URL packages remain pinned until an explicit update invalidates their
  // metadata. Local packages retain the historical mtime freshness check.
  if (isUrl(source)) return false

  let localPath: string
  try {
    localPath = await resolvePluginComponentPath(pluginPath, source, {
      component: 'MCPB source',
    })
  } catch (error) {
    logForDebugging(`MCPB source path rejected: ${source}: ${error}`, {
      level: 'error',
    })
    return true
  }
  try {
    const stats = await fs.stat(localPath)
    const cachedTime = Date.parse(metadata.cachedAt)
    const fileTime = Math.floor(stats.mtimeMs)
    if (fileTime > cachedTime) {
      logForDebugging(
        `MCPB file modified: ${new Date(fileTime)} > ${new Date(cachedTime)}`,
      )
      return true
    }
    return false
  } catch (error) {
    logForDebugging(`MCPB source file unavailable: ${localPath}: ${error}`, {
      level: getErrnoCode(error) === 'ENOENT' ? 'warn' : 'error',
    })
    return true
  }
}

async function readCachedMcpbManifest(
  cacheDir: string,
  metadata: McpbCacheMetadata,
): Promise<McpbManifest> {
  const manifestPath = await resolveInternalPluginPath(
    cacheDir,
    join(metadata.extractedPath, 'manifest.json'),
    { component: 'cached MCPB manifest' },
  )
  const manifestContent = await readCanonicalPluginTextFile(
    cacheDir,
    manifestPath,
    'cached MCPB manifest',
  )
  const manifest = await parseAndValidateManifestFromBytes(
    new TextEncoder().encode(manifestContent),
  )
  if (!manifest.server) {
    throw new Error(
      `MCPB manifest for "${manifest.name}" does not define a server configuration`,
    )
  }
  return manifest
}

async function withMcpbCacheLock<T>(
  cacheDir: string,
  source: string,
  operation: (assertLease: () => void) => Promise<T>,
): Promise<T> {
  const sourceKey = getMcpbSourceCacheKey(source)
  const lockTarget = await resolveInternalPluginPath(
    cacheDir,
    join(cacheDir, `${sourceKey}.cache-transaction`),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB cache transaction lock target',
    },
  )
  const lockfilePath = await resolveInternalPluginPath(
    cacheDir,
    `${lockTarget}.lock`,
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB cache transaction lock',
    },
  )
  let compromised: Error | null = null
  const release = await lock(lockTarget, {
    realpath: false,
    lockfilePath,
    retries: {
      retries: 600,
      factor: 1.1,
      minTimeout: 20,
      maxTimeout: 500,
      randomize: true,
    },
    // A download alone may legitimately take 120s. proper-lockfile refreshes
    // this lease periodically; a crashed process is reclaimed after 3m.
    stale: 180_000,
    update: 30_000,
    onCompromised: error => {
      compromised = error
    },
  })
  const assertLease = (): void => {
    if (compromised) {
      throw new Error(`MCPB cache lock compromised: ${compromised.message}`)
    }
  }
  try {
    assertLease()
    return await operation(assertLease)
  } finally {
    await release().catch(error => {
      logForDebugging(`Failed to release MCPB cache lock: ${error}`, {
        level: 'error',
      })
    })
  }
}

async function committedExtractionIsUsable(
  cacheDir: string,
  extractionPath: string,
): Promise<boolean> {
  const fs = getFsImplementation()
  try {
    if (!(await fs.stat(extractionPath)).isDirectory()) return false
    const manifestPath = await resolveInternalPluginPath(
      cacheDir,
      join(extractionPath, 'manifest.json'),
      { component: 'committed MCPB manifest' },
    )
    const bytes = new TextEncoder().encode(
      await readCanonicalPluginTextFile(
        cacheDir,
        manifestPath,
        'committed MCPB manifest',
      ),
    )
    return Boolean((await parseAndValidateManifestFromBytes(bytes)).server)
  } catch {
    return false
  }
}

async function materializeMcpbCacheUnderLock(
  source: string,
  pluginPath: string,
  cacheDir: string,
  onProgress: ProgressCallback | undefined,
  assertLease: () => void,
): Promise<McpbCacheMaterialization> {
  const fs = getFsImplementation()

  // Lock-after-wait recheck is the cross-process singleflight boundary.
  const cached = await loadCacheMetadata(cacheDir, source, pluginPath)
  if (cached && !(await cachedMcpbChanged(cached, source, pluginPath))) {
    try {
      const manifest = await readCachedMcpbManifest(cacheDir, cached)
      logForDebugging(
        `Using cached MCPB from ${cached.extractedPath} (hash: ${cached.contentHash})`,
      )
      return { manifest, metadata: cached }
    } catch (error) {
      logForDebugging(`Ignoring incomplete MCPB cache generation: ${error}`, {
        level: 'warn',
      })
    }
  }

  let mcpbData: Uint8Array
  if (isUrl(source)) {
    mcpbData = await downloadMcpb(source, onProgress)
  } else {
    const localPath = await resolvePluginComponentPath(pluginPath, source, {
      component: 'MCPB source',
    })
    onProgress?.(`Loading ${source}...`)
    try {
      mcpbData = await readCanonicalPluginFileBytes(
        pluginPath,
        localPath,
        'MCPB source',
      )
    } catch (error) {
      if (isENOENT(error)) {
        throw new Error(`MCPB file not found: ${localPath}`)
      }
      throw error
    }
  }

  const contentHash = generateContentHash(mcpbData)
  const sourceKey = getMcpbSourceCacheKey(source)
  logForDebugging(`MCPB content hash: ${contentHash}`)
  onProgress?.('Extracting MCPB archive...')

  const unzipped = await unzipFile(Buffer.from(mcpbData))
  const modes = parseZipModes(mcpbData)
  const manifestData = unzipped['manifest.json']
  if (!manifestData) throw new Error('No manifest.json found in MCPB file')
  const manifest = await parseAndValidateManifestFromBytes(manifestData)
  if (!manifest.server) {
    throw new Error(
      `MCPB manifest for "${manifest.name}" does not define a server configuration`,
    )
  }
  logForDebugging(
    `MCPB manifest: ${manifest.name} v${manifest.version} by ${manifest.author.name}`,
  )

  // Source-qualified + content-addressed final path makes one immutable cache
  // generation. Extraction happens in an unpublished sibling directory and
  // becomes visible in one rename.
  const extractionPath = await resolveInternalPluginPath(
    cacheDir,
    join(cacheDir, `${sourceKey}-${contentHash}`),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'MCPB extraction generation',
    },
  )
  if (!(await committedExtractionIsUsable(cacheDir, extractionPath))) {
    // A previous crashed writer can leave an unpublished generation behind.
    // It is safe to replace because no coherent metadata points at it.
    await fs.rm(extractionPath, { recursive: true, force: true })
    const stagingPath = await resolveInternalPluginPath(
      cacheDir,
      temporaryCachePath(extractionPath),
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'MCPB extraction staging directory',
      },
    )
    try {
      await extractMcpbContents(
        unzipped,
        stagingPath,
        modes,
        onProgress,
        cacheDir,
      )
      assertLease()
      await revalidatePluginPath(
        cacheDir,
        dirname(extractionPath),
        'MCPB cache directory',
      )
      await fs.rename(stagingPath, extractionPath)
    } catch (error) {
      await fs.rm(stagingPath, { recursive: true, force: true }).catch(() => {})
      throw error
    }
  }

  assertLease()
  if (isUrl(source)) {
    const archivePath = await resolveInternalPluginPath(
      cacheDir,
      join(cacheDir, `${sourceKey}-${contentHash}.mcpb`),
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'downloaded MCPB cache file',
      },
    )
    await atomicWriteMcpbCacheFile(
      cacheDir,
      archivePath,
      mcpbData,
      'downloaded MCPB cache file',
      assertLease,
    )
  }

  assertLease()
  const now = new Date().toISOString()
  const metadata: McpbCacheMetadata = {
    source,
    contentHash,
    extractedPath: extractionPath,
    cachedAt: now,
    lastChecked: now,
  }
  // Metadata is the generation commit point and is itself atomically renamed.
  await saveCacheMetadata(
    cacheDir,
    source,
    metadata,
    pluginPath,
    assertLease,
  )
  return { manifest, metadata }
}

async function ensureMcpbCache(
  source: string,
  pluginPath: string,
  onProgress?: ProgressCallback,
): Promise<McpbCacheMaterialization> {
  const fs = getFsImplementation()
  const cacheDir = await getMcpbCacheDir(pluginPath)
  await fs.mkdir(cacheDir)
  await revalidatePluginPath(pluginPath, cacheDir, 'MCPB cache directory')

  const flightKey = cacheMaterializationKey(cacheDir, source)
  const existing = mcpbCacheMaterializationFlights.get(flightKey)
  if (existing) {
    onProgress?.('Waiting for concurrent MCPB cache initialization...')
    return existing
  }

  const pending = withMcpbCacheLock(
    cacheDir,
    source,
    assertLease =>
      materializeMcpbCacheUnderLock(
        source,
        pluginPath,
        cacheDir,
        onProgress,
        assertLease,
      ),
  )
  mcpbCacheMaterializationFlights.set(flightKey, pending)
  try {
    return await pending
  } finally {
    if (mcpbCacheMaterializationFlights.get(flightKey) === pending) {
      mcpbCacheMaterializationFlights.delete(flightKey)
    }
  }
}

/** Check if an MCPB source has changed and needs re-extraction. */
export async function checkMcpbChanged(
  source: string,
  pluginPath: string,
): Promise<boolean> {
  const fs = getFsImplementation()
  const cacheDir = await getMcpbCacheDir(pluginPath)
  await fs.mkdir(cacheDir)
  await revalidatePluginPath(pluginPath, cacheDir, 'MCPB cache directory')
  const metadata = await loadCacheMetadata(cacheDir, source, pluginPath)
  return !metadata || cachedMcpbChanged(metadata, source, pluginPath)
}

/**
 * Load and extract an MCPB file, with caching and user configuration support.
 * Cache materialization is shared; user configuration remains per caller.
 */
export async function loadMcpbFile(
  source: string,
  pluginPath: string,
  pluginId: string,
  onProgress?: ProgressCallback,
  providedUserConfig?: UserConfigValues,
  forceConfigDialog?: boolean,
  storageNamespace?: string,
  expectedConfigSnapshot?: McpbConfigSnapshot,
  persistedUserConfigPatch?: UserConfigValues,
): Promise<McpbLoadResult | McpbNeedsConfigResult> {
  logForDebugging(`Loading MCPB from source: ${source}`)
  const { manifest, metadata } = await ensureMcpbCache(
    source,
    pluginPath,
    onProgress,
  )
  const storageServerName = storageNamespace
    ? `${storageNamespace}:${manifest.name}`
    : manifest.name
  const currentSchema = manifest.user_config ?? {}

  // Configuration is a two-read flow: the dialog first observes a package
  // generation, then submits values. Never carry an old secret into a changed
  // package merely because its source URL stayed the same. This comparison is
  // inside the second materialization, before validation, storage, or config
  // generation, so same-key sensitivity flips and manifest-name migrations
  // are rejected without writing or reflecting any value.
  if (
    expectedConfigSnapshot &&
    (metadata.contentHash !== expectedConfigSnapshot.contentHash ||
      manifest.name !== expectedConfigSnapshot.manifestName ||
      !isDeepStrictEqual(currentSchema, expectedConfigSnapshot.schema))
  ) {
    return {
      status: 'needs-config',
      manifest,
      extractedPath: metadata.extractedPath,
      contentHash: metadata.contentHash,
      configSchema: currentSchema,
      existingConfig: {},
      existingSecureKeys: [],
      validationErrors: [
        'MCPB package or configuration schema changed while saving',
      ],
    }
  }

  if (Object.keys(currentSchema).length > 0) {
    const savedSnapshot = loadMcpServerUserConfigWithProvenance(
      pluginId,
      storageServerName,
    )
    const savedConfig = savedSnapshot.values
    const userConfig = providedUserConfig ?? savedConfig ?? {}
    const validation = validateUserConfig(userConfig, currentSchema)
    if (forceConfigDialog || !validation.valid) {
      return {
        status: 'needs-config',
        manifest,
        extractedPath: metadata.extractedPath,
        contentHash: metadata.contentHash,
        configSchema: currentSchema,
        existingConfig: savedConfig ?? {},
        existingSecureKeys: [...savedSnapshot.storedSensitiveKeys],
        validationErrors: validation.valid ? [] : validation.errors,
      }
    }

    onProgress?.('Generating MCP server configuration...')
    const mcpConfig = await generateMcpConfig(
      manifest,
      metadata.extractedPath,
      userConfig,
    )
    // Generate first: a package that rejects the merged values must leave
    // persistent configuration untouched. Persist only the explicit patch,
    // never the full merge of old values, so concurrent fields and secrets do
    // not get rewritten under a changed sensitivity classification.
    const valuesToPersist = persistedUserConfigPatch ?? providedUserConfig
    if (valuesToPersist && Object.keys(valuesToPersist).length > 0) {
      await saveMcpServerUserConfig(
        pluginId,
        storageServerName,
        valuesToPersist,
        currentSchema,
      )
    }
    return {
      manifest,
      mcpConfig,
      extractedPath: metadata.extractedPath,
      contentHash: metadata.contentHash,
    }
  }

  onProgress?.('Generating MCP server configuration...')
  const mcpConfig = await generateMcpConfig(
    manifest,
    metadata.extractedPath,
  )
  logForDebugging(
    `Successfully loaded MCPB: ${manifest.name} (extracted to ${metadata.extractedPath})`,
  )
  return {
    manifest,
    mcpConfig,
    extractedPath: metadata.extractedPath,
    contentHash: metadata.contentHash,
  }
}
