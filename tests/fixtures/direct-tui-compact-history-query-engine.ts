import { spyOn } from 'bun:test'
import * as fsPromises from 'fs/promises'

await import('../setup.js')

const [{ QueryEngine }, state, { getDefaultAppState }, catalog] =
  await Promise.all([
    import('../../src/QueryEngine.js'),
    import('../../src/bootstrap/state.js'),
    import('../../src/state/AppStateStore.js'),
    import('../../src/cli/headlessCommands.js'),
  ])

state.resetStateForTests()
state.setSessionPersistenceDisabled(true)
state.setIsInteractive(true)

const abortController = new AbortController()
const mode = process.env.DIRECT_TUI_COMPACT_HISTORY_MODE ?? 'success'

let appState = getDefaultAppState()
const commands = await catalog.getDirectTuiCommands(process.cwd())
const engine = new QueryEngine({
  cwd: process.cwd(),
  tools: [],
  commands,
  mcpClients: [],
  agents: [],
  canUseTool: async () => ({
    behavior: 'deny',
    message: 'compact-history fixture does not execute model tools',
  }),
  getAppState: () => appState,
  setAppState: update => {
    appState = update(appState)
  },
  readFileCache: new Map(),
  interactive: true,
  querySource: 'repl_main_thread',
  abortController,
})

// Install the seam only after runtime/catalog initialization so the call
// count and outcome describe /compact-history's physical read, not unrelated
// startup configuration reads.
const { getTranscriptPath } = await import('../../src/utils/sessionStorage.js')
const transcriptPath = getTranscriptPath()
const originalReadFile = fsPromises.readFile
const readFileSpy = spyOn(fsPromises, 'readFile')
readFileSpy.mockImplementation(
  ((path: Parameters<typeof originalReadFile>[0], ...args: unknown[]) => {
    if (String(path) !== transcriptPath) {
      return Reflect.apply(originalReadFile, fsPromises, [path, ...args])
    }
    if (mode === 'failure') {
      return Promise.reject(
        Object.assign(new Error('fixture transcript access denied'), {
          code: 'EACCES',
        }),
      )
    }
    if (mode === 'read-abort') {
      const options = args[0] as { signal?: AbortSignal }
      return new Promise<string>((_resolve, reject) => {
        const signal = options.signal
        if (!signal) throw new Error('compact-history omitted read signal')
        signal.addEventListener('abort', () => reject(signal.reason), {
          once: true,
        })
        queueMicrotask(() =>
          abortController.abort(
            new DOMException('fixture read abort', 'AbortError'),
          ),
        )
      })
    }
    if (mode === 'post-read-abort') {
      abortController.abort(
        new DOMException('fixture post-read abort', 'AbortError'),
      )
      return Promise.resolve('')
    }
    return Promise.reject(
      Object.assign(new Error('fixture transcript is absent'), {
        code: 'ENOENT',
      }),
    )
  }) as never,
)

if (mode === 'pre-abort') {
  abortController.abort(
    new DOMException('fixture pre-read abort', 'AbortError'),
  )
}

const envelopes: unknown[] = []
for await (const envelope of engine.submitMessage('/compact-history')) {
  envelopes.push(envelope)
}

console.log(
  JSON.stringify({
    mode,
    sessionId: state.getSessionId(),
    readCount: readFileSpy.mock.calls.filter(
      ([path]) => String(path) === transcriptPath,
    ).length,
    envelopes,
  }),
)

readFileSpy.mockRestore()
