await import('../setup.js')

import type { Tools } from '../../src/Tool.js'
import type { AppState } from '../../src/state/AppStateStore.js'
import type {
  MCPServerConnection,
  ScopedMcpServerConfig,
} from '../../src/services/mcp/types.js'
import type { StructuredIoOutboundMessage } from '../../src/cli/structuredIO.js'

type Scenario =
  | 'public-sdk-collision'
  | 'fixed-policy-lane'
  | 'bare-fixed-public-policy'
  | 'management-sdk-failclosed'
  | 'late-fixed-logical-collision'
  | 'settings-public-toggle-race'
  | 'settings-public-desired-race'
  | 'startup-persisted-disabled-public'

type SideEffects = {
  connect: string[]
  reconnect: string[]
  evict: string[]
  setEnabled: Array<{ name: string; enabled: boolean }>
  authorizeOAuth: string[]
  performOAuth: string[]
  clearAuth: string[]
}

type Evidence = {
  scenario: Scenario
  frames: StructuredIoOutboundMessage[]
  finalClients: Array<{
    name: string
    type: MCPServerConnection['type']
    command?: string
  }>
  events: string[]
  sideEffects: SideEffects
}

const scenario = process.env.DIRECT_TUI_MCP_CONTROL_SCENARIO as
  | Scenario
  | undefined
if (!scenario) {
  throw new Error('DIRECT_TUI_MCP_CONTROL_SCENARIO is required')
}
// runHeadlessDirectTui installs the production stdout guard. Retain the
// process-owned writer before that installation so the fixture can append one
// final evidence frame after the real StructuredIO stream has closed.
const writeFixtureEvidence = process.stdout.write.bind(process.stdout)

const { spyOn } = await import('bun:test')
const clientModule = await import('../../src/services/mcp/client.js')
const configModule = await import('../../src/services/mcp/config.js')
const mcpAuthModule = await import('../../src/services/mcp/auth.js')
const clearAuthModule = await import(
  '../../src/services/mcp/mcpAuthClearRuntime.js'
)
const changeDetectorModule = await import(
  '../../src/utils/settings/changeDetector.js'
)
const bootstrapState = await import('../../src/bootstrap/state.js')
const { StructuredIO } = await import('../../src/cli/structuredIO.js')
const { buildPluginMcpRuntimeName } = await import(
  '../../src/services/mcp/pluginMcpIdentity.js'
)

const events: string[] = []
const frames: StructuredIoOutboundMessage[] = []
const sideEffects: SideEffects = {
  connect: [],
  reconnect: [],
  evict: [],
  setEnabled: [],
  authorizeOAuth: [],
  performOAuth: [],
  clearAuth: [],
}

const originalWrite = StructuredIO.prototype.write
spyOn(StructuredIO.prototype, 'write').mockImplementation(async function (
  this: StructuredIO,
  message: StructuredIoOutboundMessage,
) {
  frames.push(structuredClone(message))
  await originalWrite.call(this, message)
})

function stdioConfig(command: string): ScopedMcpServerConfig {
  return {
    type: 'stdio',
    command,
    args: [],
    scope: 'dynamic',
  }
}

function connectedClient(
  name: string,
  config: ScopedMcpServerConfig,
): MCPServerConnection {
  return {
    type: 'connected',
    name,
    config,
    capabilities: {},
    cleanup: async () => {},
  }
}

function successfulReconnect(
  name: string,
  config: ScopedMcpServerConfig,
): Awaited<ReturnType<typeof clientModule.reconnectMcpServerImpl>> {
  return {
    client: connectedClient(name, config),
    tools: [] as Tools,
    commands: [],
    resources: [],
  }
}

spyOn(clientModule, 'connectToServer').mockImplementation(
  async (name, config) => {
    sideEffects.connect.push(name)
    if (
      scenario === 'settings-public-desired-race' &&
      name === 'public' &&
      (config.type === 'stdio' || config.type === undefined) &&
      config.command === 'public-v2'
    ) {
      events.push('connect:v2:start')
      await new Promise(resolve => setTimeout(resolve, 50))
      events.push('connect:v2:end')
    }
    return connectedClient(name, config)
  },
)
spyOn(clientModule, 'evictExistingServerCache').mockImplementation(
  async name => {
    sideEffects.evict.push(name)
    events.push(`evict:start:${name}`)
    if (
      (scenario === 'settings-public-toggle-race' ||
        scenario === 'settings-public-desired-race') &&
      name === 'fixed'
    ) {
      await new Promise(resolve => setTimeout(resolve, 50))
    }
    events.push(`evict:${name}`)
  },
)
spyOn(clientModule, 'fetchToolsForClient').mockImplementation(async () => [])
spyOn(clientModule, 'fetchCommandsForClient').mockImplementation(
  async () => [],
)

const originalSetMcpServerEnabled = configModule.setMcpServerEnabled
spyOn(configModule, 'setMcpServerEnabled').mockImplementation(
  async (name, enabled) => {
    sideEffects.setEnabled.push({ name, enabled })
    await originalSetMcpServerEnabled(name, enabled)
  },
)
spyOn(configModule, 'authorizeMcpOAuthStart').mockImplementation(
  async name => {
    sideEffects.authorizeOAuth.push(name)
    return {
      allowed: false,
      reasonCode: 'not-found',
      message: `fixture refused OAuth for ${name}`,
    }
  },
)
spyOn(mcpAuthModule, 'performMCPOAuthFlow').mockImplementation(
  async serverName => {
    sideEffects.performOAuth.push(serverName)
    throw new Error('fixture OAuth must not start')
  },
)
spyOn(clearAuthModule, 'clearMcpAuthenticationRuntime').mockImplementation(
  async serverName => {
    sideEffects.clearAuth.push(serverName)
    throw new Error('fixture clear-auth must not run')
  },
)

let reconnectCount = 0
spyOn(clientModule, 'reconnectMcpServerImpl').mockImplementation(
  async (name, config) => {
    reconnectCount += 1
    sideEffects.reconnect.push(name)
    events.push(`reconnect:start:${reconnectCount}`)

    if (scenario === 'fixed-policy-lane' && reconnectCount === 1) {
      // A settings change arrives while reconnect owns the direct mutation
      // lane. The real settings subscriber must queue behind it, re-read the
      // new policy, then evict the just-reconnected fixed owner.
      bootstrapState.setFlagSettingsInline({
        deniedMcpServers: [{ serverName: name }],
      })
      changeDetectorModule.settingsChangeDetector.notifyChange('flagSettings')
      events.push('policy:deny-notified')
    }

    events.push(`reconnect:end:${reconnectCount}`)
    return successfulReconnect(name, config)
  },
)

if (scenario === 'management-sdk-failclosed') {
  const runtimeName = buildPluginMcpRuntimeName(
    'fixture-plugin',
    'fixture-source',
    'sdk-server',
  )
  spyOn(configModule, 'getCrabCodeMcpConfigs').mockResolvedValue({
    servers: {},
    errors: [],
    pluginInventory: [
      {
        runtimeName,
        transport: 'sdk',
        config: { type: 'sdk', name: 'fixture-handler' },
      },
    ],
  } as Awaited<ReturnType<typeof configModule.getCrabCodeMcpConfigs>>)
}

// Import after the external-boundary spies above. mcpServerManagement captures
// its default dependency functions during module evaluation; this ordering
// keeps the production control loop intact while making the fixture hermetic.
const { runHeadlessDirectTui } = await import(
  '../../src/cli/print/queryExecutionCore.js'
)
const { getDefaultAppState } = await import(
  '../../src/state/AppStateStore.js'
)

bootstrapState.resetStateForTests()
bootstrapState.setSessionPersistenceDisabled(true)
bootstrapState.setIsInteractive(true)

let appState = getDefaultAppState()
const stateWaiters: Array<{
  predicate: (state: AppState) => boolean
  resolve: () => void
}> = []

function settleStateWaiters(): void {
  for (let index = stateWaiters.length - 1; index >= 0; index -= 1) {
    const waiter = stateWaiters[index]
    if (waiter?.predicate(appState)) {
      stateWaiters.splice(index, 1)
      waiter.resolve()
    }
  }
}

function setAppState(updater: (previous: AppState) => AppState): void {
  appState = updater(appState)
  settleStateWaiters()
}

async function waitForState(
  predicate: (state: AppState) => boolean,
  label: string,
): Promise<void> {
  if (predicate(appState)) return
  let timeout: ReturnType<typeof setTimeout> | undefined
  try {
    await Promise.race([
      new Promise<void>(resolve => {
        stateWaiters.push({ predicate, resolve })
      }),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`timed out waiting for ${label}`)),
          5_000,
        )
      }),
    ])
  } finally {
    if (timeout) clearTimeout(timeout)
  }
}

async function waitForEvent(expected: string): Promise<void> {
  const deadline = Date.now() + 5_000
  while (!events.includes(expected)) {
    if (Date.now() >= deadline) {
      throw new Error(`timed out waiting for event: ${expected}`)
    }
    await new Promise(resolve => setTimeout(resolve, 5))
  }
}

function hasClient(name: string, type?: MCPServerConnection['type']): boolean {
  return appState.mcp.clients.some(
    client => client.name === name && (type === undefined || client.type === type),
  )
}

function request(
  requestId: string,
  payload: Record<string, unknown>,
): string {
  return `${JSON.stringify({
    type: 'control_request',
    request_id: requestId,
    request: payload,
  })}\n`
}

async function* publicSdkCollisionInput(): AsyncGenerator<string> {
  yield request('set-process', {
    subtype: 'mcp_set_servers',
    servers: {
      same: { type: 'stdio', command: 'process-v1', args: [] },
    },
  })
  yield request('set-same-name-sdk', {
    subtype: 'mcp_set_servers',
    servers: {
      same: { type: 'sdk', name: 'fixture-handler' },
    },
  })
  yield request('status-after-sdk-rejection', { subtype: 'mcp_status' })

  yield request('disable-process', {
    subtype: 'mcp_toggle',
    serverName: 'same',
    enabled: false,
  })
  await waitForState(
    state => !state.mcp.clients.some(client => client.name === 'same'),
    'the public owner to become logically disabled',
  )
  yield request('set-process-v2-while-disabled', {
    subtype: 'mcp_set_servers',
    servers: {
      same: { type: 'stdio', command: 'process-v2', args: [] },
    },
  })
  yield request('status-after-disabled-process-update', {
    subtype: 'mcp_status',
  })
  yield request('set-sdk-while-disabled', {
    subtype: 'mcp_set_servers',
    servers: {
      same: { type: 'sdk', name: 'fixture-handler' },
    },
  })
  events.push('sdk-while-disabled:settled')
  yield request('status-after-disabled-sdk-rejection', {
    subtype: 'mcp_status',
  })

  yield request('enable-process', {
    subtype: 'mcp_toggle',
    serverName: 'same',
    enabled: true,
  })
  await waitForState(
    state => hasClient('same', 'connected'),
    'the public owner to reconnect after enable',
  )
  yield request('status-after-enable', { subtype: 'mcp_status' })
  yield request('deny-process', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [{ serverName: 'same' }] },
  })
  await waitForState(
    state => !state.mcp.clients.some(client => client.name === 'same'),
    'the public owner to settle as policy-blocked',
  )
  yield request('set-sdk-while-policy-blocked', {
    subtype: 'mcp_set_servers',
    servers: {
      same: { type: 'sdk', name: 'fixture-handler' },
    },
  })
  events.push('sdk-while-policy-blocked:settled')
  yield request('status-after-policy-sdk-rejection', {
    subtype: 'mcp_status',
  })
  yield request('end', { subtype: 'end_session' })
}

let resolveLateFixedMcpConfig:
  | ((value: { name: string; config: ScopedMcpServerConfig }) => void)
  | undefined

async function* lateFixedLogicalCollisionInput(): AsyncGenerator<string> {
  yield request('set-logical-public', {
    subtype: 'mcp_set_servers',
    servers: {
      team_alpha: {
        type: 'stdio',
        command: 'public-owner',
        args: [],
      },
    },
  })
  yield request('deny-logical-public', {
    subtype: 'apply_flag_settings',
    settings: {
      deniedMcpServers: [{ serverName: 'team_alpha' }],
    },
  })
  await waitForState(
    state => !state.mcp.clients.some(client => client.name === 'team_alpha'),
    'the public owner to settle as logical-only',
  )

  resolveLateFixedMcpConfig?.({
    name: 'team.alpha',
    config: stdioConfig('late-ide'),
  })
  // Let the promise continuation enqueue its admission before this real public
  // mutation becomes a deterministic barrier behind it in the same lane.
  await Promise.resolve()
  yield request('late-fixed-lane-barrier', {
    subtype: 'mcp_set_servers',
    servers: {
      team_alpha: {
        type: 'stdio',
        command: 'public-owner',
        args: [],
      },
    },
  })
  yield request('status-after-late-fixed-rejection', {
    subtype: 'mcp_status',
  })
  yield request('end', { subtype: 'end_session' })
}

async function* settingsPublicToggleRaceInput(): AsyncGenerator<string> {
  yield request('race-set-public', {
    subtype: 'mcp_set_servers',
    servers: {
      public: { type: 'stdio', command: 'public-v1', args: [] },
    },
  })
  yield request('race-disable-public', {
    subtype: 'mcp_toggle',
    serverName: 'public',
    enabled: false,
  })
  await waitForState(
    state => !state.mcp.clients.some(client => client.name === 'public'),
    'the race public owner to become disabled',
  )

  yield request('race-deny-fixed', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [{ serverName: 'fixed' }] },
  })
  await waitForEvent('evict:start:fixed')
  yield request('race-enable-public', {
    subtype: 'mcp_toggle',
    serverName: 'public',
    enabled: true,
  })
  // The settings worker was queued behind the same slow fixed-owner mutation.
  // Let its public-owner continuation settle before observing final status.
  await new Promise(resolve => setTimeout(resolve, 25))
  yield request('race-final-status', { subtype: 'mcp_status' })
  yield request('end', { subtype: 'end_session' })
}

async function* settingsPublicDesiredRaceInput(): AsyncGenerator<string> {
  yield request('desired-race-set-v1', {
    subtype: 'mcp_set_servers',
    servers: {
      public: { type: 'stdio', command: 'public-v1', args: [] },
    },
  })
  yield request('desired-race-deny-fixed', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [{ serverName: 'fixed' }] },
  })
  await waitForEvent('evict:start:fixed')
  yield request('desired-race-set-v2', {
    subtype: 'mcp_set_servers',
    servers: {
      public: { type: 'stdio', command: 'public-v2', args: [] },
    },
  })
  await new Promise(resolve => setTimeout(resolve, 25))
  yield request('desired-race-final-status', { subtype: 'mcp_status' })
  yield request('end', { subtype: 'end_session' })
}

async function* startupPersistedDisabledPublicInput(): AsyncGenerator<string> {
  yield request('persisted-disabled-set-public', {
    subtype: 'mcp_set_servers',
    servers: {
      'public-x': {
        type: 'stdio',
        command: 'persisted-disabled',
        args: [],
      },
    },
  })
  yield request('persisted-disabled-status', { subtype: 'mcp_status' })
  yield request('persisted-disabled-reconnect', {
    subtype: 'mcp_reconnect',
    serverName: 'public-x',
  })
  yield request('persisted-disabled-status-after-reconnect', {
    subtype: 'mcp_status',
  })
  yield request('persisted-disabled-toggle-on', {
    subtype: 'mcp_toggle',
    serverName: 'public-x',
    enabled: true,
  })
  await waitForState(
    state => hasClient('public-x', 'connected'),
    'the explicitly enabled public owner to connect',
  )
  yield request('persisted-disabled-status-after-enable', {
    subtype: 'mcp_status',
  })
  yield request('end', { subtype: 'end_session' })
}

async function* fixedPolicyLaneInput(): AsyncGenerator<string> {
  yield request('reconnect-raced-by-policy', {
    subtype: 'mcp_reconnect',
    serverName: 'fixed',
  })
  await waitForState(
    state => !state.mcp.clients.some(client => client.name === 'fixed'),
    'the queued deny policy to evict fixed',
  )
  yield request('status-after-race', { subtype: 'mcp_status' })

  yield request('allow-fixed', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [] },
  })
  await waitForState(
    state =>
      state.mcp.clients.some(
        client => client.name === 'fixed' && client.type === 'connected',
      ),
    'the allow policy to restore fixed',
  )
  yield request('status-after-restore', { subtype: 'mcp_status' })

  yield request('disable-fixed', {
    subtype: 'mcp_toggle',
    serverName: 'fixed',
    enabled: false,
  })
  await waitForState(
    state =>
      state.mcp.clients.some(
        client => client.name === 'fixed' && client.type === 'disabled',
      ),
    'fixed to become explicitly disabled',
  )
  yield request('deny-disabled-fixed', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [{ serverName: 'fixed' }] },
  })
  await waitForState(
    state => !state.mcp.clients.some(client => client.name === 'fixed'),
    'the deny policy to remove disabled fixed',
  )
  yield request('allow-disabled-fixed', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [] },
  })
  // A real public mutation queues after the settings policy transition and is
  // therefore a deterministic lane barrier. It does not own or alter `fixed`.
  yield request('policy-lane-barrier', {
    subtype: 'mcp_set_servers',
    servers: {},
  })
  yield request('status-after-disabled-allow', { subtype: 'mcp_status' })
  yield request('end', { subtype: 'end_session' })
}

async function* bareFixedPublicPolicyInput(): AsyncGenerator<string> {
  yield request('set-bare-public', {
    subtype: 'mcp_set_servers',
    servers: {
      public: { type: 'stdio', command: 'bare-public', args: [] },
    },
  })
  await waitForState(
    state =>
      state.mcp.clients.some(client => client.name === 'fixed') &&
      state.mcp.clients.some(client => client.name === 'public'),
    'bare fixed and public owners to become live',
  )

  const denyBoth = {
    deniedMcpServers: [
      { serverName: 'fixed' },
      { serverName: 'public' },
    ],
  }
  yield request('bare-deny-both', {
    subtype: 'apply_flag_settings',
    settings: denyBoth,
  })
  await waitForState(
    state =>
      !state.mcp.clients.some(
        client => client.name === 'fixed' || client.name === 'public',
      ),
    'bare policy to remove fixed and public owners',
  )
  yield request('bare-status-denied', { subtype: 'mcp_status' })

  yield request('bare-allow-both', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [] },
  })
  await waitForState(
    state =>
      state.mcp.clients.some(
        client => client.name === 'fixed' && client.type === 'connected',
      ) &&
      state.mcp.clients.some(
        client => client.name === 'public' && client.type === 'connected',
      ),
    'bare policy to restore fixed and public owners',
  )
  yield request('bare-status-restored', { subtype: 'mcp_status' })

  yield request('bare-disable-fixed', {
    subtype: 'mcp_toggle',
    serverName: 'fixed',
    enabled: false,
  })
  await waitForState(
    state =>
      state.mcp.clients.some(
        client => client.name === 'fixed' && client.type === 'disabled',
      ),
    'bare fixed owner to become explicitly disabled',
  )
  yield request('bare-deny-after-disable', {
    subtype: 'apply_flag_settings',
    settings: denyBoth,
  })
  await waitForState(
    state =>
      !state.mcp.clients.some(
        client => client.name === 'fixed' || client.name === 'public',
      ),
    'bare policy to remove the disabled fixed and live public owners',
  )
  yield request('bare-allow-after-disable', {
    subtype: 'apply_flag_settings',
    settings: { deniedMcpServers: [] },
  })
  await waitForState(
    state =>
      !state.mcp.clients.some(client => client.name === 'fixed') &&
      state.mcp.clients.some(
        client => client.name === 'public' && client.type === 'connected',
      ),
    'bare allow to restore public without reviving disabled fixed',
  )
  yield request('bare-status-after-disabled-allow', {
    subtype: 'mcp_status',
  })
  yield request('end', { subtype: 'end_session' })
}

async function* managementSdkFailclosedInput(): AsyncGenerator<string> {
  const runtimeName = buildPluginMcpRuntimeName(
    'fixture-plugin',
    'fixture-source',
    'sdk-server',
  )
  yield request('sdk-reconnect', {
    subtype: 'mcp_reconnect',
    serverName: runtimeName,
  })
  yield request('sdk-toggle', {
    subtype: 'mcp_toggle',
    serverName: runtimeName,
    enabled: true,
  })
  yield request('sdk-authenticate', {
    subtype: 'mcp_authenticate',
    serverName: runtimeName,
  })
  yield request('sdk-clear-auth', {
    subtype: 'mcp_clear_auth',
    serverName: runtimeName,
  })
  yield request('end', { subtype: 'end_session' })
}

let runtimeInput: AsyncIterable<string>
let startupSessionMcpServerNames: string[] = []
let startupSessionMcpServers: Record<string, ScopedMcpServerConfig> = {}
let lateFixedMcpConfig:
  | Promise<{ name: string; config: ScopedMcpServerConfig } | null>
  | undefined

switch (scenario) {
  case 'public-sdk-collision':
    runtimeInput = publicSdkCollisionInput()
    break
  case 'fixed-policy-lane': {
    const config = stdioConfig('fixed-v1')
    const client = connectedClient('fixed', config)
    appState = {
      ...appState,
      mcp: { ...appState.mcp, clients: [client] },
    }
    startupSessionMcpServerNames = ['fixed']
    startupSessionMcpServers = { fixed: config }
    runtimeInput = fixedPolicyLaneInput()
    break
  }
  case 'bare-fixed-public-policy': {
    const config = stdioConfig('bare-fixed')
    const client = connectedClient('fixed', config)
    appState = {
      ...appState,
      mcp: { ...appState.mcp, clients: [client] },
    }
    startupSessionMcpServerNames = ['fixed']
    startupSessionMcpServers = { fixed: config }
    runtimeInput = bareFixedPublicPolicyInput()
    break
  }
  case 'management-sdk-failclosed':
    runtimeInput = managementSdkFailclosedInput()
    break
  case 'late-fixed-logical-collision':
    lateFixedMcpConfig = new Promise(resolve => {
      resolveLateFixedMcpConfig = resolve
    })
    runtimeInput = lateFixedLogicalCollisionInput()
    break
  case 'settings-public-toggle-race': {
    const config = stdioConfig('fixed-v1')
    const client = connectedClient('fixed', config)
    appState = {
      ...appState,
      mcp: { ...appState.mcp, clients: [client] },
    }
    startupSessionMcpServerNames = ['fixed']
    startupSessionMcpServers = { fixed: config }
    runtimeInput = settingsPublicToggleRaceInput()
    break
  }
  case 'settings-public-desired-race': {
    const config = stdioConfig('fixed-v1')
    const client = connectedClient('fixed', config)
    appState = {
      ...appState,
      mcp: { ...appState.mcp, clients: [client] },
    }
    startupSessionMcpServerNames = ['fixed']
    startupSessionMcpServers = { fixed: config }
    runtimeInput = settingsPublicDesiredRaceInput()
    break
  }
  case 'startup-persisted-disabled-public':
    await originalSetMcpServerEnabled('public-x', false)
    runtimeInput = startupPersistedDisabledPublicInput()
    break
}

await runHeadlessDirectTui(
  runtimeInput,
  () => appState,
  setAppState,
  [],
  [],
  {},
  [],
  {
    continue: false,
    resume: undefined,
    resumeSessionAt: undefined,
    verbose: true,
    outputFormat: 'stream-json',
    allowedTools: [],
    maxTurns: 1,
    taskBudget: undefined,
    startupSessionMcpServerNames,
    startupSessionMcpServers,
    startupPolicyBlockedMcpServerNames: [],
    lateFixedMcpConfig,
  },
)

const evidence: Evidence = {
  scenario,
  frames,
  finalClients: appState.mcp.clients.map(client => ({
    name: client.name,
    type: client.type,
    command:
      client.config.type === 'stdio' || client.config.type === undefined
        ? client.config.command
        : undefined,
  })),
  events,
  sideEffects,
}

writeFixtureEvidence(`${JSON.stringify(evidence)}\n`)
