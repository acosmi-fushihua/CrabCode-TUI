import { expandEnvVarsInString } from '../../services/mcp/envExpansion.js'
import { getKnownMcpAuthState } from '../../services/mcp/auth.js'
import {
  buildPluginMcpRuntimeName,
  isReservedPluginMcpAuthoredServerName,
  PLUGIN_MCP_REMOTE_PACKAGE_PREFIX,
} from '../../services/mcp/pluginMcpIdentity.js'
import { createHash } from 'node:crypto'
import {
  classifyPluginMcpLifecycle,
  createPluginMcpActivationSnapshotReader,
  getPersistedPluginMcpActivation,
  reconcilePluginMcpActivations,
  type PluginMcpAuthState,
  type PluginMcpConfigState,
  type PluginMcpDependencyState,
  type PluginMcpConfigurationDescriptor,
  type PluginMcpConfigurationValue,
  type PluginMcpInventoryRecord,
  type PluginMcpSourceKind,
} from '../../services/mcp/pluginMcpLifecycle.js'
import { preflightPluginStdio } from '../../services/mcp/stdioPreflight.js'
import {
  type McpServerConfig,
  McpServerConfigSchema,
  McpServerNameSchema,
  type ScopedMcpServerConfig,
} from '../../services/mcp/types.js'
import type { LoadedPlugin, PluginError } from '../../types/plugin.js'
import { logForDebugging } from '../debug.js'
import { errorMessage, isENOENT } from '../errors.js'
import { jsonParse } from '../slowOperations.js'
import {
  isMcpbSource,
  loadMcpbFile,
  loadMcpServerUserConfig,
  loadMcpServerUserConfigWithProvenance,
  type McpbLoadResult,
  type UserConfigSchema,
  type UserConfigValues,
  validateUserConfig,
} from './mcpbHandler.js'
import { getPluginDataDir } from './pluginDirectories.js'
import {
  getPluginStorageId,
  loadPluginOptionsWithProvenance,
  substitutePluginVariables,
  substituteUserConfigVariables,
} from './pluginOptionsStorage.js'
import {
  PluginPathSecurityError,
  readCanonicalPluginTextFile,
  resolvePluginComponentPath,
} from './pluginPathSecurity.js'

export type PluginMcpLoadIssue = {
  serverName: string
  activationKey?: string
  configState: 'requiresConfig' | 'invalid'
  reason: string
  configuration?: PluginMcpConfigurationDescriptor
}

type PluginDataDirResolver = (pluginSource: string) => string

export interface PluginMcpEnvironmentOptions {
  pluginDataDir?: PluginDataDirResolver
}

export interface PluginMcpLoadOptions {
  /** Exact remote MCPB/DXT sources authorized by their own activation row. */
  allowedRemoteMcpbSources?: ReadonlySet<string>
  /** Stable source-bound authorization identity for each remote package. */
  remoteMcpbActivationNames?: ReadonlyMap<string, string>
  /** Candidate logical name declared by channel/required metadata. */
  remoteMcpbDeclaredNames?: ReadonlyMap<string, string>
  /** Output seam populated with the collision-checked logical identity. */
  resolvedRemoteMcpbLogicalNames?: Map<string, string>
}

export interface PluginMcpLifecycleInventoryOptions {
  env?: Readonly<Record<string, string | undefined>>
  readPersistedActivation?: typeof getPersistedPluginMcpActivation
  reconcileActivations?: typeof reconcilePluginMcpActivations
  pluginDataDir?: PluginDataDirResolver
  getKnownAuthState?: typeof getKnownMcpAuthState
}

function isRemoteMcpbSource(source: string): boolean {
  return /^https?:\/\//i.test(source) && isMcpbSource(source)
}

function manifestMcpSpecs(
  plugin: LoadedPlugin,
): readonly (string | Record<string, McpServerConfig>)[] {
  const spec = plugin.manifest.mcpServers
  if (spec === undefined) return []
  return Array.isArray(spec) ? spec : [spec]
}

export function getPluginMcpbSources(plugin: LoadedPlugin): string[] {
  return [
    ...new Set(
      manifestMcpSpecs(plugin).flatMap(spec =>
        typeof spec === 'string' && isMcpbSource(spec) ? [spec] : [],
      ),
    ),
  ]
}

export interface PluginMcpbActivationTarget {
  source: string
  activationKey: string
  logicalCandidates: string[]
}

export interface PluginMcpStaticActivationTarget {
  serverName: string
  activationKey: string
}

/** Static, zero-package-I/O identities used by the activation write path. */
export function getPluginMcpbActivationTargets(
  plugin: LoadedPlugin,
): PluginMcpbActivationTarget[] {
  const sources = getPluginMcpbSources(plugin)
  const declared = [
    ...(plugin.manifest.channels?.map(channel => channel.server) ?? []),
    ...(plugin.manifest.requiredMcpServers ?? []),
  ].filter((name, index, names) => name.length > 0 && names.indexOf(name) === index)
  const soleDeclaredName =
    sources.length === 1 && declared.length === 1 ? declared[0] : undefined
  return sources.map(source => {
    const activationKey = getRemoteMcpbActivationName(source)
    return {
      source,
      activationKey,
      logicalCandidates: [
        activationKey,
        ...(soleDeclaredName ? [soleDeclaredName] : []),
      ],
    }
  })
}

function addStaticActivationTarget(
  targets: Map<string, string>,
  serverName: string,
  activationKey = serverName,
): void {
  if (!McpServerNameSchema().safeParse(serverName).success) return
  if (!McpServerNameSchema().safeParse(activationKey).success) return
  targets.set(serverName, activationKey)
}

function authoredServerNamesFromJson(value: unknown): string[] {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return []
  const record = value as Record<string, unknown>
  const hasWrapper = Object.prototype.hasOwnProperty.call(record, 'mcpServers')
  if (
    hasWrapper &&
    (!record.mcpServers ||
      typeof record.mcpServers !== 'object' ||
      Array.isArray(record.mcpServers))
  ) {
    return []
  }
  const candidate = hasWrapper
    ? (record.mcpServers as Record<string, unknown>)
    : record
  return Object.keys(candidate).filter(name =>
    McpServerNameSchema().safeParse(name).success,
  )
}

async function authoredServerNamesFromFile(
  plugin: LoadedPlugin,
  relativePath: string,
): Promise<string[]> {
  try {
    const filePath = await resolvePluginComponentPath(plugin.path, relativePath, {
      component: 'MCP activation identity source',
    })
    const content = await readCanonicalPluginTextFile(
      plugin.path,
      filePath,
      'MCP activation identity source',
    )
    return authoredServerNamesFromJson(jsonParse(content) as unknown)
  } catch {
    // A missing, malformed, or containment-invalid file does not authorize an
    // activation write. The lifecycle inventory may still show its diagnostic,
    // but only an authored server key can become a persistence identity.
    return []
  }
}

/**
 * Enumerate activation identities without validating or executing MCP config.
 *
 * This is the write-boundary counterpart to lifecycle inventory: it reads only
 * cache-only installed manifests and canonical local JSON object keys. It never
 * downloads/extracts an MCPB, expands variables, probes auth/dependencies, or
 * spawns a server. Invalid config payloads therefore remain non-executable,
 * while their valid authored key can still be disabled from a management UI.
 */
export async function getPluginMcpStaticActivationTargets(
  plugin: LoadedPlugin,
): Promise<PluginMcpStaticActivationTarget[]> {
  const targets = new Map<string, string>()
  const localServerNames = new Set<string>()

  for (const name of plugin.manifest.requiredMcpServers ?? []) {
    addStaticActivationTarget(targets, name)
  }
  for (const channel of plugin.manifest.channels ?? []) {
    addStaticActivationTarget(targets, channel.server)
  }

  const localPaths = new Set<string>(['.mcp.json'])
  for (const spec of manifestMcpSpecs(plugin)) {
    if (typeof spec === 'string') {
      if (!isMcpbSource(spec)) localPaths.add(spec)
      continue
    }
    for (const name of Object.keys(spec)) {
      if (McpServerNameSchema().safeParse(name).success) {
        localServerNames.add(name)
      }
      addStaticActivationTarget(targets, name)
    }
  }

  const localNames = await Promise.all(
    [...localPaths].map(path => authoredServerNamesFromFile(plugin, path)),
  )
  for (const names of localNames) {
    for (const name of names) {
      localServerNames.add(name)
      addStaticActivationTarget(targets, name)
    }
  }

  // MCPB identities are derived solely from the authored package source and,
  // when unambiguous, its single declared channel/required alias. No package
  // materialization is permitted on this activation-only path.
  for (const target of getPluginMcpbActivationTargets(plugin)) {
    for (const candidate of target.logicalCandidates) {
      // The source-hash activation key deliberately uses the reserved remote
      // package namespace, so validate only the presentation candidate here.
      if (
        candidate === target.activationKey ||
        McpServerNameSchema().safeParse(candidate).success
      ) {
        // Runtime loading gives an authored local server ownership of its
        // logical name and moves the remote package to its source hash. Static
        // activation must mirror that exact collision rule: a channel alias
        // may point at the remote package only when no local source owns it.
        if (
          candidate !== target.activationKey &&
          localServerNames.has(candidate)
        ) {
          continue
        }
        targets.set(candidate, target.activationKey)
      }
    }
  }

  return [...targets].map(([serverName, activationKey]) => ({
    serverName,
    activationKey,
  }))
}

/** Bind MCPB configuration and secrets to the exact package source. */
export function getPluginMcpbStorageNamespace(source: string): string {
  return `mcpb-${createHash('sha256').update(source).digest('hex')}`
}

export function getRemoteMcpbActivationName(source: string): string {
  const hash = createHash('sha256').update(source).digest('hex')
  return `${PLUGIN_MCP_REMOTE_PACKAGE_PREFIX}${hash}`
}

type RemoteMcpbEntry = {
  source: string
  activationName: string
  declaredName?: string
}

function remoteMcpbEntries(plugin: LoadedPlugin): RemoteMcpbEntry[] {
  return getPluginMcpbActivationTargets(plugin).map(target => ({
    source: target.source,
    activationName: target.activationKey,
    ...(target.logicalCandidates[1]
      ? { declaredName: target.logicalCandidates[1] }
      : {}),
  }))
}

function rejectReservedAuthoredServerNames(
  plugin: LoadedPlugin,
  authored: Record<string, McpServerConfig>,
  errors: PluginError[],
): Record<string, McpServerConfig> {
  const accepted: Record<string, McpServerConfig> = {}
  for (const [serverName, config] of Object.entries(authored)) {
    if (!McpServerNameSchema().safeParse(serverName).success) {
      errors.push({
        type: 'mcp-config-invalid',
        source: plugin.source,
        plugin: plugin.name,
        serverName,
        validationError:
          'Plugin MCP server name is empty or uses a reserved runtime namespace',
      })
      continue
    }
    accepted[serverName] = config
  }
  return accepted
}

function inlineManifestMcpServers(
  plugin: LoadedPlugin,
  errors: PluginError[],
): Record<string, McpServerConfig> | undefined {
  const inline: Record<string, McpServerConfig> = {}
  for (const spec of manifestMcpSpecs(plugin)) {
    if (typeof spec === 'string' || Array.isArray(spec)) continue
    Object.assign(inline, rejectReservedAuthoredServerNames(plugin, spec, errors))
  }
  return Object.keys(inline).length > 0 ? inline : undefined
}

function addRemoteMcpbIssue(
  serverName: string,
  lifecycleIssues: PluginMcpLoadIssue[],
  configState: PluginMcpLoadIssue['configState'],
  reason: string,
  activationKey = serverName,
  configuration?: PluginMcpConfigurationDescriptor,
): void {
  if (lifecycleIssues.some(issue => issue.serverName === serverName)) return
  lifecycleIssues.push({
    serverName,
    activationKey,
    configState,
    reason,
    ...(configuration ? { configuration } : {}),
  })
}

function resolveRemoteMcpbLogicalName(
  source: string,
  localServerNames: ReadonlySet<string>,
  options: PluginMcpLoadOptions,
): { logicalName: string; activationKey: string; expectedName?: string } {
  const activationKey =
    options.remoteMcpbActivationNames?.get(source) ??
    getRemoteMcpbActivationName(source)
  const declaredName = options.remoteMcpbDeclaredNames?.get(source)
  const logicalName =
    declaredName && !localServerNames.has(declaredName)
      ? declaredName
      : activationKey
  options.resolvedRemoteMcpbLogicalNames?.set(source, logicalName)
  return {
    logicalName,
    activationKey,
    ...(logicalName === declaredName ? { expectedName: declaredName } : {}),
  }
}

/**
 * Load MCP servers from an MCPB file
 * Handles downloading, extracting, and converting DXT manifest to MCP config
 */
async function loadMcpServersFromMcpb(
  plugin: LoadedPlugin,
  mcpbPath: string,
  errors: PluginError[],
  lifecycleIssues: PluginMcpLoadIssue[],
  logicalServerName?: string,
  expectedManifestName?: string,
  activationKey = logicalServerName,
): Promise<Record<string, McpServerConfig> | null> {
  try {
    logForDebugging(`Loading MCP servers from MCPB: ${mcpbPath}`)

    // Use plugin.repository directly - it's already in "plugin@marketplace" format
    const pluginId = getPluginStorageId(plugin)

    const result = await loadMcpbFile(
      mcpbPath,
      plugin.path,
      pluginId,
      status => {
        logForDebugging(`MCPB [${plugin.name}]: ${status}`)
      },
      undefined,
      undefined,
      getPluginMcpbStorageNamespace(mcpbPath),
    )

    if (!McpServerNameSchema().safeParse(result.manifest.name).success) {
      const invalidName = logicalServerName ?? result.manifest.name
      const validationError =
        'MCPB manifest name is empty, reserved, or too large'
      errors.push({
        type: 'mcp-config-invalid',
        source: plugin.source,
        plugin: plugin.name,
        serverName: invalidName,
        validationError,
      })
      if (logicalServerName) {
        addRemoteMcpbIssue(
          invalidName,
          lifecycleIssues,
          'invalid',
          validationError,
          activationKey,
        )
      }
      return null
    }

    if (expectedManifestName && result.manifest.name !== expectedManifestName) {
      const validationError =
        'Remote MCPB manifest name does not match the plugin-declared server name'
      errors.push({
        type: 'mcp-config-invalid',
        source: plugin.source,
        plugin: plugin.name,
        serverName: expectedManifestName,
        validationError,
      })
      addRemoteMcpbIssue(
        expectedManifestName,
        lifecycleIssues,
        'invalid',
        validationError,
        activationKey,
      )
      return null
    }

    // Check if MCPB needs user configuration
    if ('status' in result && result.status === 'needs-config') {
      // User config needed - this is normal for unconfigured plugins
      // Don't load the MCP server yet - user can configure via /plugin menu
      logForDebugging(
        `MCPB ${mcpbPath} requires user configuration. ` +
          `User can configure via: /plugin → Manage plugins → ${plugin.name} → Configure`,
      )
      addRemoteMcpbIssue(
        logicalServerName ?? result.manifest.name,
        lifecycleIssues,
        'requiresConfig',
        'Required plugin configuration is missing or invalid',
        activationKey,
        projectPluginMcpConfiguration(
          result.configSchema,
          result.existingConfig,
          new Set(result.existingSecureKeys),
        ),
      )
      // Return null to skip this server for now (not an error)
      return null
    }

    // Type guard passed - result is success type
    const successResult = result as McpbLoadResult

    // Use the DXT manifest name as the server name
    const serverName = logicalServerName ?? successResult.manifest.name

    // Check for server name conflicts with existing servers
    // This will be checked later when merging all servers, but we log here for debugging
    logForDebugging(
      `Loaded MCP server "${serverName}" from MCPB (extracted to ${successResult.extractedPath})`,
    )

    return { [serverName]: successResult.mcpConfig }
  } catch (error) {
    const errorMsg = errorMessage(error)
    logForDebugging(`Failed to load MCPB ${mcpbPath}: ${errorMsg}`, {
      level: 'error',
    })

    // Use plugin@repository as source (consistent with other plugin errors)
    const source = `${plugin.name}@${plugin.repository}`

    // Determine error type based on error message
    const isUrl = mcpbPath.startsWith('http')
    if (
      isUrl &&
      (errorMsg.includes('download') || errorMsg.includes('network'))
    ) {
      errors.push({
        type: 'mcpb-download-failed',
        source,
        plugin: plugin.name,
        url: mcpbPath,
        reason: errorMsg,
      })
    } else if (
      errorMsg.includes('manifest') ||
      errorMsg.includes('user configuration')
    ) {
      errors.push({
        type: 'mcpb-invalid-manifest',
        source,
        plugin: plugin.name,
        mcpbPath,
        validationError: errorMsg,
      })
    } else {
      errors.push({
        type: 'mcpb-extract-failed',
        source,
        plugin: plugin.name,
        mcpbPath,
        reason: errorMsg,
      })
    }

    if (logicalServerName) {
      addRemoteMcpbIssue(
        logicalServerName,
        lifecycleIssues,
        'invalid',
        'Remote MCPB could not be loaded after explicit activation',
        activationKey,
      )
    }

    return null
  }
}

/**
 * Load MCP servers from a plugin's manifest
 * This function loads MCP server configurations from various sources within the plugin
 * including manifest entries, .mcp.json files, and .mcpb files
 */
export async function loadPluginMcpServers(
  plugin: LoadedPlugin,
  errors: PluginError[] = [],
  lifecycleIssues: PluginMcpLoadIssue[] = [],
  options: PluginMcpLoadOptions = {},
): Promise<Record<string, McpServerConfig> | undefined> {
  let servers: Record<string, McpServerConfig> = {}
  // Authored local keys reserve their logical identity even when the payload
  // is currently invalid. Otherwise a remote MCPB alias can occupy the same
  // name at runtime while lifecycle inventory also creates an invalid local
  // row, producing two source identities behind one opaque runtime name.
  const authoredLocalServerNames = new Set<string>()
  const rememberAuthoredLocalName = (name: string): void => {
    authoredLocalServerNames.add(name)
  }

  // Check for .mcp.json in plugin directory first (lowest priority)
  const defaultMcpServers = await loadMcpServersFromFile(
    plugin,
    '.mcp.json',
    errors,
    true,
    rememberAuthoredLocalName,
  )
  if (defaultMcpServers) {
    servers = { ...servers, ...defaultMcpServers }
  }

  // Handle manifest mcpServers if present (higher priority)
  if (plugin.manifest.mcpServers) {
    const mcpServersSpec = plugin.manifest.mcpServers

    // Handle different mcpServers formats
    if (typeof mcpServersSpec === 'string') {
      // Check if it's an MCPB file
      if (isMcpbSource(mcpServersSpec)) {
        const isManagedPackage = true
        const remoteIdentity = isManagedPackage
          ? resolveRemoteMcpbLogicalName(
              mcpServersSpec,
              new Set([
                ...authoredLocalServerNames,
                ...Object.keys(servers),
              ]),
              options,
            )
          : undefined
        if (
          isManagedPackage &&
          !options.allowedRemoteMcpbSources?.has(mcpServersSpec)
        ) {
          addRemoteMcpbIssue(
            remoteIdentity?.logicalName ??
              getRemoteMcpbActivationName(mcpServersSpec),
            lifecycleIssues,
            'requiresConfig',
            'Remote MCPB is deferred until this server is explicitly enabled',
            remoteIdentity?.activationKey,
          )
          return Object.keys(servers).length > 0 ? servers : undefined
        }
        const mcpbServers = await loadMcpServersFromMcpb(
          plugin,
          mcpServersSpec,
          errors,
          lifecycleIssues,
          remoteIdentity?.logicalName,
          remoteIdentity?.expectedName,
          remoteIdentity?.activationKey,
        )
        if (mcpbServers) {
          servers = { ...servers, ...mcpbServers }
        }
      } else {
        // Path to JSON file
        const mcpServers = await loadMcpServersFromFile(
          plugin,
          mcpServersSpec,
          errors,
          false,
          rememberAuthoredLocalName,
        )
        if (mcpServers) {
          servers = { ...servers, ...mcpServers }
        }
      }
    } else if (Array.isArray(mcpServersSpec)) {
      // Resolve every authored/local spec first. Remote logical names may use
      // a sole channel/required declaration only when no local source owns the
      // same name. Results are still merged in original order (last wins).
      const results: Array<Record<string, McpServerConfig> | null> =
        Array.from({ length: mcpServersSpec.length }, () => null)
      await Promise.all(
        mcpServersSpec.map(async (spec, index) => {
          if (
            typeof spec === 'string' &&
            isMcpbSource(spec)
          ) {
            return
          }
          try {
            if (typeof spec === 'string') {
              if (isMcpbSource(spec)) {
                results[index] = await loadMcpServersFromMcpb(
                  plugin,
                  spec,
                  errors,
                  lifecycleIssues,
                )
                return
              }
              results[index] = await loadMcpServersFromFile(
                plugin,
                spec,
                errors,
                false,
                rememberAuthoredLocalName,
              )
              return
            }
            for (const name of Object.keys(spec)) {
              if (McpServerNameSchema().safeParse(name).success) {
                rememberAuthoredLocalName(name)
              }
            }
            results[index] = rejectReservedAuthoredServerNames(
              plugin,
              spec,
              errors,
            )
          } catch (e) {
            logForDebugging(
              `Failed to load MCP servers from spec for plugin ${plugin.name}: ${e}`,
              { level: 'error' },
            )
          }
        }),
      )

      const localServerNames = new Set([
        ...authoredLocalServerNames,
        ...Object.keys(servers),
      ])
      for (const result of results) {
        for (const serverName of Object.keys(result ?? {})) {
          localServerNames.add(serverName)
        }
      }

      // Duplicate URL specs share one Promise: exact source authorization
      // yields exactly one download/extraction side effect.
      const remoteLoads = new Map<
        string,
        Promise<Record<string, McpServerConfig> | null>
      >()
      await Promise.all(
        mcpServersSpec.map(async (spec, index) => {
          if (
            typeof spec !== 'string' ||
            !isMcpbSource(spec)
          ) {
            return
          }
          const identity = resolveRemoteMcpbLogicalName(
            spec,
            localServerNames,
            options,
          )
          if (!options.allowedRemoteMcpbSources?.has(spec)) {
            addRemoteMcpbIssue(
              identity.logicalName,
              lifecycleIssues,
              'requiresConfig',
              'Remote MCPB is deferred until this server is explicitly enabled',
              identity.activationKey,
            )
            return
          }
          let pending = remoteLoads.get(spec)
          if (!pending) {
            pending = loadMcpServersFromMcpb(
              plugin,
              spec,
              errors,
              lifecycleIssues,
              identity.logicalName,
              identity.expectedName,
              identity.activationKey,
            )
            remoteLoads.set(spec, pending)
          }
          results[index] = await pending
        }),
      )
      for (const result of results) {
        if (result) {
          servers = { ...servers, ...result }
        }
      }
    } else {
      // Direct MCP server configs
      servers = {
        ...servers,
        ...rejectReservedAuthoredServerNames(plugin, mcpServersSpec, errors),
      }
    }
  }

  return Object.keys(servers).length > 0 ? servers : undefined
}

/**
 * Load MCP servers from a JSON file within a plugin
 * This is a simplified version that doesn't expand environment variables
 * and is specifically for plugin MCP configs
 */
async function loadMcpServersFromFile(
  plugin: LoadedPlugin,
  relativePath: string,
  errors: PluginError[],
  optional = false,
  onAuthoredServerName?: (name: string) => void,
): Promise<Record<string, McpServerConfig> | null> {
  let filePath: string
  try {
    filePath = await resolvePluginComponentPath(plugin.path, relativePath, {
      component: 'MCP config',
    })
  } catch (error) {
    if (
      optional &&
      error instanceof PluginPathSecurityError &&
      error.reason === 'path-missing'
    ) {
      return null
    }
    const validationError = errorMessage(error)
    logForDebugging(
      `Rejected MCP config path ${relativePath} for plugin ${plugin.name}: ${validationError}`,
      { level: 'warn' },
    )
    errors.push({
      type: 'mcp-config-invalid',
      source: plugin.source,
      plugin: plugin.name,
      serverName: '*',
      diagnosticScope: 'file',
      validationError: `${relativePath}: ${validationError}`,
    })
    return null
  }

  let content: string
  try {
    content = await readCanonicalPluginTextFile(
      plugin.path,
      filePath,
      'MCP config',
    )
  } catch (e: unknown) {
    if (isENOENT(e)) {
      if (!optional) {
        errors.push({
          type: 'mcp-config-invalid',
          source: plugin.source,
          plugin: plugin.name,
          serverName: '*',
          diagnosticScope: 'file',
          validationError: `${relativePath}: MCP config file disappeared before it was read`,
        })
      }
      return null
    }
    logForDebugging(`Failed to load MCP servers from ${filePath}: ${e}`, {
      level: 'error',
    })
    errors.push({
      type: 'mcp-config-invalid',
      source: plugin.source,
      plugin: plugin.name,
      serverName: '*',
      diagnosticScope: 'file',
      validationError: `${relativePath}: MCP config file could not be read safely`,
    })
    return null
  }

  try {
    const parsed = jsonParse(content) as unknown
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      throw new Error('MCP config root must be an object')
    }
    const parsedRecord = parsed as Record<string, unknown>
    const hasWrapper = Object.prototype.hasOwnProperty.call(
      parsedRecord,
      'mcpServers',
    )
    if (
      hasWrapper &&
      (!parsedRecord.mcpServers ||
        typeof parsedRecord.mcpServers !== 'object' ||
        Array.isArray(parsedRecord.mcpServers))
    ) {
      throw new Error('MCP config mcpServers must be an object')
    }

    // Check if it's in the .mcp.json format with mcpServers key.
    const mcpServers = hasWrapper
      ? (parsedRecord.mcpServers as Record<string, unknown>)
      : parsedRecord

    // Validate each server config
    const validatedServers: Record<string, McpServerConfig> = {}
    for (const [name, config] of Object.entries(mcpServers)) {
      if (!McpServerNameSchema().safeParse(name).success) {
        errors.push({
          type: 'mcp-config-invalid',
          source: plugin.source,
          plugin: plugin.name,
          serverName: name,
          validationError:
            'Plugin MCP server name is empty or uses a reserved runtime namespace',
        })
        continue
      }
      onAuthoredServerName?.(name)
      const result = McpServerConfigSchema().safeParse(config)
      if (result.success) {
        validatedServers[name] = result.data
      } else {
        logForDebugging(
          `Invalid MCP server config for ${name} in ${filePath}: ${result.error.message}`,
          { level: 'error' },
        )
        errors.push({
          type: 'mcp-config-invalid',
          source: plugin.source,
          plugin: plugin.name,
          serverName: name,
          validationError: 'MCP server configuration is invalid',
        })
      }
    }

    return validatedServers
  } catch (error) {
    logForDebugging(`Failed to load MCP servers from ${filePath}: ${error}`, {
      level: 'error',
    })
    errors.push({
      type: 'mcp-config-invalid',
      source: plugin.source,
      plugin: plugin.name,
      serverName: '*',
      diagnosticScope: 'file',
      validationError: `${relativePath}: MCP config file is malformed`,
    })
    return null
  }
}

/**
 * A channel entry from a plugin's manifest whose userConfig has not yet been
 * filled in (required fields are missing from saved settings).
 */
export type UnconfiguredChannel = {
  server: string
  displayName: string
  configSchema: UserConfigSchema
}

/**
 * Find channel entries in a plugin's manifest whose required userConfig
 * fields are not yet saved. Pure function — no React, no prompting.
 * ManagePlugins.tsx calls this after a plugin is enabled to decide whether
 * to show the config dialog.
 *
 * Entries without a `userConfig` schema are skipped (nothing to prompt for).
 * Entries whose saved config already satisfies `validateUserConfig` are
 * skipped. The `configSchema` in the return value is structurally a
 * `UserConfigSchema` because the Zod schema in schemas.ts matches
 * `McpbUserConfigurationOption` field-for-field.
 */
export function getUnconfiguredChannels(
  plugin: LoadedPlugin,
): UnconfiguredChannel[] {
  const channels = plugin.manifest.channels
  if (!channels || channels.length === 0) {
    return []
  }

  // plugin.repository is already in "plugin@marketplace" format — same key
  // loadMcpServerUserConfig / saveMcpServerUserConfig use.
  const pluginId = plugin.repository

  const unconfigured: UnconfiguredChannel[] = []
  for (const channel of channels) {
    if (!channel.userConfig || Object.keys(channel.userConfig).length === 0) {
      continue
    }
    const saved = loadMcpServerUserConfig(pluginId, channel.server) ?? {}
    const validation = validateUserConfig(saved, channel.userConfig)
    if (!validation.valid) {
      unconfigured.push({
        server: channel.server,
        displayName: channel.displayName ?? channel.server,
        configSchema: channel.userConfig,
      })
    }
  }
  return unconfigured
}

/**
 * Look up saved user config for a server, if this server is declared as a
 * channel in the plugin's manifest. Returns undefined for non-channel servers
 * or channels without a userConfig schema — resolvePluginMcpEnvironment will
 * then skip ${user_config.X} substitution for that server.
 */
function loadChannelUserConfigWithProvenance(
  plugin: LoadedPlugin,
  serverName: string,
): {
  values: UserConfigValues | undefined
  storedSensitiveKeys: ReadonlySet<string>
} {
  const channel = plugin.manifest.channels?.find(c => c.server === serverName)
  if (!channel?.userConfig) {
    return { values: undefined, storedSensitiveKeys: new Set() }
  }
  const snapshot = loadMcpServerUserConfigWithProvenance(
    plugin.repository,
    serverName,
  )
  return {
    values: snapshot.values ?? undefined,
    storedSensitiveKeys: snapshot.storedSensitiveKeys,
  }
}

/**
 * Add plugin scope to MCP server configs
 * This adds a prefix to server names to avoid conflicts between plugins
 */
export function addPluginScopeToServers(
  servers: Record<string, McpServerConfig>,
  pluginName: string,
  pluginSource: string,
): Record<string, ScopedMcpServerConfig> {
  const scopedServers: Record<string, ScopedMcpServerConfig> = {}

  for (const [name, config] of Object.entries(servers)) {
    // Source-qualified opaque identity prevents same-name plugins from
    // overwriting each other. Authorization continues to use pluginSource /
    // lifecycle metadata, never decoded runtime-name segments.
    const scopedName = buildPluginMcpRuntimeName(
      pluginName,
      pluginSource,
      name,
    )
    const scoped: ScopedMcpServerConfig = {
      ...config,
      scope: 'dynamic', // Use dynamic scope for plugin servers
      pluginSource,
    }
    scopedServers[scopedName] = scoped
  }

  return scopedServers
}

/**
 * Extract all MCP servers from loaded plugins
 * NOTE: Resolves environment variables for all servers before returning
 */
export async function extractMcpServersFromPlugins(
  plugins: LoadedPlugin[],
  errors: PluginError[] = [],
): Promise<Record<string, ScopedMcpServerConfig>> {
  const allServers: Record<string, ScopedMcpServerConfig> = {}

  const scopedResults = await Promise.all(
    plugins.map(async plugin => {
      if (!plugin.enabled) return null

      const servers = await loadPluginMcpServers(plugin, errors)
      if (!servers) return null

      // Resolve environment variables before scoping. When a saved channel
      // config is missing a key (plugin update added a required field, or a
      // hand-edited settings.json), substituteUserConfigVariables throws
      // inside resolvePluginMcpEnvironment — catch per-server so one bad
      // config doesn't crash the whole plugin load via Promise.all.
      const resolvedServers: Record<string, McpServerConfig> = {}
      for (const [name, config] of Object.entries(servers)) {
        const userConfig = buildMcpUserConfig(plugin, name)
        try {
          resolvedServers[name] = resolvePluginMcpEnvironment(
            config,
            plugin,
            userConfig,
            errors,
            plugin.name,
            name,
          )
        } catch (err) {
          errors?.push({
            type: 'generic-error',
            source: name,
            plugin: plugin.name,
            error: errorMessage(err),
          })
        }
      }

      // Store the UNRESOLVED servers on the plugin for caching
      // (Environment variables will be resolved fresh each time they're needed)
      plugin.mcpServers = servers

      logForDebugging(
        `Loaded ${Object.keys(servers).length} MCP servers from plugin ${plugin.name}`,
      )

      return addPluginScopeToServers(
        resolvedServers,
        plugin.name,
        plugin.source,
      )
    }),
  )

  for (const scopedServers of scopedResults) {
    if (scopedServers) {
      Object.assign(allServers, scopedServers)
    }
  }

  return allServers
}

/**
 * Build the userConfig map for a single MCP server by merging the plugin's
 * top-level manifest.userConfig values with the channel-specific per-server
 * config (assistant-mode channels). Channel-specific wins on collision so
 * plugins that declare the same key at both levels get the more specific value.
 *
 * Returns undefined when neither source has anything — resolvePluginMcpEnvironment
 * skips substituteUserConfigVariables in that case.
 */
function buildMcpUserConfig(
  plugin: LoadedPlugin,
  serverName: string,
): UserConfigValues | undefined {
  // Gate on manifest.userConfig. loadPluginOptions always returns at least {}
  // (it spreads two `?? {}` fallbacks), so without this guard topLevel is never
  // undefined — the `!topLevel` check below is dead, we return {} for
  // unconfigured plugins, and resolvePluginMcpEnvironment runs
  // substituteUserConfigVariables against an empty map → throws on any
  // ${user_config.X} ref. The manifest check also skips the unconditional
  // keychain read (~50-100ms on macOS) for plugins that don't use options.
  return buildMcpUserConfigWithProvenance(plugin, serverName).values
}

function buildMcpUserConfigWithProvenance(
  plugin: LoadedPlugin,
  serverName: string,
): {
  values: UserConfigValues | undefined
  storedSensitiveKeys: ReadonlySet<string>
} {
  const topLevel = plugin.manifest.userConfig
    ? loadPluginOptionsWithProvenance(getPluginStorageId(plugin))
    : undefined
  const channelSpecific = loadChannelUserConfigWithProvenance(plugin, serverName)

  if (!topLevel && !channelSpecific.values) {
    return { values: undefined, storedSensitiveKeys: new Set() }
  }
  const storedSensitiveKeys = new Set<string>(
    topLevel?.storedSensitiveKeys ?? [],
  )
  // Channel values win on collisions, so their storage provenance must also
  // replace the top-level provenance for that key.
  for (const key of Object.keys(channelSpecific.values ?? {})) {
    storedSensitiveKeys.delete(key)
  }
  for (const key of channelSpecific.storedSensitiveKeys) {
    storedSensitiveKeys.add(key)
  }
  return {
    values: { ...topLevel?.values, ...channelSpecific.values },
    storedSensitiveKeys,
  }
}

/**
 * Resolve environment variables for plugin MCP servers
 * Handles ${CRABCODE_PLUGIN_ROOT}, ${user_config.X}, and general ${VAR} substitution
 * Tracks missing environment variables for error reporting
 */
export function resolvePluginMcpEnvironment(
  config: McpServerConfig,
  plugin: { path: string; source: string },
  userConfig?: UserConfigValues,
  errors?: PluginError[],
  pluginName?: string,
  serverName?: string,
  options: PluginMcpEnvironmentOptions = {},
): McpServerConfig {
  const allMissingVars: string[] = []

  const resolveValue = (value: string): string => {
    // First substitute plugin-specific variables
    let resolved = substitutePluginVariables(value, plugin)

    // Then substitute user config variables if provided
    if (userConfig) {
      resolved = substituteUserConfigVariables(resolved, userConfig)
    }

    // Finally expand general environment variables
    // This is done last so plugin-specific and user config vars take precedence
    const { expanded, missingVars } = expandEnvVarsInString(resolved)
    allMissingVars.push(...missingVars)

    return expanded
  }

  let resolved: McpServerConfig

  // Handle different server types
  switch (config.type) {
    case undefined:
    case 'stdio': {
      const stdioConfig = { ...config }

      // Resolve command path
      if (stdioConfig.command) {
        stdioConfig.command = resolveValue(stdioConfig.command)
      }

      // Resolve args
      if (stdioConfig.args) {
        stdioConfig.args = stdioConfig.args.map(arg => resolveValue(arg))
      }

      // Resolve environment variables and add CRABCODE_PLUGIN_ROOT / CRABCODE_PLUGIN_DATA
      const resolvedEnv: Record<string, string> = {
        CRABCODE_PLUGIN_ROOT: plugin.path,
        CRABCODE_PLUGIN_DATA: (options.pluginDataDir ?? getPluginDataDir)(
          plugin.source,
        ),
        ...(stdioConfig.env || {}),
      }
      for (const [key, value] of Object.entries(resolvedEnv)) {
        if (key !== 'CRABCODE_PLUGIN_ROOT' && key !== 'CRABCODE_PLUGIN_DATA') {
          resolvedEnv[key] = resolveValue(value)
        }
      }
      stdioConfig.env = resolvedEnv

      resolved = stdioConfig
      break
    }

    case 'sse':
    case 'http':
    case 'ws': {
      const remoteConfig = { ...config }

      // Resolve URL
      if (remoteConfig.url) {
        remoteConfig.url = resolveValue(remoteConfig.url)
      }

      // Resolve headers
      if (remoteConfig.headers) {
        const resolvedHeaders: Record<string, string> = {}
        for (const [key, value] of Object.entries(remoteConfig.headers)) {
          resolvedHeaders[key] = resolveValue(value)
        }
        remoteConfig.headers = resolvedHeaders
      }

      resolved = remoteConfig
      break
    }

    // For other types (sse-ide, ws-ide, sdk, acosmi-proxy), pass through unchanged
    case 'sse-ide':
    case 'ws-ide':
    case 'sdk':
    case 'acosmi-proxy':
      resolved = config
      break
  }

  // Log and track missing variables if any were found and errors array provided
  if (errors && allMissingVars.length > 0) {
    const uniqueMissingVars = [...new Set(allMissingVars)]
    const varList = uniqueMissingVars.join(', ')

    logForDebugging(
      `Missing environment variables in plugin MCP config: ${varList}`,
      { level: 'warn' },
    )

    // Add error to the errors array if plugin and server names are provided
    if (pluginName && serverName) {
      errors.push({
        type: 'mcp-config-invalid',
        source: `plugin:${pluginName}`,
        plugin: pluginName,
        serverName,
        validationError: `Missing environment variables: ${varList}`,
      })
    }
  }

  return resolved
}

/**
 * Get MCP servers from a specific plugin with environment variable resolution and scoping
 * This function is called when the MCP servers need to be activated and ensures they have
 * the proper environment variables and scope applied
 */
export async function getPluginMcpServers(
  plugin: LoadedPlugin,
  errors: PluginError[] = [],
  options: PluginMcpEnvironmentOptions = {},
): Promise<Record<string, ScopedMcpServerConfig> | undefined> {
  if (!plugin.enabled) {
    return undefined
  }

  // Use cached servers if available
  const servers =
    plugin.mcpServers || (await loadPluginMcpServers(plugin, errors))
  if (!servers) {
    return undefined
  }

  // Resolve environment variables. Same per-server try/catch as
  // extractMcpServersFromPlugins above: a partial saved channel config
  // (plugin update added a required field) would make
  // substituteUserConfigVariables throw inside resolvePluginMcpEnvironment,
  // and this function runs inside Promise.all at config.ts:911 — one
  // uncaught throw crashes all plugin MCP loading.
  const resolvedServers: Record<string, McpServerConfig> = {}
  for (const [name, config] of Object.entries(servers)) {
    const userConfig = buildMcpUserConfig(plugin, name)
    try {
      resolvedServers[name] = resolvePluginMcpEnvironment(
        config,
        plugin,
        userConfig,
        errors,
        plugin.name,
        name,
        options,
      )
    } catch (err) {
      errors?.push({
        type: 'generic-error',
        source: name,
        plugin: plugin.name,
        error: errorMessage(err),
      })
    }
  }

  // Add plugin scope
  return addPluginScopeToServers(resolvedServers, plugin.name, plugin.source)
}

function pluginMcpSourceKind(plugin: LoadedPlugin): PluginMcpSourceKind {
  if (plugin.isBuiltin) return 'builtin'
  if (plugin.source.endsWith('@inline')) return 'inline'
  return 'marketplace'
}

function pluginMcpGeneration(plugin: LoadedPlugin): string {
  return `${plugin.sha ?? plugin.manifest.version ?? 'unversioned'}:${plugin.path}`
}

function userConfigSchemaForServer(
  plugin: LoadedPlugin,
  serverName: string,
): UserConfigSchema | undefined {
  const channel = plugin.manifest.channels?.find(c => c.server === serverName)
  const schema = {
    ...(plugin.manifest.userConfig ?? {}),
    ...(channel?.userConfig ?? {}),
  }
  return Object.keys(schema).length > 0 ? schema : undefined
}

function safeConfigurationValue(
  value: unknown,
): PluginMcpConfigurationValue | undefined {
  if (typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (Array.isArray(value) && value.every(item => typeof item === 'string')) {
    return value
  }
  return undefined
}

/**
 * Convert an author schema plus saved values into a wire-safe management
 * descriptor. Sensitive values and sensitive defaults are deliberately never
 * copied into the projection; only a configured bit crosses the boundary.
 */
export function projectPluginMcpConfiguration(
  schema: UserConfigSchema,
  values: UserConfigValues = {},
  storedSensitiveKeys: ReadonlySet<string> = new Set(),
): PluginMcpConfigurationDescriptor {
  // Existing settings/storage writers use ordinary objects internally. Do not
  // expose a write UI for author schemas containing prototype-mutating keys.
  if (
    Object.keys(schema).some(key =>
      ['__proto__', 'prototype', 'constructor'].includes(key),
    )
  ) {
    return { fields: [] }
  }
  return {
    fields: Object.entries(schema)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, field]) => {
        const saved = safeConfigurationValue(values[key])
        const configured =
          storedSensitiveKeys.has(key) ||
          (saved !== undefined &&
            saved !== '' &&
            (!Array.isArray(saved) || saved.length > 0))
        // A value already resident in secure storage remains wire-sensitive
        // across an author schema flip until the user explicitly rewrites it
        // and the writer scrubs the old secure copy.
        const sensitive =
          field.sensitive === true || storedSensitiveKeys.has(key)
        return {
          key,
          type: field.type,
          title: field.title,
          description: field.description,
          required: field.required === true,
          multiple: field.multiple === true,
          sensitive,
          configured,
          ...(field.min !== undefined ? { min: field.min } : {}),
          ...(field.max !== undefined ? { max: field.max } : {}),
          ...(!sensitive && safeConfigurationValue(field.default) !== undefined
            ? { defaultValue: safeConfigurationValue(field.default)! }
            : {}),
          ...(!sensitive && saved !== undefined
            ? { currentValue: saved }
            : {}),
        }
      }),
  }
}

function blockedInventoryRecord(
  plugin: LoadedPlugin,
  serverName: string,
  pluginRoot: string,
  configState: 'requiresConfig' | 'invalid',
  reason: string,
  persistedActivation: ReturnType<typeof getPersistedPluginMcpActivation>,
  activationKey = serverName,
  configuration?: PluginMcpConfigurationDescriptor,
): PluginMcpInventoryRecord {
  return {
    ...classifyPluginMcpLifecycle({
      pluginId: getPluginStorageId(plugin),
      pluginName: plugin.name,
      serverName,
      runtimeName: buildPluginMcpRuntimeName(
        plugin.name,
        plugin.source,
        activationKey,
      ),
      activationKey,
      sourceKind: pluginMcpSourceKind(plugin),
      pluginEnabled: plugin.enabled === true,
      persistedActivation,
      manifestRequired:
        plugin.manifest.requiredMcpServers?.includes(serverName) ?? false,
      requiredAutoActivationEligible: activationKey === serverName,
      configState,
      authState: 'unknown',
      dependencyState: 'unknown',
      transport: undefined,
      pluginRoot,
      generation: pluginMcpGeneration(plugin),
      reason,
    }),
    ...(configuration ? { configuration } : {}),
  }
}

/**
 * Build the complete plugin MCP inventory. Unlike `getPluginMcpServers`, this
 * preserves disabled/inactive/config-blocked entries. A remote package is
 * read only when that exact package's logical activation is already enabled;
 * inactive packages do not download, read auth storage, or spawn.
 */
export async function getPluginMcpLifecycleInventory(
  plugin: LoadedPlugin,
  errors: PluginError[] = [],
  options: PluginMcpLifecycleInventoryOptions = {},
): Promise<PluginMcpInventoryRecord[]> {
  const pluginId = getPluginStorageId(plugin)
  const readPersistedActivation =
    options.readPersistedActivation ?? createPluginMcpActivationSnapshotReader()
  const reconcileActivations =
    options.reconcileActivations ?? reconcilePluginMcpActivations
  const readKnownAuthState = options.getKnownAuthState ?? getKnownMcpAuthState
  const localErrors: PluginError[] = []
  const lifecycleIssues: PluginMcpLoadIssue[] = []
  const records: PluginMcpInventoryRecord[] = []

  let canonicalRoot = plugin.path
  let rootInvalidReason: string | undefined
  if (!plugin.isBuiltin) {
    try {
      canonicalRoot = await resolvePluginComponentPath(plugin.path, '.', {
        component: 'plugin MCP root',
      })
    } catch (error) {
      rootInvalidReason = errorMessage(error)
      localErrors.push({
        type: 'mcp-config-invalid',
        source: plugin.source,
        plugin: plugin.name,
        serverName: '*',
        diagnosticScope: 'plugin',
        validationError: rootInvalidReason,
      })
    }
  }

  const remoteEntries = remoteMcpbEntries(plugin)
  const remoteMcpbActivationNames = new Map(
    remoteEntries.map(entry => [entry.source, entry.activationName]),
  )
  const remoteMcpbDeclaredNames = new Map(
    remoteEntries.flatMap(entry =>
      entry.declaredName
        ? [[entry.source, entry.declaredName] as const]
        : [],
    ),
  )
  const resolvedRemoteMcpbLogicalNames = new Map<string, string>()
  const allowedRemoteMcpbSources = new Set(
    plugin.enabled === true
      ? remoteEntries
          .filter(
            entry =>
              readPersistedActivation(pluginId, entry.activationName) ===
              'enabled',
          )
          .map(entry => entry.source)
      : [],
  )
  const remoteMcpbDeferred = remoteEntries.some(
    entry => !allowedRemoteMcpbSources.has(entry.source),
  )
  if (rootInvalidReason) {
    for (const entry of remoteEntries) {
      addRemoteMcpbIssue(
        entry.activationName,
        lifecycleIssues,
        'invalid',
        'Plugin root could not be resolved safely',
      )
    }
  }
  const servers = rootInvalidReason
    ? inlineManifestMcpServers(plugin, localErrors)
    : (remoteEntries.length === 0 && plugin.mcpServers
      ? plugin.mcpServers
      :
      (await loadPluginMcpServers(plugin, localErrors, lifecycleIssues, {
        allowedRemoteMcpbSources,
        remoteMcpbActivationNames,
        remoteMcpbDeclaredNames,
        resolvedRemoteMcpbLogicalNames,
      })))

  for (const [serverName, unresolved] of Object.entries(servers ?? {})) {
    const isRemotePackageIdentity = remoteEntries.some(
      entry =>
        (resolvedRemoteMcpbLogicalNames.get(entry.source) ??
          entry.activationName) === serverName,
    )
    if (
      isReservedPluginMcpAuthoredServerName(serverName) &&
      !isRemotePackageIdentity
    ) {
      localErrors.push({
        type: 'mcp-config-invalid',
        source: plugin.source,
        plugin: plugin.name,
        serverName,
        validationError:
          'Plugin MCP server name uses a reserved runtime namespace',
      })
      continue
    }
    const remoteEntry = remoteEntries.find(
      entry =>
        resolvedRemoteMcpbLogicalNames.get(entry.source) === serverName,
    )
    const activationKey = remoteEntry?.activationName ?? serverName
    // Runtime identity is also the stable control identity. A remote package's
    // presentation alias can switch ownership when a local authored server is
    // added/removed; binding the opaque name to source-hash activationKey keeps
    // a stale UI action from toggling the other source after that TOCTOU.
    const runtimeName = buildPluginMcpRuntimeName(
      plugin.name,
      plugin.source,
      activationKey,
    )
    const persistedActivation = readPersistedActivation(pluginId, activationKey)
    const manifestRequired =
      plugin.manifest.requiredMcpServers?.includes(serverName) ?? false
    const schema = userConfigSchemaForServer(plugin, serverName)
    const userConfigSnapshot = buildMcpUserConfigWithProvenance(
      plugin,
      serverName,
    )
    const userConfig = userConfigSnapshot.values
    let configState: PluginMcpConfigState = rootInvalidReason
      ? 'invalid'
      : 'ready'
    let reason: string | undefined = rootInvalidReason
      ? 'Plugin root could not be resolved safely'
      : undefined
    let configurationRecoverable = false
    if (configState === 'ready' && schema) {
      const validation = validateUserConfig(userConfig ?? {}, schema)
      if (!validation.valid) {
        configState = 'requiresConfig'
        reason = 'Required plugin configuration is missing or invalid'
      }
    }

    const localTransport =
      unresolved.type === undefined ||
      unresolved.type === 'stdio' ||
      unresolved.type === 'sdk'
    const shouldEvaluateRuntime =
      plugin.enabled === true &&
      (persistedActivation === 'enabled' ||
        (persistedActivation === undefined &&
          manifestRequired &&
          localTransport))

    let resolved: McpServerConfig | undefined
    if (configState === 'ready' && shouldEvaluateRuntime) {
      const errorCountBeforeResolve = localErrors.length
      try {
        resolved = resolvePluginMcpEnvironment(
          unresolved,
          { path: canonicalRoot, source: plugin.source },
          userConfig,
          localErrors,
          plugin.name,
          serverName,
          options.pluginDataDir
            ? { pluginDataDir: options.pluginDataDir }
            : {},
        )
      } catch (error) {
        configState = 'invalid'
        reason = 'Plugin MCP configuration could not be resolved'
        const message = errorMessage(error)
        configurationRecoverable =
          schema !== undefined &&
          Object.keys(schema).some(
            key =>
              message ===
              `Missing required user configuration value: ${key}. ` +
                'This should have been validated before variable substitution.',
          )
      }
      if (localErrors.length > errorCountBeforeResolve) {
        configState = 'invalid'
        reason = 'Plugin MCP configuration contains unresolved values'
        resolved = undefined
      }
    } else if (configState === 'ready') {
      // Inactive/disabled inventory is deliberately static: keep enough shape
      // for management UI without touching plugin data, PATH, keychain, or a
      // remote endpoint. A fresh inventory rebuild evaluates it after opt-in.
      resolved = unresolved
    }

    let dependencyState: PluginMcpDependencyState =
      configState === 'ready' && shouldEvaluateRuntime ? 'ready' : 'unknown'
    let authState: PluginMcpAuthState =
      resolved?.type === 'http' || resolved?.type === 'sse'
        ? 'unknown'
        : 'notRequired'
    let executableConfig = resolved

    if (
      shouldEvaluateRuntime &&
      resolved &&
      (resolved.type === undefined || resolved.type === 'stdio')
    ) {
      try {
        const preflight = await preflightPluginStdio(resolved, canonicalRoot, {
          ...(options.env ? { env: options.env } : {}),
        })
        if (preflight.state === 'ready') {
          executableConfig = preflight.config
          canonicalRoot = preflight.cwd
          dependencyState = 'ready'
        } else {
          executableConfig = resolved
          dependencyState = 'requiresDependency'
          reason = preflight.reason
        }
      } catch {
        executableConfig = resolved
        dependencyState = 'requiresDependency'
        reason = 'Plugin MCP runtime or canonical working directory is unavailable'
      }
    }

    if (
      executableConfig &&
      (executableConfig.type === 'http' || executableConfig.type === 'sse') &&
      shouldEvaluateRuntime
    ) {
      authState = await readKnownAuthState(runtimeName, executableConfig)
      if (authState === 'requiresLogin') {
        reason = 'Authentication is required before this connector can start'
      }
    }

    const lifecycle = classifyPluginMcpLifecycle({
      pluginId,
      pluginName: plugin.name,
      serverName,
      runtimeName,
      activationKey,
      sourceKind: pluginMcpSourceKind(plugin),
      pluginEnabled: plugin.enabled === true,
      persistedActivation,
      manifestRequired,
      requiredAutoActivationEligible:
        manifestRequired && localTransport && remoteEntry === undefined,
      configState,
      authState,
      dependencyState,
      transport: executableConfig?.type,
      pluginRoot: canonicalRoot,
      generation: pluginMcpGeneration(plugin),
      ...(reason ? { reason } : {}),
    })

    records.push({
      ...lifecycle,
      ...(schema && (configState !== 'invalid' || configurationRecoverable)
        ? {
            configuration: projectPluginMcpConfiguration(
              schema,
              userConfig,
              userConfigSnapshot.storedSensitiveKeys,
            ),
          }
        : {}),
      ...(executableConfig
        ? {
            config: {
              ...executableConfig,
              scope: 'dynamic',
              pluginSource: plugin.source,
              pluginMcp: lifecycle,
            } as ScopedMcpServerConfig,
          }
        : {}),
    })
  }

  for (const error of localErrors) {
    errors.push(error)
    if (
      error.type === 'mcp-config-invalid' &&
      error.diagnosticScope !== 'file' &&
      error.diagnosticScope !== 'plugin' &&
      McpServerNameSchema().safeParse(error.serverName).success &&
      !lifecycleIssues.some(issue => issue.serverName === error.serverName) &&
      !records.some(record => record.serverName === error.serverName)
    ) {
      records.push(
        blockedInventoryRecord(
          plugin,
          error.serverName,
          canonicalRoot,
          'invalid',
          'Plugin MCP configuration is invalid',
          readPersistedActivation(pluginId, error.serverName),
        ),
      )
    }
  }

  for (const issue of lifecycleIssues) {
    if (records.some(record => record.serverName === issue.serverName)) continue
    records.push(
      blockedInventoryRecord(
        plugin,
        issue.serverName,
        canonicalRoot,
        issue.configState,
        issue.reason,
        readPersistedActivation(
          pluginId,
          issue.activationKey ?? issue.serverName,
        ),
        issue.activationKey,
        issue.configuration,
        ),
    )
  }

  // A load with any MCP error is degraded and cannot authorize pruning. A
  // successful complete inventory may prune rename/delete residue while
  // preserving same-name upgrade activation.
  reconcileActivations(
    pluginId,
    records.map(record => record.activationKey ?? record.serverName),
    localErrors.length === 0 && !remoteMcpbDeferred,
  )
  return records
}

export async function extractPluginMcpLifecycleInventory(
  plugins: readonly LoadedPlugin[],
  errors: PluginError[] = [],
  options: PluginMcpLifecycleInventoryOptions = {},
): Promise<PluginMcpInventoryRecord[]> {
  const inventoryOptions: PluginMcpLifecycleInventoryOptions =
    options.readPersistedActivation
      ? options
      : {
          ...options,
          readPersistedActivation: createPluginMcpActivationSnapshotReader(),
        }
  const results = await Promise.all(
    plugins.map(async plugin => {
      try {
        return await getPluginMcpLifecycleInventory(
          plugin,
          errors,
          inventoryOptions,
        )
      } catch (error) {
        errors.push({
          type: 'mcp-config-invalid',
          source: plugin.source,
          plugin: plugin.name,
          serverName: '*',
          diagnosticScope: 'plugin',
          validationError: errorMessage(error),
        })
        return []
      }
    }),
  )
  return results.flat()
}
