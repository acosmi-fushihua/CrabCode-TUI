import { feature } from '../../utils/featurePolyfill.js'
import type { JSONRPCMessage } from '@modelcontextprotocol/sdk/types.js'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import pMap from 'p-map'
import type { Command } from '../../commands.js'
import { PRODUCT_URL } from '../../constants/product.js'
import type { Tool } from '../../Tool.js'
import { toolMatchesName } from '../../Tool.js'
import { ListMcpResourcesTool } from '../../tools/ListMcpResourcesTool/ListMcpResourcesTool.js'
import { ReadMcpResourceTool } from '../../tools/ReadMcpResourceTool/ReadMcpResourceTool.js'
import { createMcpAuthTool } from '../../tools/McpAuthTool/McpAuthTool.js'
import { count } from '../../utils/array.js'
import { errorMessage } from '../../utils/errors.js'
import { logMCPDebug, logMCPError } from '../../utils/log.js'
import { clearKeychainCache } from '../../utils/secureStorage/macOsKeychainHelpers.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import { hasMcpDiscoveryButNoToken } from './auth.js'
import { markAcosmiMcpConnected } from './acosmi.js'
import {
  getAllMcpConfigs,
  isMcpServerDisabled,
  isMcpServerRuntimeActive,
} from './config.js'
import { SdkControlClientTransport } from './SdkControlTransport.js'
import { isPluginMcpPersistedActivationAllowed } from './pluginMcpLifecycle.js'
import type {
  ConnectedMCPServer,
  MCPServerConnection,
  McpSdkServerConfig,
  ScopedMcpServerConfig,
  ServerResource,
} from './types.js'
import {
  isMcpAuthCached,
  getMcpServerConnectionBatchSize,
  getRemoteMcpServerConnectionBatchSize,
  isLocalMcpServer,
} from './authCache.js'
import {
  connectToServer,
  clearServerCache,
  fetchToolsForClient,
  fetchResourcesForClient,
  fetchCommandsForClient,
} from './connectionAndFetch.js'

// Replaced 2026-03: previous implementation ran fixed-size sequential batches
// (await batch 1 fully, then start batch 2). That meant one slow server in
// batch N held up ALL servers in batch N+1, even if the other 19 slots were
// idle. pMap frees each slot as soon as its server completes, so a single
// slow server only occupies one slot instead of blocking an entire batch
// boundary. Same concurrency ceiling, same results, better scheduling.
async function processBatched<T>(
  items: T[],
  concurrency: number,
  processor: (item: T) => Promise<void>,
): Promise<void> {
  await pMap(items, processor, { concurrency })
}

export async function getMcpToolsCommandsAndResources(
  onConnectionAttempt: (params: {
    client: MCPServerConnection
    tools: Tool[]
    commands: Command[]
    resources?: ServerResource[]
  }) => void,
  mcpConfigs?: Record<string, ScopedMcpServerConfig>,
): Promise<void> {
  let resourceToolsAdded = false

  const allConfigEntries = Object.entries(
    mcpConfigs ?? (await getAllMcpConfigs()).servers,
  )

  // Partition into disabled and active entries — disabled servers should
  // never generate HTTP connections or flow through batch processing
  const configEntries: typeof allConfigEntries = []
  for (const entry of allConfigEntries) {
    const [name, config] = entry
    if (config.pluginMcp && !isMcpServerRuntimeActive(name, config)) {
      if (
        config.pluginMcp.reasonCode === 'requires-login' &&
        (config.type === 'http' || config.type === 'sse')
      ) {
        onConnectionAttempt({
          client: { name, type: 'needs-auth', config },
          tools: [createMcpAuthTool(name, config)],
          commands: [],
        })
      } else {
        onConnectionAttempt({
          client: { name, type: 'disabled', config },
          tools: [],
          commands: [],
        })
      }
    } else if (isMcpServerDisabled(name)) {
      onConnectionAttempt({
        client: { name, type: 'disabled', config },
        tools: [],
        commands: [],
      })
    } else {
      configEntries.push(entry)
    }
  }

  // Calculate transport counts for logging
  const totalServers = configEntries.length
  const stdioCount = count(configEntries, ([_, c]) => c.type === 'stdio')
  const sseCount = count(configEntries, ([_, c]) => c.type === 'sse')
  const httpCount = count(configEntries, ([_, c]) => c.type === 'http')
  const sseIdeCount = count(configEntries, ([_, c]) => c.type === 'sse-ide')
  const wsIdeCount = count(configEntries, ([_, c]) => c.type === 'ws-ide')

  // Split servers by type: local (stdio/sdk) need lower concurrency due to
  // process spawning, remote servers can connect with higher concurrency
  const localServers = configEntries.filter(([_, config]) =>
    isLocalMcpServer(config),
  )
  const remoteServers = configEntries.filter(
    ([_, config]) => !isLocalMcpServer(config),
  )

  const serverStats = {
    totalServers,
    stdioCount,
    sseCount,
    httpCount,
    sseIdeCount,
    wsIdeCount,
  }

  const processServer = async ([name, config]: [
    string,
    ScopedMcpServerConfig,
  ]): Promise<void> => {
    try {
      // Check if server is disabled - if so, just add it to state without connecting
      if (!isMcpServerRuntimeActive(name, config)) {
        onConnectionAttempt({
          client: {
            name,
            type: 'disabled',
            config,
          },
          tools: [],
          commands: [],
        })
        return
      }

      // Skip connection for servers that recently returned 401 (15min TTL),
      // or that we have probed before but hold no token for. The second
      // check closes the gap the TTL leaves open: without it, every 15min
      // we re-probe servers that cannot succeed until the user runs /mcp.
      // Each probe is a network round-trip for connect-401 plus OAuth
      // discovery, and print mode awaits the whole batch (main.tsx:3503).
      if (
        (config.type === 'acosmi-proxy' ||
          config.type === 'http' ||
          config.type === 'sse') &&
        ((await isMcpAuthCached(name)) ||
          ((config.type === 'http' || config.type === 'sse') &&
            (await hasMcpDiscoveryButNoToken(name, config))))
      ) {
        logMCPDebug(name, `Skipping connection (cached needs-auth)`)
        onConnectionAttempt({
          client: { name, type: 'needs-auth' as const, config },
          tools: [createMcpAuthTool(name, config)],
          commands: [],
        })
        return
      }

      const client = await connectToServer(name, config, serverStats)

      if (client.type !== 'connected') {
        onConnectionAttempt({
          client,
          tools:
            client.type === 'needs-auth'
              ? [createMcpAuthTool(name, config)]
              : [],
          commands: [],
        })
        return
      }

      if (config.type === 'acosmi-proxy') {
        markAcosmiMcpConnected(name)
      }

      const supportsResources = !!client.capabilities?.resources

      const [tools, commands, resources] = await Promise.all([
        fetchToolsForClient(client),
        fetchCommandsForClient(client),
        // Fetch resources if supported
        supportsResources
          ? fetchResourcesForClient(client)
          : Promise.resolve([]),
      ])

      // If this server resources and we haven't added resource tools yet,
      // include our resource tools with this client's tools
      const resourceTools: Tool[] = []
      if (supportsResources && !resourceToolsAdded) {
        resourceToolsAdded = true
        resourceTools.push(ListMcpResourcesTool, ReadMcpResourceTool)
      }

      onConnectionAttempt({
        client,
        tools: [...tools, ...resourceTools],
        commands,
        resources: resources.length > 0 ? resources : undefined,
      })
    } catch (error) {
      // Handle errors gracefully - connection might have closed during fetch
      logMCPError(
        name,
        `Error fetching tools/commands/resources: ${errorMessage(error)}`,
      )

      // Still update with the client but no tools/commands
      onConnectionAttempt({
        // 轴A G-A (2026-06-26): attach the captured failure reason so /mcp can
        // surface WHY a server failed (ETIMEDOUT / discovery 404 / TLS / …)
        // without --debug. `FailedMCPServer.error` exists but was dropped here.
        client: { name, type: 'failed' as const, config, error: errorMessage(error) },
        tools: [],
        commands: [],
      })
    }
  }

  // Process both groups concurrently, each with their own concurrency limits:
  // - Local servers (stdio/sdk): lower concurrency to avoid process spawning resource contention
  // - Remote servers: higher concurrency since they're just network connections
  await Promise.all([
    processBatched(
      localServers,
      getMcpServerConnectionBatchSize(),
      processServer,
    ),
    processBatched(
      remoteServers,
      getRemoteMcpServerConnectionBatchSize(),
      processServer,
    ),
  ])
}

// Not memoized: called only 2-3 times at startup/reconfig. The inner work
// (connectToServer, fetch*ForClient) is already cached. Memoizing here by
// mcpConfigs object ref leaked — main.tsx creates fresh config objects each call.
export function prefetchAllMcpResources(
  mcpConfigs: Record<string, ScopedMcpServerConfig>,
): Promise<{
  clients: MCPServerConnection[]
  tools: Tool[]
  commands: Command[]
}> {
  return new Promise(resolve => {
    let pendingCount = 0
    let completedCount = 0

    pendingCount = Object.keys(mcpConfigs).length

    if (pendingCount === 0) {
      void resolve({
        clients: [],
        tools: [],
        commands: [],
      })
      return
    }

    const clients: MCPServerConnection[] = []
    const tools: Tool[] = []
    const commands: Command[] = []

    getMcpToolsCommandsAndResources(result => {
      clients.push(result.client)
      tools.push(...result.tools)
      commands.push(...result.commands)

      completedCount++
      if (completedCount >= pendingCount) {
        const commandsMetadataLength = commands.reduce((sum, command) => {
          const commandMetadataLength =
            command.name.length +
            (command.description ?? '').length +
            (command.argumentHint ?? '').length
          return sum + commandMetadataLength
        }, 0)
        logEvent('tengu_mcp_tools_commands_loaded', {
          tools_count: tools.length,
          commands_count: commands.length,
          commands_metadata_length: commandsMetadataLength,
        })

        void resolve({
          clients,
          tools,
          commands,
        })
      }
    }, mcpConfigs).catch(error => {
      logMCPError(
        'prefetchAllMcpResources',
        `Failed to get MCP resources: ${errorMessage(error)}`,
      )
      // Still resolve with empty results
      void resolve({
        clients: [],
        tools: [],
        commands: [],
      })
    })
  })
}

/**
 * Note: This should not be called by UI components directly, they should use the reconnectMcpServer
 * function from useManageMcpConnections.
 * @param name Server name
 * @param config Server configuration
 * @returns Object containing the client connection and its resources
 */
export async function reconnectMcpServerImpl(
  name: string,
  config: ScopedMcpServerConfig,
): Promise<{
  client: MCPServerConnection
  tools: Tool[]
  commands: Command[]
  resources?: ServerResource[]
}> {
  if (
    !isMcpServerRuntimeActive(name, config) ||
    (config.pluginMcp !== undefined &&
      !isPluginMcpPersistedActivationAllowed(config.pluginMcp))
  ) {
    return {
      client: { name, type: 'disabled', config },
      tools: [],
      commands: [],
    }
  }
  try {
    // Invalidate the keychain cache so we read fresh credentials from disk.
    // This is necessary when another process (e.g. the VS Code extension host)
    // has modified stored tokens (cleared auth, saved new OAuth tokens) and then
    // asks the CLI subprocess to reconnect.  Without this, the subprocess would
    // use stale cached data and never notice the tokens were removed.
    clearKeychainCache()

    await clearServerCache(name, config)
    const client = await connectToServer(name, config)

    if (client.type !== 'connected') {
      return {
        client,
        tools: [],
        commands: [],
      }
    }

    if (config.type === 'acosmi-proxy') {
      markAcosmiMcpConnected(name)
    }

    const supportsResources = !!client.capabilities?.resources

    const [tools, commands, resources] = await Promise.all([
      fetchToolsForClient(client),
      fetchCommandsForClient(client),
      supportsResources ? fetchResourcesForClient(client) : Promise.resolve([]),
    ])

    // Check if we need to add resource tools
    const resourceTools: Tool[] = []
    if (supportsResources) {
      // Only add resource tools if no other server has them
      const hasResourceTools = [ListMcpResourcesTool, ReadMcpResourceTool].some(
        tool => tools.some(t => toolMatchesName(t, tool.name)),
      )
      if (!hasResourceTools) {
        resourceTools.push(ListMcpResourcesTool, ReadMcpResourceTool)
      }
    }

    return {
      client,
      tools: [...tools, ...resourceTools],
      commands,
      resources: resources.length > 0 ? resources : undefined,
    }
  } catch (error) {
    // Handle errors gracefully - connection might have closed during fetch
    logMCPError(name, `Error during reconnection: ${errorMessage(error)}`)

    // Return with failed status
    // 轴A G-A: surface the reconnect failure reason in /mcp（与初次连接 :218、
    // SDK MCP 连接 :468 两处 failed 构造逐字对齐;b4651ab5 漏填此第三处）。
    return {
      client: { name, type: 'failed' as const, config, error: errorMessage(error) },
      tools: [],
      commands: [],
    }
  }
}

/**
 * Sets up SDK MCP clients by creating transports and connecting them.
 * This is used for SDK MCP servers that run in the same process as the SDK.
 *
 * @param sdkMcpConfigs - The SDK MCP server configurations
 * @param sendMcpMessage - Callback to send MCP messages through the control channel
 * @returns Connected clients, their tools, and transport map for message routing
 */
export async function setupSdkMcpClients(
  sdkMcpConfigs: Record<string, McpSdkServerConfig>,
  sendMcpMessage: (
    serverName: string,
    message: JSONRPCMessage,
  ) => Promise<JSONRPCMessage>,
): Promise<{
  clients: MCPServerConnection[]
  tools: Tool[]
}> {
  const clients: MCPServerConnection[] = []
  const tools: Tool[] = []

  // Connect to all servers in parallel
  const results = await Promise.allSettled(
    Object.entries(sdkMcpConfigs).map(async ([name, config]) => {
      const transport = new SdkControlClientTransport(name, sendMcpMessage)

      const client = new Client(
        {
          name: 'crabcode',
          title: 'CrabCode',
          version: MACRO.VERSION ?? 'unknown',
          description: "Acosmi's agentic coding tool",
          websiteUrl: PRODUCT_URL,
        },
        {
          capabilities: {},
        },
      )

      try {
        // Connect the client
        await client.connect(transport)

        // Get capabilities from the server
        const capabilities = client.getServerCapabilities()

        // Create the connected client object
        const connectedClient: MCPServerConnection = {
          type: 'connected',
          name,
          capabilities: capabilities || {},
          client,
          config: { ...config, scope: 'dynamic' as const },
          cleanup: async () => {
            await client.close()
          },
        }

        // Fetch tools if the server has them
        const serverTools: Tool[] = []
        if (capabilities?.tools) {
          const sdkTools = await fetchToolsForClient(connectedClient)
          serverTools.push(...sdkTools)
        }

        return {
          client: connectedClient,
          tools: serverTools,
        }
      } catch (error) {
        // If connection fails, return failed server
        logMCPError(name, `Failed to connect SDK MCP server: ${error}`)
        return {
          client: {
            type: 'failed' as const,
            name,
            config: { ...config, scope: 'user' as const },
            // 轴A G-A: surface the SDK MCP connection failure reason in /mcp.
            error: errorMessage(error),
          },
          tools: [],
        }
      }
    }),
  )

  // Process results and collect clients and tools
  for (const result of results) {
    if (result.status === 'fulfilled') {
      clients.push(result.value.client)
      tools.push(...result.value.tools)
    }
    // If rejected (unexpected), the error was already logged inside the promise
  }

  return { clients, tools }
}
