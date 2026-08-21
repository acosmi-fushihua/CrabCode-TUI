// ANT-ONLY import markers must not be reordered
import type { Tools } from 'src/Tool.js'
import type { Command } from 'src/commands.js'
import type {
  MCPServerConnection,
  ScopedMcpServerConfig,
  McpSdkServerConfig,
} from 'src/services/mcp/types.js'
import type {
  McpServerConfigForProcessTransport,
} from 'src/entrypoints/agentSdkTypes.js'
import type {
  SDKControlMcpSetServersResponse,
} from 'src/entrypoints/sdk/controlTypes.js'
import type { AppState } from 'src/state/AppStateStore.js'
import {
  connectToServer,
  evictExistingServerCache,
  fetchToolsForClient,
  fetchCommandsForClient,
  areMcpConfigsEqual,
} from 'src/services/mcp/client.js'
import { filterMcpServersByPolicy } from 'src/services/mcp/config.js'
import { commandBelongsToServer } from 'src/services/mcp/utils.js'
import { getMcpPrefix } from 'src/services/mcp/mcpStringUtils.js'
import { logError } from 'src/utils/log.js'
import { toError } from 'src/utils/errors.js'

export type DynamicMcpState = {
  clients: MCPServerConnection[]
  tools: Tools
  configs: Record<string, ScopedMcpServerConfig>
}

export type StartupProcessMcpPartition = {
  dynamicState: DynamicMcpState
  remainingClients: MCPServerConnection[]
  remainingTools: Tools
}

export type McpServerManagementDependencies = {
  connectToServer: (
    name: string,
    config: ScopedMcpServerConfig,
  ) => Promise<MCPServerConnection>
  evictExistingServerCache: (
    name: string,
    config: ScopedMcpServerConfig,
  ) => Promise<void>
  fetchToolsForClient: (client: MCPServerConnection) => Promise<Tools>
  fetchCommandsForClient: (client: MCPServerConnection) => Promise<Command[]>
}

export type ReconcileMcpServersOptions = {
  /**
   * Direct-TUI-only opt-in for keeping live MCP prompt commands in AppState.
   * The public SDK/headless route intentionally retains its established behavior
   * when this is omitted or false.
   */
  syncPromptCommands?: boolean
  /** Direct-route opt-in for removing stale resource snapshots on reconcile. */
  syncResourceCleanup?: boolean
  /** Direct route has globally admitted normalized wire namespaces. */
  strictWireNamespaceCleanup?: boolean
  /** Preserve trusted internal config scope/provenance for direct plugin state. */
  preserveDesiredScopes?: boolean
  /** Narrow test seam; production callers use the module defaults. */
  dependencies?: Partial<McpServerManagementDependencies>
}

const defaultDependencies: McpServerManagementDependencies = {
  connectToServer,
  evictExistingServerCache,
  fetchToolsForClient,
  fetchCommandsForClient,
}

/** Small FIFO lane for stateful MCP mutations; a rejected task never poisons it. */
export function createMcpMutationLane(): <T>(
  work: () => Promise<T>,
) => Promise<T> {
  let tail: Promise<void> = Promise.resolve()
  return <T>(work: () => Promise<T>): Promise<T> => {
    const result = tail.then(work, work)
    tail = result.then(
      () => undefined,
      () => undefined,
    )
    return result
  }
}

function isProcessTransportConnection(
  connection: MCPServerConnection,
): boolean {
  const type = connection.config.type
  return (
    type === undefined || type === 'stdio' || type === 'sse' || type === 'http'
  )
}

/**
 * Moves startup process-transport MCP ownership into DynamicMcpState.
 *
 * Direct TUI connects configured servers before the headless query loop starts.
 * Seeding reconciliation from that live snapshot lets the very first plugin diff
 * observe removals instead of treating every surviving server as a fresh add.
 * The returned remainder keeps non-process transports and non-MCP tools with
 * their existing owner.
 */
export function partitionStartupProcessMcpState(
  clients: MCPServerConnection[],
  tools: Tools,
  retainedProcessOwnerNames: ReadonlySet<string> = new Set(),
): StartupProcessMcpPartition {
  const processClients: MCPServerConnection[] = []
  const remainingClients: MCPServerConnection[] = []
  const configs: Record<string, ScopedMcpServerConfig> = {}

  for (const client of clients) {
    if (
      isProcessTransportConnection(client) &&
      !retainedProcessOwnerNames.has(client.name)
    ) {
      processClients.push(client)
      configs[client.name] = client.config
    } else {
      remainingClients.push(client)
    }
  }

  const processPrefixes = processClients.map(client => getMcpPrefix(client.name))
  const processTools: Tools[number][] = []
  const remainingTools: Tools[number][] = []
  for (const tool of tools) {
    if (processPrefixes.some(prefix => tool.name?.startsWith(prefix))) {
      processTools.push(tool)
    } else {
      remainingTools.push(tool)
    }
  }

  return {
    dynamicState: {
      clients: processClients,
      tools: processTools,
      configs,
    },
    remainingClients,
    remainingTools,
  }
}

/**
 * State for SDK MCP servers that run in the SDK process.
 */
export type SdkMcpState = {
  configs: Record<string, McpSdkServerConfig>
  clients: MCPServerConnection[]
  tools: Tools
}

/**
 * Result of handleMcpSetServers - contains new state and response data.
 */
export type McpSetServersResult = {
  response: SDKControlMcpSetServersResponse
  newSdkState: SdkMcpState
  newDynamicState: DynamicMcpState
  sdkServersChanged: boolean
}

/**
 * Converts a process transport config to a scoped config.
 * The types are structurally compatible, so we just add the scope.
 */
export function toScopedConfig(
  config: McpServerConfigForProcessTransport,
  preserveDesiredScope = false,
): ScopedMcpServerConfig {
  // McpServerConfigForProcessTransport is a subset of McpServerConfig
  // (it excludes IDE-specific types like sse-ide and ws-ide)
  // Adding scope makes it a valid ScopedMcpServerConfig
  const suppliedScope = preserveDesiredScope
    ? (config as { scope?: ScopedMcpServerConfig['scope'] }).scope
    : undefined
  return {
    ...config,
    scope: suppliedScope ?? 'dynamic',
  } as ScopedMcpServerConfig
}

/**
 * Handles mcp_set_servers requests by processing both SDK and process-based servers.
 * SDK servers run in the SDK process; process-based servers are spawned by the CLI.
 *
 * Applies enterprise allowedMcpServers/deniedMcpServers policy — same filter as
 * --mcp-config (see filterMcpServersByPolicy call in main.tsx). Without this,
 * SDK V2 Query.setMcpServers() was a second policy bypass vector. Blocked servers
 * are reported in response.errors so the SDK consumer knows why they weren't added.
 */
export async function handleMcpSetServers(
  servers: Record<string, McpServerConfigForProcessTransport>,
  sdkState: SdkMcpState,
  dynamicState: DynamicMcpState,
  setAppState: (f: (prev: AppState) => AppState) => void,
  options: ReconcileMcpServersOptions = {},
): Promise<McpSetServersResult> {
  // Enforce enterprise MCP policy on process-based servers (stdio/http/sse).
  // Mirrors the --mcp-config filter in main.tsx — both user-controlled injection
  // paths must have the same gate. type:'sdk' servers are exempt (SDK-managed,
  // CLI never spawns/connects for them — see filterMcpServersByPolicy jsdoc).
  // Blocked servers go into response.errors so the SDK caller sees why.
  const { allowed: allowedServers, blocked } = filterMcpServersByPolicy(servers)
  const policyErrors: Record<string, string> = {}
  for (const name of blocked) {
    policyErrors[name] =
      'Blocked by enterprise policy (allowedMcpServers/deniedMcpServers)'
  }

  // Separate SDK servers from process-based servers
  const sdkServers: Record<string, McpSdkServerConfig> = {}
  const processServers: Record<string, McpServerConfigForProcessTransport> = {}

  for (const [name, config] of Object.entries(allowedServers)) {
    if (config.type === 'sdk') {
      sdkServers[name] = config
    } else {
      processServers[name] = config
    }
  }

  // Handle SDK servers
  const currentSdkNames = new Set(Object.keys(sdkState.configs))
  const newSdkNames = new Set(Object.keys(sdkServers))
  const sdkAdded: string[] = []
  const sdkRemoved: string[] = []

  const newSdkConfigs = { ...sdkState.configs }
  let newSdkClients = [...sdkState.clients]
  let newSdkTools = [...sdkState.tools]

  // Remove SDK servers no longer in desired state
  for (const name of currentSdkNames) {
    if (!newSdkNames.has(name)) {
      const client = newSdkClients.find(c => c.name === name)
      if (client && client.type === 'connected') {
        await client.cleanup()
      }
      newSdkClients = newSdkClients.filter(c => c.name !== name)
      const prefix = `mcp__${name}__`
      newSdkTools = newSdkTools.filter(t => !t.name.startsWith(prefix))
      delete newSdkConfigs[name]
      sdkRemoved.push(name)
    }
  }

  // Add new SDK servers as pending - they'll be upgraded to connected
  // when updateSdkMcp() runs on the next query
  for (const [name, config] of Object.entries(sdkServers)) {
    if (!currentSdkNames.has(name)) {
      newSdkConfigs[name] = config
      const pendingClient: MCPServerConnection = {
        type: 'pending',
        name,
        config: { ...config, scope: 'dynamic' as const },
      }
      newSdkClients = [...newSdkClients, pendingClient]
      sdkAdded.push(name)
    }
  }

  // Handle process-based servers
  const processResult = await reconcileMcpServers(
    processServers,
    dynamicState,
    setAppState,
    options,
  )

  return {
    response: {
      added: [...sdkAdded, ...processResult.response.added],
      removed: [...sdkRemoved, ...processResult.response.removed],
      errors: { ...policyErrors, ...processResult.response.errors },
    },
    newSdkState: {
      configs: newSdkConfigs,
      clients: newSdkClients,
      tools: newSdkTools,
    },
    newDynamicState: processResult.newState,
    sdkServersChanged: sdkAdded.length > 0 || sdkRemoved.length > 0,
  }
}

/**
 * Reconciles the current set of dynamic MCP servers with a new desired state.
 * Handles additions, removals, and config changes.
 */
export async function reconcileMcpServers(
  desiredConfigs: Record<string, McpServerConfigForProcessTransport>,
  currentState: DynamicMcpState,
  setAppState: (f: (prev: AppState) => AppState) => void,
  options: ReconcileMcpServersOptions = {},
): Promise<{
  response: SDKControlMcpSetServersResponse
  newState: DynamicMcpState
}> {
  const currentNames = new Set(Object.keys(currentState.configs))
  const desiredNames = new Set(Object.keys(desiredConfigs))

  const toRemove = [...currentNames].filter(n => !desiredNames.has(n))
  const toAdd = [...desiredNames].filter(n => !currentNames.has(n))

  // Check for config changes (same name, different config)
  const toCheck = [...currentNames].filter(n => desiredNames.has(n))
  const toReplace = toCheck.filter(name => {
    const currentConfig = currentState.configs[name]
    const desiredConfigRaw = desiredConfigs[name]
    if (!currentConfig || !desiredConfigRaw) return true
    const desiredConfig = toScopedConfig(
      desiredConfigRaw,
      options.preserveDesiredScopes,
    )
    return (
      !areMcpConfigsEqual(currentConfig, desiredConfig) ||
      (options.preserveDesiredScopes === true &&
        currentConfig.scope !== desiredConfig.scope)
    )
  })

  const removed: string[] = []
  const added: string[] = []
  const errors: Record<string, string> = {}
  const dependencies = {
    ...defaultDependencies,
    ...options.dependencies,
  }

  let newClients = [...currentState.clients]
  let newTools = [...currentState.tools]
  const refreshedPromptCommands: Command[] = []

  // Remove old servers (including ones being replaced)
  for (const name of [...toRemove, ...toReplace]) {
    const config = currentState.configs[name]
    if (config) {
      // This is the sole teardown owner. Unlike clearServerCache, eviction
      // never creates a connection on a cache miss, and it cleans an existing
      // cached client at most once before invalidating all derived caches.
      await dependencies.evictExistingServerCache(name, config)
    }

    // Remove tools from this server
    const prefix = options.strictWireNamespaceCleanup
      ? getMcpPrefix(name)
      : `mcp__${name}__`
    newTools = newTools.filter(t => !t.name.startsWith(prefix))

    // Remove from clients list
    newClients = newClients.filter(c => c.name !== name)

    // Track removal (only for actually removed, not replaced)
    if (toRemove.includes(name)) {
      removed.push(name)
    }
  }

  // Add new servers (including replacements)
  for (const name of [...toAdd, ...toReplace]) {
    const config = desiredConfigs[name]
    if (!config) continue
    const scopedConfig = toScopedConfig(config, options.preserveDesiredScopes)

    // SDK servers are managed by the SDK process, not the CLI.
    // Just track them without trying to connect.
    if (config.type === 'sdk') {
      added.push(name)
      continue
    }

    try {
      const client = await dependencies.connectToServer(name, scopedConfig)
      newClients.push(client)

      if (client.type === 'connected') {
        const [serverTools, serverCommands] = await Promise.all([
          dependencies.fetchToolsForClient(client),
          options.syncPromptCommands
            ? dependencies.fetchCommandsForClient(client)
            : Promise.resolve([]),
        ])
        newTools.push(...serverTools)
        refreshedPromptCommands.push(...serverCommands)
      } else if (client.type === 'failed') {
        errors[name] = client.error || 'Connection failed'
      }

      added.push(name)
    } catch (e) {
      const err = toError(e)
      errors[name] = err.message
      logError(err)
    }
  }

  // Build new configs
  const newConfigs: Record<string, ScopedMcpServerConfig> = {}
  for (const name of desiredNames) {
    const config = desiredConfigs[name]
    if (config) {
      const currentConfig = currentState.configs[name]
      newConfigs[name] =
        currentConfig && !toReplace.includes(name)
          ? currentConfig
          : toScopedConfig(config, options.preserveDesiredScopes)
    }
  }

  const newState: DynamicMcpState = {
    clients: newClients,
    tools: newTools,
    configs: newConfigs,
  }

  // Update AppState once so clients, tools, and (for the opted-in direct route)
  // prompt commands describe the same completed reconciliation.
  setAppState(prev => {
    // Get all dynamic server names (current + new)
    const allDynamicServerNames = new Set([
      ...Object.keys(currentState.configs),
      ...Object.keys(newConfigs),
    ])

    // Remove old dynamic tools
    const nonDynamicTools = prev.mcp.tools.filter(t => {
      for (const serverName of allDynamicServerNames) {
        const prefix = options.strictWireNamespaceCleanup
          ? getMcpPrefix(serverName)
          : `mcp__${serverName}__`
        if (t.name.startsWith(prefix)) {
          return false
        }
      }
      return true
    })

    // Remove old dynamic clients
    const nonDynamicClients = prev.mcp.clients.filter(c => {
      return !allDynamicServerNames.has(c.name)
    })

    const affectedPromptServerNames = new Set([
      ...toRemove,
      ...toAdd,
      ...toReplace,
    ])
    let resources = prev.mcp.resources
    if (options.syncResourceCleanup && affectedPromptServerNames.size > 0) {
      resources = { ...prev.mcp.resources }
      // Reconciliation does not fetch resources. Remove only the affected
      // owner's stale snapshots; unrelated/fixed owners retain their entries.
      for (const serverName of affectedPromptServerNames) {
        delete resources[serverName]
      }
    }
    const commands =
      options.syncPromptCommands && affectedPromptServerNames.size > 0
        ? [
            ...prev.mcp.commands.filter(command => {
              for (const serverName of affectedPromptServerNames) {
                if (
                  (command.isMcp === true ||
                    (command.type === 'prompt' &&
                      command.loadedFrom === 'mcp')) &&
                  commandBelongsToServer(command, serverName)
                ) {
                  return false
                }
              }
              return true
            }),
            ...refreshedPromptCommands,
          ]
        : prev.mcp.commands

    return {
      ...prev,
      mcp: {
        ...prev.mcp,
        tools: [...nonDynamicTools, ...newTools],
        clients: [...nonDynamicClients, ...newClients],
        commands,
        resources,
      },
    }
  })

  return {
    response: { added, removed, errors },
    newState,
  }
}
