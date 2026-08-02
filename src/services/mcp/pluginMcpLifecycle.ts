import type {
  MCPServerConnection,
  McpServerConfig,
  PluginMcpLifecycleMetadata,
  ScopedMcpServerConfig,
} from './types.js'
import { existsSync } from 'node:fs'
import {
  refreshSettingsForSource,
  updateSettingsForSource,
} from '../../utils/settings/settings.js'
import { resetSettingsCache } from '../../utils/settings/settingsCache.js'
import type { SettingsJson } from '../../utils/settings/types.js'

export type PluginMcpPersistedActivation = 'inactive' | 'enabled'
export type PluginMcpActivation = PluginMcpLifecycleMetadata['activation']
export type PluginMcpConfigState = PluginMcpLifecycleMetadata['configState']
export type PluginMcpAuthState = PluginMcpLifecycleMetadata['authState']
export type PluginMcpDependencyState =
  PluginMcpLifecycleMetadata['dependencyState']
export type PluginMcpSourceKind = PluginMcpLifecycleMetadata['sourceKind']

export type PluginMcpConfigurationValue =
  | string
  | number
  | boolean
  | string[]

export type PluginMcpConfigurationField = {
  key: string
  type: 'string' | 'number' | 'boolean' | 'directory' | 'file'
  title: string
  description: string
  required: boolean
  multiple: boolean
  sensitive: boolean
  configured: boolean
  min?: number
  max?: number
  defaultValue?: PluginMcpConfigurationValue
  currentValue?: PluginMcpConfigurationValue
}

export type PluginMcpConfigurationDescriptor = {
  fields: PluginMcpConfigurationField[]
}

let userSettingsReaderOverride: (() => SettingsJson | null) | null = null

/** Test-only source seam; production remains hard-wired to raw userSettings. */
export function __setPluginMcpUserSettingsReaderForTest(
  reader: (() => SettingsJson | null) | null,
): void {
  userSettingsReaderOverride = reader
}

function readFreshPluginMcpUserSettings(): SettingsJson | null {
  if (userSettingsReaderOverride) return userSettingsReaderOverride()
  return refreshSettingsForSource('userSettings') ?? null
}

export interface PluginMcpLifecycleInput {
  pluginId: string
  pluginName: string
  serverName: string
  runtimeName: string
  /** Persisted activation leaf; differs from display name for remote MCPB. */
  activationKey?: string
  sourceKind: PluginMcpSourceKind
  pluginEnabled: boolean
  persistedActivation: PluginMcpPersistedActivation | undefined
  manifestRequired: boolean
  requiredAutoActivationEligible?: boolean
  configState: PluginMcpConfigState
  authState: PluginMcpAuthState
  dependencyState: PluginMcpDependencyState
  transport: McpServerConfig['type']
  pluginRoot?: string
  generation: string
  reason?: string
}

export interface PluginMcpInventoryRecord
  extends PluginMcpLifecycleMetadata {
  config?: ScopedMcpServerConfig
  /** Secret-free management projection; never contains a sensitive value. */
  configuration?: PluginMcpConfigurationDescriptor
}

export function lifecycleMetadataFromInventory(
  record: PluginMcpInventoryRecord,
): PluginMcpLifecycleMetadata {
  const {
    config: _config,
    configuration: _configuration,
    ...metadata
  } = record
  return metadata
}

/**
 * Put a lifecycle-only row into the existing MCP management store without
 * fabricating a connectable transport. Runtime eligibility rejects the
 * marker; a successful opt-in replaces it with the freshly resolved config.
 */
export function managementConfigFromPluginMcpInventory(
  record: PluginMcpInventoryRecord,
): ScopedMcpServerConfig {
  return {
    type: 'sdk',
    name: record.runtimeName,
    scope: 'dynamic',
    pluginSource: record.pluginId,
    pluginMcp: lifecycleMetadataFromInventory(record),
    pluginMcpManagementOnly: true,
  }
}

function isLocalTransport(type: McpServerConfig['type']): boolean {
  return type === undefined || type === 'stdio' || type === 'sdk'
}

/**
 * Pure lifecycle predicate. It has no I/O and therefore cannot accidentally
 * probe a remote endpoint or spawn a runtime while deciding whether a server
 * is eligible to connect.
 */
export function classifyPluginMcpLifecycle(
  input: PluginMcpLifecycleInput,
): PluginMcpLifecycleMetadata {
  const requiredAutoActivationEligible =
    input.requiredAutoActivationEligible ?? isLocalTransport(input.transport)
  const activation: PluginMcpActivation =
    input.persistedActivation ??
    (input.manifestRequired && requiredAutoActivationEligible
      ? 'required'
      : 'inactive')

  let reasonCode: PluginMcpLifecycleMetadata['reasonCode'] = 'ready'
  if (!input.pluginEnabled) reasonCode = 'plugin-disabled'
  else if (activation === 'inactive') reasonCode = 'inactive'
  else if (input.configState === 'requiresConfig') {
    reasonCode = 'requires-config'
  } else if (input.configState === 'invalid') {
    reasonCode = 'invalid-config'
  } else if (input.authState === 'requiresLogin') {
    reasonCode = 'requires-login'
  } else if (input.dependencyState !== 'ready') {
    reasonCode = 'requires-dependency'
  }

  const metadata: PluginMcpLifecycleMetadata = {
    pluginId: input.pluginId,
    pluginName: input.pluginName,
    serverName: input.serverName,
    runtimeName: input.runtimeName,
    activationKey: input.activationKey ?? input.serverName,
    sourceKind: input.sourceKind,
    pluginEnabled: input.pluginEnabled,
    activation,
    configState: input.configState,
    authState: input.authState,
    dependencyState: input.dependencyState,
    active: reasonCode === 'ready',
    reasonCode,
    pluginRoot: input.pluginRoot ?? '',
    generation: input.generation,
  }
  if (input.reason) metadata.reason = input.reason
  return metadata
}

export function pluginMcpLifecycleOf(
  config: Pick<ScopedMcpServerConfig, 'pluginMcp'>,
): PluginMcpLifecycleMetadata | undefined {
  return config.pluginMcp
}

export function isPluginMcpConfigActive(
  config: Pick<ScopedMcpServerConfig, 'pluginMcp'>,
): boolean {
  return config.pluginMcp?.active ?? true
}

/** Activation intent is independent from the current connection/blocker state. */
export function isMcpServerExplicitlyEnabled(
  client: Pick<MCPServerConnection, 'type' | 'config'>,
): boolean {
  return client.config.pluginMcp
    ? client.config.pluginMcp.activation !== 'inactive'
    : client.type !== 'disabled'
}

/** Fresh user-settings authority check for reconnect/retry boundaries. */
export function isPluginMcpPersistedActivationAllowed(
  metadata: PluginMcpLifecycleMetadata,
): boolean {
  if (metadata.pluginRoot && !existsSync(metadata.pluginRoot)) return false
  const userSettings = readFreshPluginMcpUserSettings()
  if (userSettings?.enabledPlugins?.[metadata.pluginId] === false) return false
  const persisted = userSettings?.pluginConfigs?.[metadata.pluginId]
    ?.mcpServerActivation?.[metadata.activationKey ?? metadata.serverName]
  if (metadata.activation === 'required') return persisted !== 'inactive'
  return metadata.activation === 'enabled' && persisted === 'enabled'
}

/** Eligibility immediately after a user opt-in, before a fresh inventory is rebuilt. */
export function canPluginMcpConnectAfterExplicitEnable(
  metadata: PluginMcpLifecycleMetadata,
): boolean {
  return (
    metadata.pluginEnabled &&
    metadata.configState === 'ready' &&
    metadata.authState !== 'requiresLogin' &&
    metadata.dependencyState === 'ready'
  )
}

export function getPersistedPluginMcpActivation(
  pluginId: string,
  serverName: string,
): PluginMcpPersistedActivation | undefined {
  return createPluginMcpActivationSnapshotReader()(pluginId, serverName)
}

/** Fresh, single-read user-settings snapshot for one complete inventory pass. */
export function createPluginMcpActivationSnapshotReader(): (
  pluginId: string,
  serverName: string,
) => PluginMcpPersistedActivation | undefined {
  const pluginConfigs = readFreshPluginMcpUserSettings()?.pluginConfigs
  return (pluginId, serverName) =>
    pluginConfigs?.[pluginId]?.mcpServerActivation?.[serverName]
}

function assertStorageIdentity(pluginId: string, serverName: string): void {
  if (pluginId.trim().length === 0 || serverName.trim().length === 0) {
    throw new Error('plugin MCP activation requires non-empty plugin/server IDs')
  }
}

/**
 * Persist explicit per-server activation in user settings. The stable storage
 * key is plugin.source + logical server name; runtime/display names never
 * participate in permission inheritance.
 */
export function setPluginMcpServerActivation(
  pluginId: string,
  serverName: string,
  enabled: boolean,
): void {
  assertStorageIdentity(pluginId, serverName)
  // updateSettingsForSource acquires the cross-process file lock, but its
  // internal parse path can still reuse this process's stale path cache. Clear
  // it immediately before entering the synchronous lock/write boundary so the
  // leaf patch merges into the latest on-disk sibling values.
  resetSettingsCache()
  const result = updateSettingsForSource(
    'userSettings',
    buildPluginMcpActivationPatch({}, pluginId, serverName, enabled),
  )
  if (result.error) throw result.error
}

/**
 * Build the settings patch used by the persistence boundary. Keeping this
 * transformation pure lets tests prove the stable source/server key without
 * mutating process-wide settings paths (Bun runs test files concurrently).
 */
export function buildPluginMcpActivationPatch(
  _user: SettingsJson,
  pluginId: string,
  serverName: string,
  enabled: boolean,
): SettingsJson {
  assertStorageIdentity(pluginId, serverName)
  const next: PluginMcpPersistedActivation = enabled ? 'enabled' : 'inactive'
  return {
    pluginConfigs: {
      [pluginId]: {
        mcpServerActivation: {
          [serverName]: next,
        },
      },
    },
  }
}

/**
 * Activation cleanup is deliberately non-destructive. An inventory snapshot
 * has no generation/CAS token, so even a locally successful load can be stale
 * relative to another window that just installed or enabled a new server.
 * Stale activation leaves are harmless and are removed with the whole plugin
 * config on uninstall; deleting them here can lose a current user choice.
 */
export function reconcilePluginMcpActivationMap(
  current: Readonly<Record<string, PluginMcpPersistedActivation>>,
  _serverNames: readonly string[],
  _authoritative: boolean,
): Record<string, PluginMcpPersistedActivation> {
  return { ...current }
}

/**
 * Reserved for a future generation-aware CAS reconciliation. It is a no-op
 * until inventory carries a storage generation that can prove freshness.
 */
export function reconcilePluginMcpActivations(
  _pluginId: string,
  _serverNames: readonly string[],
  _authoritative: boolean,
): void {
  // Intentionally empty; see comment above.
}
