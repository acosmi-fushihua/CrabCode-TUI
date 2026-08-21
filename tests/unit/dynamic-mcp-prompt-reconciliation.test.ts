import { afterEach, describe, expect, mock, test } from 'bun:test'

import type { Tools } from '../../src/Tool.js'
import type { Command } from '../../src/commands.js'
import type { McpServerConfigForProcessTransport } from '../../src/entrypoints/agentSdkTypes.js'
import {
  getDefaultAppState,
  type AppState,
} from '../../src/state/AppStateStore.js'
import {
  createMcpMutationLane,
  partitionStartupProcessMcpState,
  reconcileMcpServers,
  type DynamicMcpState,
  type McpServerManagementDependencies,
} from '../../src/cli/print/mcpServerManagement.js'
import {
  admitStartupMcpNamespaceReservations,
  canCommitDirectMcpOAuthGeneration,
  filterMcpServersByWireNamespace,
  isDirectTuiSdkMcpInventoryRecord,
  orderMcpServersByPrecedence,
  planCapturedMcpPolicyTransitions,
  preservePublicProcessDesiredAcrossSdkRejections,
  prepareMcpServersForOwner,
} from '../../src/cli/print/mcpServerOwnership.js'
import {
  __setMcpAuthClearRuntimeDepsForTest,
  clearMcpAuthenticationRuntime,
} from '../../src/services/mcp/mcpAuthClearRuntime.js'
import { getMcpPrefix } from '../../src/services/mcp/mcpStringUtils.js'
import type {
  MCPServerConnection,
  ScopedMcpServerConfig,
} from '../../src/services/mcp/types.js'

function rawConfig(command: string): McpServerConfigForProcessTransport {
  return { type: 'stdio', command, args: [] }
}

function scopedConfig(command: string): ScopedMcpServerConfig {
  return { ...rawConfig(command), scope: 'dynamic' } as ScopedMcpServerConfig
}

function connectedClient(
  name: string,
  config: ScopedMcpServerConfig,
  cleanup: () => Promise<void> = async () => {},
): MCPServerConnection {
  return {
    type: 'connected',
    name,
    config,
    capabilities: {},
    cleanup,
  }
}

afterEach(() => {
  __setMcpAuthClearRuntimeDepsForTest(null)
})

function failedClient(
  name: string,
  config: ScopedMcpServerConfig,
  error = 'connection failed',
): MCPServerConnection {
  return { type: 'failed', name, config, error }
}

function mcpPrompt(serverName: string, promptName: string): Command {
  return {
    type: 'prompt',
    name: `${getMcpPrefix(serverName)}${promptName}`,
    description: promptName,
    isMcp: true,
    getPromptForCommand: async () => [],
  } as Command
}

function mcpSkill(serverName: string, skillName: string): Command {
  return {
    type: 'prompt',
    name: `${serverName}:${skillName}`,
    description: skillName,
    loadedFrom: 'mcp',
    getPromptForCommand: async () => [],
  } as Command
}

function ordinaryCommand(name: string): Command {
  return {
    type: 'prompt',
    name,
    description: name,
    getPromptForCommand: async () => [],
  } as Command
}

function namedTool(name: string): Tools[number] {
  return { name } as Tools[number]
}

function mcpTool(serverName: string, toolName: string): Tools[number] {
  return namedTool(`${getMcpPrefix(serverName)}${toolName}`)
}

function stateHarness(input?: {
  clients?: MCPServerConnection[]
  tools?: Tools
  commands?: Command[]
  resources?: AppState['mcp']['resources']
}): {
  getState: () => AppState
  setAppState: (updater: (previous: AppState) => AppState) => void
} {
  const initial = getDefaultAppState()
  let state: AppState = {
    ...initial,
    mcp: {
      ...initial.mcp,
      clients: input?.clients ?? [],
      tools: input?.tools ?? [],
      commands: input?.commands ?? [],
      resources: input?.resources ?? initial.mcp.resources,
    },
  }
  return {
    getState: () => state,
    setAppState: updater => {
      state = updater(state)
    },
  }
}

function dependencies(
  overrides: Partial<McpServerManagementDependencies> = {},
): McpServerManagementDependencies {
  return {
    connectToServer: async (name, config) => connectedClient(name, config),
    evictExistingServerCache: async () => {},
    fetchToolsForClient: async () => [],
    fetchCommandsForClient: async () => [],
    ...overrides,
  }
}

const emptyDynamicState = (): DynamicMcpState => ({
  clients: [],
  tools: [],
  configs: {},
})

describe('dynamic MCP prompt reconciliation', () => {
  test('serializes mutation authority reads and recovers after rejection', async () => {
    const runMutation = createMcpMutationLane()
    const order: string[] = []
    let releaseFirst!: () => void
    const firstGate = new Promise<void>(resolve => {
      releaseFirst = resolve
    })
    let authority = 'old'

    const first = runMutation(async () => {
      order.push('first:start')
      await firstGate
      authority = 'new'
      order.push('first:end')
      throw new Error('first failed after committing authority')
    })
    const second = runMutation(async () => {
      order.push(`second:read:${authority}`)
      return authority
    })
    await Promise.resolve()
    expect(order).toEqual(['first:start'])

    releaseFirst()
    expect(await first.catch(error => String(error))).toBe(
      'Error: first failed after committing authority',
    )
    expect(await second).toBe('new')
    expect(order).toEqual([
      'first:start',
      'first:end',
      'second:read:new',
    ])
  })

  test('plans captured session policy deny and allow recovery without reviving disabled names', () => {
    const captured = {
      cli: rawConfig('cli'),
      disabled: rawConfig('disabled'),
      unrelated: rawConfig('unrelated'),
    }
    expect(
      planCapturedMcpPolicyTransitions(
        captured,
        new Set(['unrelated']),
        new Set(),
      ),
    ).toEqual({ toBlock: ['cli', 'disabled'], toRestore: [] })

    expect(
      planCapturedMcpPolicyTransitions(
        captured,
        new Set(Object.keys(captured)),
        new Set(['cli', 'disabled']),
        new Set(['disabled']),
      ),
    ).toEqual({ toBlock: [], toRestore: ['cli'] })
  })

  test('rejects stale OAuth completion after owner, endpoint, disable, or generation changes', () => {
    const baseline = {
      sameGeneration: true,
      expectedOwner: 'plugin' as const,
      currentOwner: null,
      authorizedServerKey: 'server-key-v1',
      currentServerKey: 'server-key-v1',
      disabled: false,
    }
    expect(canCommitDirectMcpOAuthGeneration(baseline)).toBe(true)
    expect(
      canCommitDirectMcpOAuthGeneration({
        ...baseline,
        currentOwner: 'plugin',
      }),
    ).toBe(true)
    expect(
      canCommitDirectMcpOAuthGeneration({
        ...baseline,
        currentOwner: 'public',
      }),
    ).toBe(false)
    expect(
      canCommitDirectMcpOAuthGeneration({
        ...baseline,
        currentServerKey: 'server-key-v2',
      }),
    ).toBe(false)
    expect(
      canCommitDirectMcpOAuthGeneration({
        ...baseline,
        sameGeneration: false,
      }),
    ).toBe(false)
    expect(
      canCommitDirectMcpOAuthGeneration({ ...baseline, disabled: true }),
    ).toBe(false)
  })

  test('adds fetched prompts while preserving non-target commands', async () => {
    const serverName = 'alpha'
    const nextPrompt = mcpPrompt(serverName, 'new-prompt')
    const otherPrompt = mcpPrompt('other', 'keep')
    const ordinarySamePrefix = ordinaryCommand(
      `${getMcpPrefix(serverName)}ordinary`,
    )
    const harness = stateHarness({
      commands: [otherPrompt, ordinarySamePrefix],
    })
    const fetchCommands = mock(async () => [nextPrompt])

    const result = await reconcileMcpServers(
      { [serverName]: rawConfig('new-command') },
      emptyDynamicState(),
      harness.setAppState,
      {
        syncPromptCommands: true,
        dependencies: dependencies({ fetchCommandsForClient: fetchCommands }),
      },
    )

    expect(result.response).toEqual({
      added: [serverName],
      removed: [],
      errors: {},
    })
    expect(fetchCommands).toHaveBeenCalledTimes(1)
    expect(harness.getState().mcp.commands).toEqual([
      otherPrompt,
      ordinarySamePrefix,
      nextPrompt,
    ])
  })

  test('replaces an old prompt with the prompt bound to the new client', async () => {
    const serverName = 'replace-me'
    const oldConfig = scopedConfig('old-command')
    const oldCleanup = mock(async () => {})
    const oldClient = connectedClient(serverName, oldConfig, oldCleanup)
    const oldPrompt = mcpPrompt(serverName, 'old-prompt')
    const oldSkill = mcpSkill(serverName, 'old-skill')
    const nextPrompt = mcpPrompt(serverName, 'new-prompt')
    const otherPrompt = mcpPrompt('other', 'keep')
    const harness = stateHarness({
      clients: [oldClient],
      tools: [mcpTool(serverName, 'old-tool')],
      commands: [oldPrompt, oldSkill, otherPrompt],
    })
    const evictCache = mock(async () => {
      await oldCleanup()
    })
    const nextClient = connectedClient(
      serverName,
      scopedConfig('new-command'),
    )
    const connect = mock(async () => nextClient)

    const result = await reconcileMcpServers(
      { [serverName]: rawConfig('new-command') },
      {
        clients: [oldClient],
        tools: [mcpTool(serverName, 'old-tool')],
        configs: { [serverName]: oldConfig },
      },
      harness.setAppState,
      {
        syncPromptCommands: true,
        dependencies: dependencies({
          connectToServer: connect,
          evictExistingServerCache: evictCache,
          fetchCommandsForClient: async () => [nextPrompt],
        }),
      },
    )

    expect(result.response.added).toEqual([serverName])
    expect(result.response.removed).toEqual([])
    expect(oldCleanup).toHaveBeenCalledTimes(1)
    expect(evictCache).toHaveBeenCalledTimes(1)
    expect(connect).toHaveBeenCalledTimes(1)
    expect(harness.getState().mcp.commands).toEqual([otherPrompt, nextPrompt])
    expect(result.newState.clients).toEqual([nextClient])
  })

  test('removes only the affected server prompt', async () => {
    const serverName = 'remove-me'
    const config = scopedConfig('old-command')
    const cleanup = mock(async () => {})
    const client = connectedClient(serverName, config, cleanup)
    const removedPrompt = mcpPrompt(serverName, 'gone')
    const removedSkill = mcpSkill(serverName, 'gone-skill')
    const ordinarySameNamespace = ordinaryCommand(
      `${serverName}:ordinary-command`,
    )
    const retainedPrompt = mcpPrompt('other', 'keep')
    const harness = stateHarness({
      clients: [client],
      commands: [
        removedPrompt,
        removedSkill,
        ordinarySameNamespace,
        retainedPrompt,
      ],
    })
    const connect = mock(async () => {
      throw new Error('remove must not connect')
    })
    const evictCache = mock(async () => {
      if (client.type === 'connected') await client.cleanup()
    })

    const result = await reconcileMcpServers(
      {},
      { clients: [client], tools: [], configs: { [serverName]: config } },
      harness.setAppState,
      {
        syncPromptCommands: true,
        dependencies: dependencies({
          connectToServer: connect,
          evictExistingServerCache: evictCache,
        }),
      },
    )

    expect(result.response.removed).toEqual([serverName])
    expect(evictCache).toHaveBeenCalledTimes(1)
    expect(cleanup).toHaveBeenCalledTimes(1)
    expect(connect).not.toHaveBeenCalled()
    expect(harness.getState().mcp.commands).toEqual([
      ordinarySameNamespace,
      retainedPrompt,
    ])
  })

  test('drops the stale prompt when a replacement connection fails', async () => {
    const serverName = 'failed-replacement'
    const oldConfig = scopedConfig('old-command')
    const oldClient = connectedClient(serverName, oldConfig)
    const oldPrompt = mcpPrompt(serverName, 'stale')
    const harness = stateHarness({
      clients: [oldClient],
      commands: [oldPrompt],
    })
    const fetchCommands = mock(async () => [mcpPrompt(serverName, 'invalid')])

    const result = await reconcileMcpServers(
      { [serverName]: rawConfig('new-command') },
      {
        clients: [oldClient],
        tools: [],
        configs: { [serverName]: oldConfig },
      },
      harness.setAppState,
      {
        syncPromptCommands: true,
        dependencies: dependencies({
          connectToServer: async (_name, config) =>
            failedClient(serverName, config),
          fetchCommandsForClient: fetchCommands,
        }),
      },
    )

    expect(result.response.errors).toEqual({
      [serverName]: 'connection failed',
    })
    expect(fetchCommands).not.toHaveBeenCalled()
    expect(harness.getState().mcp.commands).toEqual([])
  })

  test('defaults prompt sync off without fetching or replacing command identity', async () => {
    const existingCommands = [mcpPrompt('existing', 'keep')]
    const harness = stateHarness({ commands: existingCommands })
    const fetchCommands = mock(async () => [mcpPrompt('alpha', 'hidden')])

    await reconcileMcpServers(
      { alpha: rawConfig('new-command') },
      emptyDynamicState(),
      harness.setAppState,
      {
        dependencies: dependencies({ fetchCommandsForClient: fetchCommands }),
      },
    )

    expect(fetchCommands).not.toHaveBeenCalled()
    expect(harness.getState().mcp.commands).toBe(existingCommands)
  })

  test('uses the normalized MCP namespace for prompt and tool cleanup', async () => {
    const serverName = 'team alpha'
    const config = scopedConfig('old-command')
    const client = connectedClient(serverName, config)
    const normalizedPrompt = mcpPrompt(serverName, 'gone')
    const normalizedTool = mcpTool(serverName, 'gone')
    const rawLookalike = ordinaryCommand(`mcp__${serverName}__keep`)
    const harness = stateHarness({
      clients: [client],
      tools: [normalizedTool],
      commands: [normalizedPrompt, rawLookalike],
    })

    const result = await reconcileMcpServers(
      {},
      {
        clients: [client],
        tools: [],
        configs: { [serverName]: config },
      },
      harness.setAppState,
      {
        syncPromptCommands: true,
        strictWireNamespaceCleanup: true,
        dependencies: dependencies(),
      },
    )

    expect(result.newState.tools).toEqual([])
    expect(harness.getState().mcp.tools).toEqual([])
    expect(harness.getState().mcp.commands).toEqual([rawLookalike])
  })

  test('preserves the standard raw-prefix cleanup boundary by default', async () => {
    const serverName = 'team alpha'
    const config = scopedConfig('old-command')
    const client = connectedClient(serverName, config)
    const normalizedTool = mcpTool(serverName, 'legacy-collision')
    const harness = stateHarness({ clients: [client], tools: [normalizedTool] })

    await reconcileMcpServers(
      {},
      {
        clients: [client],
        tools: [],
        configs: { [serverName]: config },
      },
      harness.setAppState,
      { dependencies: dependencies() },
    )

    expect(harness.getState().mcp.tools).toEqual([normalizedTool])
  })

  test('cleans affected resources only on the opted-in direct route', async () => {
    const serverName = 'resource-owner'
    const config = scopedConfig('old-command')
    const client = connectedClient(serverName, config)
    const resources = {
      [serverName]: [{ uri: 'resource://stale', name: 'stale' }],
      other: [{ uri: 'resource://keep', name: 'keep' }],
    } as unknown as AppState['mcp']['resources']

    const standardHarness = stateHarness({ clients: [client], resources })
    await reconcileMcpServers(
      {},
      { clients: [client], tools: [], configs: { [serverName]: config } },
      standardHarness.setAppState,
      { dependencies: dependencies() },
    )
    expect(standardHarness.getState().mcp.resources).toBe(resources)

    const directHarness = stateHarness({ clients: [client], resources })
    await reconcileMcpServers(
      {},
      { clients: [client], tools: [], configs: { [serverName]: config } },
      directHarness.setAppState,
      { syncResourceCleanup: true, dependencies: dependencies() },
    )
    expect(directHarness.getState().mcp.resources).toEqual({
      other: resources.other,
    })
  })

  test('partitions startup process ownership so the first diff can remove it', async () => {
    const serverName = 'startup-plugin'
    const processConfig = scopedConfig('plugin-command')
    const processClient = connectedClient(serverName, processConfig)
    const ideConfig = {
      type: 'sse-ide',
      url: 'http://127.0.0.1:1234',
      ideName: 'test-ide',
      scope: 'dynamic',
    } as ScopedMcpServerConfig
    const ideClient: MCPServerConnection = {
      type: 'pending',
      name: 'ide',
      config: ideConfig,
    }
    const processTool = mcpTool(serverName, 'startup-tool')
    const ideTool = mcpTool('ide', 'keep')
    const builtinTool = namedTool('Read')
    const partition = partitionStartupProcessMcpState(
      [processClient, ideClient],
      [builtinTool, processTool, ideTool],
    )

    expect(partition.dynamicState).toEqual({
      clients: [processClient],
      tools: [processTool],
      configs: { [serverName]: processConfig },
    })
    expect(partition.remainingClients).toEqual([ideClient])
    expect(partition.remainingTools).toEqual([builtinTool, ideTool])

    const startupPrompt = mcpPrompt(serverName, 'startup-prompt')
    const harness = stateHarness({
      clients: [processClient, ideClient],
      tools: [processTool, ideTool],
      commands: [startupPrompt],
    })
    const result = await reconcileMcpServers(
      {},
      partition.dynamicState,
      harness.setAppState,
      {
        syncPromptCommands: true,
        dependencies: dependencies(),
      },
    )

    expect(result.response.removed).toEqual([serverName])
    expect(harness.getState().mcp.clients).toEqual([ideClient])
    expect(harness.getState().mcp.tools).toEqual([ideTool])
    expect(harness.getState().mcp.commands).toEqual([])
  })

  test('keeps public and plugin process owners isolated across replace and remove', async () => {
    const publicName = 'public-server'
    const pluginName = 'plugin-server'
    const publicConfig = scopedConfig('public-v1')
    const pluginConfig = scopedConfig('plugin-v1')
    const publicClient = connectedClient(publicName, publicConfig)
    const pluginClient = connectedClient(pluginName, pluginConfig)
    let publicState: DynamicMcpState = {
      clients: [publicClient],
      tools: [mcpTool(publicName, 'public-v1')],
      configs: { [publicName]: publicConfig },
    }
    let pluginState: DynamicMcpState = {
      clients: [pluginClient],
      tools: [mcpTool(pluginName, 'plugin-v1')],
      configs: { [pluginName]: pluginConfig },
    }
    const harness = stateHarness({
      clients: [publicClient, pluginClient],
      tools: [...publicState.tools, ...pluginState.tools],
      commands: [
        mcpPrompt(publicName, 'public-v1'),
        mcpPrompt(pluginName, 'plugin-v1'),
      ],
    })
    const ownerDependencies = dependencies({
      connectToServer: async (name, config) => connectedClient(name, config),
      fetchToolsForClient: async client => [
        mcpTool(
          client.name,
          client.config.type === 'stdio' || client.config.type === undefined
            ? client.config.command
            : 'tool',
        ),
      ],
      fetchCommandsForClient: async client => [
        mcpPrompt(
          client.name,
          client.config.type === 'stdio' || client.config.type === undefined
            ? client.config.command
            : 'prompt',
        ),
      ],
    })
    const applyOwner = async (
      owner: 'public' | 'plugin',
      desired: Record<string, McpServerConfigForProcessTransport>,
    ) => {
      const current = owner === 'public' ? publicState : pluginState
      const other = owner === 'public' ? pluginState : publicState
      const prepared = prepareMcpServersForOwner(
        owner,
        desired,
        new Set(Object.keys(other.configs)),
        new Set(),
      )
      const result = await reconcileMcpServers(
        prepared.reconciliationServers,
        current,
        harness.setAppState,
        {
          syncPromptCommands: true,
          dependencies: ownerDependencies,
        },
      )
      if (owner === 'public') publicState = result.newState
      else pluginState = result.newState
      return { result, prepared }
    }

    const publicReplace = await applyOwner('public', {
      [publicName]: rawConfig('public-v2'),
      [pluginName]: rawConfig('collision'),
    })
    expect(publicReplace.prepared.errors).toHaveProperty(pluginName)
    expect(Object.keys(publicState.configs)).toEqual([publicName])
    expect(Object.keys(pluginState.configs)).toEqual([pluginName])
    expect(harness.getState().mcp.commands.map(command => command.name)).toEqual([
      `${getMcpPrefix(pluginName)}plugin-v1`,
      `${getMcpPrefix(publicName)}public-v2`,
    ])

    const pluginReplace = await applyOwner('plugin', {
      [pluginName]: rawConfig('plugin-v2'),
      [publicName]: rawConfig('collision'),
    })
    expect(pluginReplace.prepared.errors).toHaveProperty(publicName)
    expect(harness.getState().mcp.commands.map(command => command.name)).toEqual([
      `${getMcpPrefix(publicName)}public-v2`,
      `${getMcpPrefix(pluginName)}plugin-v2`,
    ])
    expect(new Set(harness.getState().mcp.clients.map(client => client.name))).toEqual(
      new Set([publicName, pluginName]),
    )
    expect(harness.getState().mcp.clients).toHaveLength(2)

    await applyOwner('public', {})
    expect(harness.getState().mcp.clients.map(client => client.name)).toEqual([
      pluginName,
    ])
    expect(Object.keys(pluginState.configs)).toEqual([pluginName])

    await applyOwner('public', {
      [publicName]: rawConfig('public-v3'),
    })
    await applyOwner('plugin', {})
    expect(harness.getState().mcp.clients.map(client => client.name)).toEqual([
      publicName,
    ])
    expect(Object.keys(publicState.configs)).toEqual([publicName])
  })

  test('replays captured public desired after policy deny without deleting plugin state', async () => {
    const publicName = 'public-policy'
    const pluginName = 'plugin-policy'
    const publicConfig = scopedConfig('public')
    const pluginConfig = scopedConfig('plugin')
    const publicClient = connectedClient(publicName, publicConfig)
    const pluginClient = connectedClient(pluginName, pluginConfig)
    const pluginTool = mcpTool(pluginName, 'keep')
    const pluginPrompt = mcpPrompt(pluginName, 'keep')
    const harness = stateHarness({
      clients: [publicClient, pluginClient],
      tools: [mcpTool(publicName, 'old'), pluginTool],
      commands: [mcpPrompt(publicName, 'old'), pluginPrompt],
    })

    const denied = await reconcileMcpServers(
      {},
      {
        clients: [publicClient],
        tools: [mcpTool(publicName, 'old')],
        configs: { [publicName]: publicConfig },
      },
      harness.setAppState,
      {
        syncPromptCommands: true,
        strictWireNamespaceCleanup: true,
        dependencies: dependencies(),
      },
    )
    expect(harness.getState().mcp.clients).toEqual([pluginClient])
    expect(harness.getState().mcp.tools).toEqual([pluginTool])
    expect(harness.getState().mcp.commands).toEqual([pluginPrompt])

    const restoredPrompt = mcpPrompt(publicName, 'restored')
    const restoredTool = mcpTool(publicName, 'restored')
    await reconcileMcpServers(
      { [publicName]: rawConfig('public') },
      denied.newState,
      harness.setAppState,
      {
        syncPromptCommands: true,
        strictWireNamespaceCleanup: true,
        dependencies: dependencies({
          fetchToolsForClient: async () => [restoredTool],
          fetchCommandsForClient: async () => [restoredPrompt],
        }),
      },
    )
    expect(
      new Set(harness.getState().mcp.clients.map(client => client.name)),
    ).toEqual(new Set([pluginName, publicName]))
    expect(harness.getState().mcp.tools).toEqual([
      pluginTool,
      restoredTool,
    ])
    expect(harness.getState().mcp.commands).toEqual([
      pluginPrompt,
      restoredPrompt,
    ])
  })

  test('rejects direct SDK configs without dropping process siblings', () => {
    const prepared = prepareMcpServersForOwner(
      'public',
      {
        sdk: { type: 'sdk', name: 'handler' },
        process: rawConfig('healthy'),
      },
      new Set(),
    )

    expect(prepared.reconciliationServers).toEqual({
      process: rawConfig('healthy'),
    })
    expect(prepared.errors.sdk).toContain('not supported')
  })

  test('classifies a management-only SDK inventory row without guessing a missing transport', () => {
    expect(
      isDirectTuiSdkMcpInventoryRecord({ transport: 'sdk' }),
    ).toBe(true)
    expect(
      isDirectTuiSdkMcpInventoryRecord({
        transport: 'http',
        config: { type: 'sdk' },
      }),
    ).toBe(true)
    expect(isDirectTuiSdkMcpInventoryRecord({})).toBe(false)
    expect(
      isDirectTuiSdkMcpInventoryRecord({ transport: 'http' }),
    ).toBe(false)
  })

  test('keeps an existing public process owner unchanged when the same name is replaced by unsupported SDK input', async () => {
    const name = 'public-existing'
    const previous = { [name]: rawConfig('healthy') }
    const currentConfig = scopedConfig('healthy')
    const preserved = preservePublicProcessDesiredAcrossSdkRejections(
      {
        [name]: { type: 'sdk', name: 'unsupported' },
        sibling: rawConfig('sibling'),
      },
      previous,
      { [name]: currentConfig },
    )

    expect(preserved.desired).toEqual({
      [name]: previous[name],
      sibling: rawConfig('sibling'),
    })
    expect(preserved.retained).toEqual({ [name]: currentConfig })

    const cleanup = mock(async () => {})
    const client = connectedClient(name, currentConfig, cleanup)
    const harness = stateHarness({ clients: [client] })
    const result = await reconcileMcpServers(
      preserved.retained,
      {
        clients: [client],
        tools: [],
        configs: { [name]: currentConfig },
      },
      harness.setAppState,
      { dependencies: dependencies() },
    )
    expect(result.newState.clients).toEqual([client])
    expect(cleanup).not.toHaveBeenCalled()
  })

  test('keeps a logical public desired row without reviving it for rejected SDK input', () => {
    const name = 'public-inactive'
    const previous = { [name]: rawConfig('healthy') }
    const preserved = preservePublicProcessDesiredAcrossSdkRejections(
      { [name]: { type: 'sdk', name: 'unsupported' } },
      previous,
      {},
    )

    expect(preserved.desired).toEqual(previous)
    expect(preserved.retained).toEqual({})
  })

  test('reserves inactive startup CLI namespaces before lower-priority rows connect', () => {
    const reservation = scopedConfig('cli-disabled')
    const lowerCollision = scopedConfig('persisted-lower')
    const healthy = scopedConfig('healthy')
    const result = admitStartupMcpNamespaceReservations(
      { 'team alpha': reservation },
      {
        'team.alpha': lowerCollision,
        healthy,
      },
    )

    expect(result.acceptedReservationNames).toEqual(
      new Set(['team alpha']),
    )
    expect(result.acceptedActive).toEqual({ healthy })
    expect(result.errors['team.alpha']).toContain('team alpha')
  })

  test('returns an active CLI reservation as executable exactly once', () => {
    const cli = scopedConfig('cli-active')
    const result = admitStartupMcpNamespaceReservations(
      { cli },
      { cli },
    )

    expect(result.acceptedReservationNames).toEqual(new Set(['cli']))
    expect(result.acceptedActive).toEqual({ cli })
    expect(result.errors).toEqual({})
  })

  test('keeps direct clear-auth fresh resolution inside the captured owner', async () => {
    const captured = {
      type: 'http' as const,
      url: 'https://public.example/mcp',
      scope: 'dynamic' as const,
    }
    const foreign = {
      type: 'http' as const,
      url: 'https://foreign.example/mcp',
      scope: 'user' as const,
    }
    const evicted: string[] = []
    __setMcpAuthClearRuntimeDepsForTest({
      revokeServerTokens: mock(async () => {}),
      getActiveMcpConfigByName: mock(async () => foreign),
      evictExistingServerCache: mock(async (_name, config) => {
        evicted.push(config.url)
      }),
    })

    const directResult = await clearMcpAuthenticationRuntime(
      'shared-name',
      captured,
      { resolveFreshConfig: async () => captured },
    )
    expect(directResult).toEqual(captured)
    expect(evicted).toEqual([captured.url])

    evicted.length = 0
    const standardResult = await clearMcpAuthenticationRuntime(
      'shared-name',
      captured,
    )
    expect(standardResult).toEqual(foreign)
    expect(evicted).toEqual([captured.url, foreign.url])
  })

  test('keeps an exact captured fixed name unavailable to a plugin claim', () => {
    const result = prepareMcpServersForOwner(
      'plugin',
      { fixed: rawConfig('plugin') },
      new Set(),
      new Set(['fixed']),
    )
    expect(result.reconciliationServers).toEqual({})
    expect(result.errors.fixed).toContain('already owned')
  })

  test('preserves startup scope through no-op reconciliation and removal', async () => {
    const name = 'startup-user'
    const userConfig = {
      ...rawConfig('user-command'),
      scope: 'user' as const,
    } as ScopedMcpServerConfig
    const client = connectedClient(name, userConfig)
    const harness = stateHarness({ clients: [client] })
    const evicted: ScopedMcpServerConfig[] = []
    const noOp = await reconcileMcpServers(
      {
        [name]: userConfig as McpServerConfigForProcessTransport,
      },
      { clients: [client], tools: [], configs: { [name]: userConfig } },
      harness.setAppState,
      {
        preserveDesiredScopes: true,
        dependencies: dependencies(),
      },
    )
    expect(noOp.newState.configs[name]).toBe(userConfig)

    await reconcileMcpServers({}, noOp.newState, harness.setAppState, {
      dependencies: dependencies({
        evictExistingServerCache: async (_name, config) => {
          evicted.push(config)
        },
      }),
    })
    expect(evicted).toEqual([userConfig])
  })

  test('preserves supplied user/project scope for newly added and replaced plugin configs', async () => {
    const harness = stateHarness()
    const userConfig = {
      ...rawConfig('user-command'),
      scope: 'user' as const,
    } as McpServerConfigForProcessTransport
    const added = await reconcileMcpServers(
      { plugin: userConfig },
      emptyDynamicState(),
      harness.setAppState,
      { preserveDesiredScopes: true, dependencies: dependencies() },
    )
    expect(added.newState.configs.plugin?.scope).toBe('user')

    const projectConfig = {
      ...rawConfig('project-command'),
      scope: 'project' as const,
    } as McpServerConfigForProcessTransport
    const replaced = await reconcileMcpServers(
      { plugin: projectConfig },
      added.newState,
      harness.setAppState,
      { preserveDesiredScopes: true, dependencies: dependencies() },
    )
    expect(replaced.newState.configs.plugin?.scope).toBe('project')
  })

  test('treats a scope-only provenance change as a replacement', async () => {
    const name = 'scope-only'
    const userConfig = {
      ...rawConfig('same-command'),
      scope: 'user' as const,
    } as ScopedMcpServerConfig
    const projectConfig = {
      ...rawConfig('same-command'),
      scope: 'project' as const,
    } as McpServerConfigForProcessTransport
    const oldClient = connectedClient(name, userConfig)
    const nextClient = connectedClient(
      name,
      projectConfig as ScopedMcpServerConfig,
    )
    const evict = mock(async () => {})
    const connect = mock(async () => nextClient)
    const harness = stateHarness({ clients: [oldClient] })

    const result = await reconcileMcpServers(
      { [name]: projectConfig },
      { clients: [oldClient], tools: [], configs: { [name]: userConfig } },
      harness.setAppState,
      {
        preserveDesiredScopes: true,
        dependencies: dependencies({
          evictExistingServerCache: evict,
          connectToServer: connect,
        }),
      },
    )

    expect(evict).toHaveBeenCalledTimes(1)
    expect(connect).toHaveBeenCalledTimes(1)
    expect(result.newState.configs[name]?.scope).toBe('project')
    expect(result.newState.clients).toEqual([nextClient])
  })

  test('retains CLI startup process owners outside plugin desired state', () => {
    const cliProcess = connectedClient('cli-process', scopedConfig('cli'))
    const pluginProcess = connectedClient(
      'plugin-process',
      scopedConfig('plugin'),
    )
    const cliTool = mcpTool('cli-process', 'tool')
    const pluginTool = mcpTool('plugin-process', 'tool')
    const partition = partitionStartupProcessMcpState(
      [cliProcess, pluginProcess],
      [cliTool, pluginTool],
      new Set(['cli-process']),
    )
    expect(partition.remainingClients).toEqual([cliProcess])
    expect(partition.remainingTools).toEqual([cliTool])
    expect(Object.keys(partition.dynamicState.configs)).toEqual([
      'plugin-process',
    ])

  })

  test('fails wire namespace collisions closed across owners and within one desired set', async () => {
    const originalName = 'foo bar'
    const collidingName = 'foo.bar'
    const originalConfig = scopedConfig('original')
    const originalClient = connectedClient(originalName, originalConfig)
    const originalTool = mcpTool(originalName, 'tool')
    const originalPrompt = mcpPrompt(originalName, 'prompt')
    const harness = stateHarness({
      clients: [originalClient],
      tools: [originalTool],
      commands: [originalPrompt],
    })
    const crossOwner = prepareMcpServersForOwner(
      'public',
      { [collidingName]: rawConfig('collision') },
      new Set([originalName]),
      new Set(),
    )
    expect(crossOwner.reconciliationServers).toEqual({})
    expect(crossOwner.errors[collidingName]).toContain('wire namespace')
    await reconcileMcpServers(
      crossOwner.reconciliationServers,
      emptyDynamicState(),
      harness.setAppState,
      { dependencies: dependencies() },
    )
    expect(harness.getState().mcp.clients).toEqual([originalClient])
    expect(harness.getState().mcp.tools).toEqual([originalTool])
    expect(harness.getState().mcp.commands).toEqual([originalPrompt])

    const sameOwner = prepareMcpServersForOwner(
      'public',
      {
        [originalName]: rawConfig('first'),
        [collidingName]: rawConfig('second'),
      },
      new Set(),
      new Set(),
    )
    expect(Object.keys(sameOwner.reconciliationServers)).toEqual([
      originalName,
    ])
    expect(sameOwner.errors[collidingName]).toContain('wire namespace')
  })

  test('applies explicit startup and config-scope precedence before namespace first-wins', () => {
    const cli = { ...rawConfig('cli'), scope: 'dynamic' as const }
    const plugin = {
      ...rawConfig('plugin'),
      scope: 'user' as const,
      pluginMcp: { pluginName: 'fixture' },
    }
    const user = { ...rawConfig('user'), scope: 'user' as const }
    const ordered = orderMcpServersByPrecedence({
      'foo bar': plugin,
      'foo.bar': user,
    })
    expect(Object.keys(ordered)).toEqual(['foo.bar', 'foo bar'])
    expect(Object.keys(filterMcpServersByWireNamespace(ordered).accepted)).toEqual([
      'foo.bar',
    ])

    const cliFirst = filterMcpServersByWireNamespace({
      'foo.bar': cli,
      'foo bar': user,
    })
    expect(Object.keys(cliFirst.accepted)).toEqual(['foo.bar'])
  })
})
