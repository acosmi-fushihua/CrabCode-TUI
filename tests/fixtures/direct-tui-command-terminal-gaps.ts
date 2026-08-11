await import('../setup.js')

import { spyOn } from 'bun:test'
import { dirname, join } from 'node:path'
import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs'

const workspace = process.env.TERMINAL_GAPS_WORKSPACE
if (!workspace) throw new Error('TERMINAL_GAPS_WORKSPACE is required')
const fixtureConfigDir = process.env.TERMINAL_GAPS_CONFIG_DIR
if (!fixtureConfigDir) {
  throw new Error('TERMINAL_GAPS_CONFIG_DIR is required')
}
// tests/setup.ts deliberately replaces CRABCODE_CONFIG_DIR with its own
// process-local auto-cleaned root. Re-pin this cross-process fixture only
// after setup and before importing config/session modules so recovery can
// inspect files after the writer process exits.
process.env.CRABCODE_CONFIG_DIR = fixtureConfigDir

const [
  { QueryEngine },
  state,
  shell,
  storage,
  { getDefaultAppState },
  catalog,
  { createCompactBoundaryMessage, createUserMessage },
] = await Promise.all([
  import('../../src/QueryEngine.js'),
  import('../../src/bootstrap/state.js'),
  import('../../src/utils/Shell.js'),
  import('../../src/utils/sessionStorage.js'),
  import('../../src/state/AppStateStore.js'),
  import('../../src/cli/headlessCommands.js'),
  import('../../src/utils/messages.js'),
])

type JsonRecord = Record<string, unknown>

function summarizeEnvelope(value: unknown): JsonRecord {
  const envelope = value as JsonRecord
  const message = envelope.message as JsonRecord | undefined
  return {
    type: envelope.type,
    subtype: envelope.subtype,
    session_id: envelope.session_id,
    uuid: envelope.uuid,
    content: envelope.content,
    message:
      message === undefined
        ? undefined
        : {
            content: message.content,
            role: message.role,
          },
    is_error: envelope.is_error,
    stop_reason: envelope.stop_reason,
    errors: envelope.errors,
    result: envelope.result,
  }
}

function summarizeStoredMessage(value: unknown): JsonRecord {
  const message = value as JsonRecord
  const apiMessage = message.message as JsonRecord | undefined
  return {
    type: message.type,
    subtype: message.subtype,
    uuid: message.uuid,
    content: message.content,
    messageContent: apiMessage?.content,
  }
}

function persistedMessage(
  message: JsonRecord,
  sessionId: string,
  parentUuid: string | null,
): JsonRecord {
  return {
    parentUuid,
    isSidechain: false,
    userType: 'external',
    cwd: workspace,
    sessionId,
    version: 'terminal-gap-fixture',
    ...message,
  }
}

async function resetCase(options: {
  persistence: boolean
  originalCwd?: string
}): Promise<void> {
  state.resetStateForTests()
  storage.resetProjectForTesting()
  state.setSessionPersistenceDisabled(!options.persistence)
  state.setIsInteractive(true)
  state.setSessionTrustAccepted(true)
  state.setProjectRoot(workspace)
  state.setOriginalCwd(options.originalCwd ?? workspace)
  shell.setCwd(workspace)
  catalog.clearHeadlessCommandMemoizationCaches()
}

async function runQuery(options: {
  invocation: string
  initialMessages: unknown[]
  abortBeforeSubmit?: boolean
  abortDuringClearHook?: boolean
  persistence: boolean
}): Promise<{
  directEvents: JsonRecord[]
  envelopes: JsonRecord[]
  hookAbortObserved: boolean
  messagesAfter: JsonRecord[]
}> {
  let appState = getDefaultAppState()
  const abortController = new AbortController()
  const directEvents: unknown[] = []
  const commands = await catalog.getDirectTuiCommands(workspace)
  const engine = new QueryEngine({
    cwd: workspace,
    tools: [],
    commands,
    mcpClients: [],
    agents: [],
    canUseTool: async () => ({
      behavior: 'deny',
      message: 'terminal gap fixture does not execute model tools',
    }),
    getAppState: () => appState,
    setAppState: update => {
      appState = update(appState)
    },
    readFileCache: new Map(),
    initialMessages: options.initialMessages as never[],
    abortController,
    interactive: true,
    querySource: 'repl_main_thread',
    onQueryEvent: event => directEvents.push(event),
  })

  if (options.abortBeforeSubmit) abortController.abort()

  const envelopes: unknown[] = []
  let hookAbortObserved = false
  let hookSpy: { mockRestore(): void } | undefined
  if (options.abortDuringClearHook) {
    const hooks = await import('../../src/utils/hooks.js')
    hookSpy = spyOn(hooks, 'executeSessionEndHooks').mockImplementation(
      (async (_reason, hookOptions) => {
        const signal = hookOptions?.signal
        if (!signal) throw new Error('clear omitted the SessionEnd hook signal')
        await new Promise<void>(resolve => {
          signal.addEventListener(
            'abort',
            () => {
              hookAbortObserved = true
              resolve()
            },
            { once: true },
          )
          queueMicrotask(() =>
            abortController.abort(
              new DOMException('fixture in-flight clear abort', 'AbortError'),
            ),
          )
        })
      }) as never,
    )
  }
  try {
    for await (const envelope of engine.submitMessage(options.invocation)) {
      envelopes.push(envelope)
    }
  } finally {
    hookSpy?.mockRestore()
  }
  if (options.persistence) await storage.flushSessionStorage()

  return {
    directEvents: directEvents.map(summarizeEnvelope),
    envelopes: envelopes.map(summarizeEnvelope),
    hookAbortObserved,
    messagesAfter: engine.getMessages().map(summarizeStoredMessage),
  }
}

async function compactHistorySuccess(): Promise<JsonRecord> {
  await resetCase({ persistence: true })
  const sessionId = state.getSessionId()
  const transcriptPath = storage.getTranscriptPath()
  mkdirSync(dirname(transcriptPath), { recursive: true })

  const before = createUserMessage({
    content: 'COMPACT-HISTORY-ORIGINAL-SENTINEL',
    timestamp: '2026-08-10T00:00:01.000Z',
  })
  const falsePositive = createUserMessage({
    content: 'This valid user entry is not a compact boundary.',
    timestamp: '2026-08-10T00:00:02.000Z',
  }) as unknown as JsonRecord
  falsePositive.subtype = 'compact_boundary'
  const boundary = {
    ...createCompactBoundaryMessage(
      'manual',
      12_345,
      before.uuid,
      undefined,
      7,
    ),
    timestamp: '2026-08-10T00:00:04.000Z',
  }
  const summary = createUserMessage({
    content: 'COMPACT-HISTORY-SUMMARY-SENTINEL',
    isCompactSummary: true,
    isVisibleInTranscriptOnly: true,
    timestamp: '2026-08-10T00:00:05.000Z',
  })
  const malformedSentinel =
    '{"type":"system","subtype":"compact_boundary","fixture":"MALFORMED-SENTINEL"'
  const initialRaw = [
    JSON.stringify(persistedMessage(before as unknown as JsonRecord, sessionId, null)),
    JSON.stringify(
      persistedMessage(
        falsePositive,
        sessionId,
        before.uuid,
      ),
    ),
    malformedSentinel,
    JSON.stringify(
      persistedMessage(
        boundary as unknown as JsonRecord,
        sessionId,
        null,
      ),
    ),
    JSON.stringify(
      persistedMessage(
        summary as unknown as JsonRecord,
        sessionId,
        boundary.uuid,
      ),
    ),
  ].join('\n') + '\n'
  writeFileSync(transcriptPath, initialRaw)

  const query = await runQuery({
    invocation: '/compact-history',
    initialMessages: [boundary, summary],
    persistence: true,
  })
  const finalRaw = readFileSync(transcriptPath, 'utf8')
  return {
    scenario: 'compact-history-success',
    sessionId,
    transcriptPath,
    initialRaw,
    finalRaw,
    ...query,
  }
}

async function compactHistoryFailure(): Promise<JsonRecord> {
  await resetCase({ persistence: false })
  const sessionId = state.getSessionId()
  const transcriptPath = storage.getTranscriptPath()
  mkdirSync(transcriptPath, { recursive: true })
  const seed = createUserMessage({ content: 'compact-history-failure-seed' })
  const query = await runQuery({
    invocation: '/compact-history',
    initialMessages: [seed],
    persistence: false,
  })
  return {
    scenario: 'compact-history-failure',
    oldSessionId: sessionId,
    newSessionId: state.getSessionId(),
    transcriptPath,
    seedUuid: seed.uuid,
    ...query,
  }
}

async function compactHistoryCancellation(): Promise<JsonRecord> {
  await resetCase({ persistence: false })
  const sessionId = state.getSessionId()
  const seed = createUserMessage({ content: 'compact-history-cancel-seed' })
  const query = await runQuery({
    invocation: '/compact-history',
    initialMessages: [seed],
    abortBeforeSubmit: true,
    persistence: false,
  })
  return {
    scenario: 'compact-history-cancellation',
    oldSessionId: sessionId,
    newSessionId: state.getSessionId(),
    seedUuid: seed.uuid,
    ...query,
  }
}

async function clearSuccess(invocation: string): Promise<JsonRecord> {
  await resetCase({ persistence: true })
  const oldSessionId = state.getSessionId()
  const oldTranscriptPath = storage.getTranscriptPath()
  mkdirSync(dirname(oldTranscriptPath), { recursive: true })
  const seed = createUserMessage({
    content: `CLEAR-ORIGINAL-SENTINEL:${invocation}`,
    timestamp: '2026-08-10T00:01:00.000Z',
  })
  const oldRaw =
    JSON.stringify(
      persistedMessage(seed as unknown as JsonRecord, oldSessionId, null),
    ) + '\n'
  writeFileSync(oldTranscriptPath, oldRaw)

  const query = await runQuery({
    invocation,
    initialMessages: [seed],
    persistence: true,
  })
  const newSessionId = state.getSessionId()
  const newTranscriptPath = storage.getTranscriptPath()
  return {
    scenario: 'clear-success',
    invocation,
    oldSessionId,
    newSessionId,
    oldTranscriptPath,
    newTranscriptPath,
    oldRaw,
    oldRawAfter: readFileSync(oldTranscriptPath, 'utf8'),
    newRaw: readFileSync(newTranscriptPath, 'utf8'),
    ...query,
  }
}

async function clearFailure(invocation: string, index: number): Promise<JsonRecord> {
  const missingRoot = join(workspace, `missing-clear-root-${index}`)
  await resetCase({ persistence: false, originalCwd: missingRoot })
  const oldSessionId = state.getSessionId()
  const seed = createUserMessage({ content: `clear-failure-seed:${invocation}` })
  const query = await runQuery({
    invocation,
    initialMessages: [seed],
    persistence: false,
  })
  return {
    scenario: 'clear-failure',
    invocation,
    missingRoot,
    oldSessionId,
    newSessionId: state.getSessionId(),
    seedUuid: seed.uuid,
    ...query,
  }
}

async function clearCancellation(invocation: string): Promise<JsonRecord> {
  await resetCase({ persistence: false })
  const oldSessionId = state.getSessionId()
  const seed = createUserMessage({ content: `clear-cancel-seed:${invocation}` })
  const query = await runQuery({
    invocation,
    initialMessages: [seed],
    abortBeforeSubmit: true,
    persistence: false,
  })
  return {
    scenario: 'clear-cancellation',
    invocation,
    oldSessionId,
    newSessionId: state.getSessionId(),
    seedUuid: seed.uuid,
    ...query,
  }
}

async function clearInFlightCancellation(): Promise<JsonRecord> {
  await resetCase({ persistence: false })
  const oldSessionId = state.getSessionId()
  const seed = createUserMessage({ content: 'clear-in-flight-cancel-seed' })
  const query = await runQuery({
    invocation: '/clear',
    initialMessages: [seed],
    abortDuringClearHook: true,
    persistence: false,
  })
  return {
    scenario: 'clear-in-flight-cancellation',
    invocation: '/clear',
    oldSessionId,
    newSessionId: state.getSessionId(),
    seedUuid: seed.uuid,
    ...query,
  }
}

async function runMatrix(): Promise<void> {
  mkdirSync(workspace, { recursive: true })
  const aliases = ['/clear', '/new', '/reset']
  const compact = {
    success: await compactHistorySuccess(),
    failure: await compactHistoryFailure(),
    cancellation: await compactHistoryCancellation(),
  }
  const clearInFlight = await clearInFlightCancellation()
  const clear = []
  for (const [index, invocation] of aliases.entries()) {
    clear.push({
      invocation,
      success: await clearSuccess(invocation),
      failure: await clearFailure(invocation, index),
      cancellation: await clearCancellation(invocation),
    })
  }
  console.log(JSON.stringify({ compact, clear, clearInFlight }))
}

async function runRecovery(): Promise<void> {
  const encoded = process.env.TERMINAL_GAPS_RECOVERY
  if (!encoded) throw new Error('TERMINAL_GAPS_RECOVERY is required')
  const recoveryItems = JSON.parse(encoded) as Array<{
    kind: 'compact-history' | 'clear'
    path: string
    sessionId: string
  }>
  const recovered: JsonRecord[] = []
  for (const item of recoveryItems) {
    const log = await storage.loadTranscriptFromFile(item.path)
    recovered.push({
      kind: item.kind,
      path: item.path,
      expectedSessionId: item.sessionId,
      sessionIds: [...new Set(log.messages.map(message => message.sessionId))],
      messageCount: log.messages.length,
      messageTypes: log.messages.map(message =>
        message.type === 'system'
          ? `${message.type}:${message.subtype}`
          : message.type,
      ),
      contents: log.messages.map(message =>
        message.type === 'user'
          ? message.message.content
          : message.type === 'system'
            ? message.content
            : undefined,
      ),
    })
  }

  const compactItem = recoveryItems.find(
    item => item.kind === 'compact-history',
  )
  let compactHistoryReplay: JsonRecord | undefined
  if (compactItem) {
    await resetCase({ persistence: false })
    state.switchSession(compactItem.sessionId as never, dirname(compactItem.path))
    const log = await storage.loadTranscriptFromFile(compactItem.path)
    compactHistoryReplay = await runQuery({
      invocation: '/compact-history',
      initialMessages: log.messages,
      persistence: false,
    })
  }

  console.log(JSON.stringify({ recovered, compactHistoryReplay }))
}

if (process.env.TERMINAL_GAPS_MODE === 'recover') {
  await runRecovery()
} else {
  await runMatrix()
}
