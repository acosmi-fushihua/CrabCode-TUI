await import('../setup.js')

import { spyOn } from 'bun:test'

const [
  { QueryEngine },
  state,
  { getDefaultAppState },
  catalog,
] = await Promise.all([
  import('../../src/QueryEngine.js'),
  import('../../src/bootstrap/state.js'),
  import('../../src/state/AppStateStore.js'),
  import('../../src/cli/headlessCommands.js'),
])

state.resetStateForTests()
state.setSessionPersistenceDisabled(true)
state.setIsInteractive(true)

const abortController = new AbortController()
if (process.env.DIRECT_TUI_CLEAR_PREABORT === '1') {
  abortController.abort(new DOMException('fixture pre-abort', 'AbortError'))
}

let clearConversationSpy: { mockRestore(): void } | undefined
if (process.env.DIRECT_TUI_CLEAR_FAILURE === '1') {
  const transaction = await import(
    '../../src/commands/clear/conversation.js'
  )
  clearConversationSpy = spyOn(
    transaction,
    'clearConversation',
  ).mockRejectedValue(new Error('fixture clear transaction failed'))
}

let appState = getDefaultAppState()
const commands = await catalog.getDirectTuiCommands(process.cwd())
const oldSessionId = state.getSessionId()
const directEvents: unknown[] = []
const engine = new QueryEngine({
  cwd: process.cwd(),
  tools: [],
  commands,
  mcpClients: [],
  agents: [],
  canUseTool: async () => ({
    behavior: 'deny',
    message: 'clear fixture does not execute model tools',
  }),
  getAppState: () => appState,
  setAppState: update => {
    appState = update(appState)
  },
  readFileCache: new Map(),
  interactive: true,
  querySource: 'repl_main_thread',
  abortController,
  onQueryEvent: event => {
    directEvents.push(event)
  },
})

const envelopes: unknown[] = []
for await (const envelope of engine.submitMessage('/clear')) {
  envelopes.push(envelope)
}

const newSessionId = state.getSessionId()
console.log(
  JSON.stringify({
    oldSessionId,
    newSessionId,
    directEventCount: directEvents.length,
    envelopes,
  }),
)

clearConversationSpy?.mockRestore()
