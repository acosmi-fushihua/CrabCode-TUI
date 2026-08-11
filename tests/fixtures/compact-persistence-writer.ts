await import('../setup.js')

import { dirname } from 'node:path'
import { writeFile } from 'node:fs/promises'

import type { Command } from '../../src/types/command.js'
import type { Message } from '../../src/types/message.js'
import type { ToolUseContext } from '../../src/Tool.js'
import { asSessionId } from '../../src/types/ids.js'

const transcriptPath = process.env.COMPACT_TRANSCRIPT_PATH
const sessionId = process.env.COMPACT_SESSION_ID
const configDir = process.env.COMPACT_CONFIG_DIR

if (!transcriptPath || !sessionId || !configDir) {
  throw new Error(
    'COMPACT_TRANSCRIPT_PATH, COMPACT_SESSION_ID, and COMPACT_CONFIG_DIR are required',
  )
}

// tests/setup.ts installs native-package stubs before source imports. Restore
// this fixture's process-private config root before loading stateful modules.
process.env.CRABCODE_CONFIG_DIR = configDir
process.env.TEST_ENABLE_SESSION_PERSISTENCE = '1'
process.env.CRABCODE_EAGER_FLUSH = '1'

const [
  { QueryEngine },
  state,
  { getDefaultAppState },
  {
    createAssistantMessage,
    createCompactBoundaryMessage,
    createUserMessage,
  },
  sessionStorage,
] = await Promise.all([
  import('../../src/QueryEngine.js'),
  import('../../src/bootstrap/state.js'),
  import('../../src/state/AppStateStore.js'),
  import('../../src/utils/messages.js'),
  import('../../src/utils/sessionStorage.js'),
])

state.resetStateForTests()
state.setSessionPersistenceDisabled(false)
state.setIsInteractive(true)
state.switchSession(asSessionId(sessionId), dirname(transcriptPath))
state.setOriginalCwd(dirname(transcriptPath))
sessionStorage.resetProjectForTesting()
sessionStorage.clearSessionMessagesCache()
await writeFile(transcriptPath, '', { mode: 0o600 })
sessionStorage.setSessionFileForTesting(transcriptPath)

const at = (message: Message, timestamp: string): Message => ({
  ...message,
  timestamp,
})

const oldUser = at(
  createUserMessage({ content: 'ORIGINAL-OLD-USER: remove from resumed context' }),
  '2026-08-10T00:00:01.000Z',
)
const oldAssistant = at(
  createAssistantMessage({
    content: 'ORIGINAL-OLD-ASSISTANT: remove from resumed context',
  }),
  '2026-08-10T00:00:02.000Z',
)
const keptUser = at(
  createUserMessage({ content: 'PRESERVED-USER: retain after compaction' }),
  '2026-08-10T00:00:03.000Z',
)
const keptAssistant = at(
  createAssistantMessage({
    content: 'PRESERVED-ASSISTANT: retain after compaction',
  }),
  '2026-08-10T00:00:04.000Z',
)
const initialHistory = [
  oldUser,
  oldAssistant,
  keptUser,
  keptAssistant,
]

await sessionStorage.recordTranscript(initialHistory)
await sessionStorage.flushSessionStorage()

let appState = getDefaultAppState()
let boundaryUuid = ''
let summaryUuid = ''

const compactCommand = {
  type: 'local',
  name: 'compact',
  description: 'Hermetic persisted compaction fixture',
  argumentHint: '',
  supportsNonInteractive: true,
  load: async () => ({
    call: async (_args: string, _context: ToolUseContext) => {
      const summary = createUserMessage({
        content: 'COMPACT-SUMMARY: old work summarized without a model call',
        isCompactSummary: true,
        isVisibleInTranscriptOnly: true,
        timestamp: '2026-08-10T00:00:06.000Z',
      })
      const boundary = {
        ...createCompactBoundaryMessage(
          'manual',
          12_345,
          keptAssistant.uuid,
          undefined,
          2,
        ),
        timestamp: '2026-08-10T00:00:05.000Z',
      }
      boundary.compactMetadata = {
        ...boundary.compactMetadata,
        preservedSegment: {
          headUuid: keptUser.uuid,
          anchorUuid: summary.uuid,
          tailUuid: keptAssistant.uuid,
        },
      }
      boundaryUuid = boundary.uuid
      summaryUuid = summary.uuid

      return {
        type: 'compact' as const,
        compactionResult: {
          boundaryMarker: boundary,
          summaryMessages: [summary],
          messagesToKeep: [keptUser, keptAssistant],
          attachments: [],
          hookResults: [],
        },
        displayText: 'fixture compaction persisted',
      }
    },
  }),
} satisfies Command

const engine = new QueryEngine({
  cwd: dirname(transcriptPath),
  tools: [],
  commands: [compactCommand],
  mcpClients: [],
  agents: [],
  canUseTool: async () => ({
    behavior: 'deny',
    message: 'compact persistence fixture never executes model tools',
  }),
  getAppState: () => appState,
  setAppState: update => {
    appState = update(appState)
  },
  readFileCache: new Map(),
  initialMessages: initialHistory,
  abortController: new AbortController(),
  interactive: true,
  querySource: 'repl_main_thread',
})

const envelopes: Array<{ type?: string; subtype?: string; is_error?: boolean }> =
  []
for await (const envelope of engine.submitMessage('/compact')) {
  envelopes.push(envelope)
}

// This is the process-close durability boundary. The parent test reads the
// file only after this process exits, so no in-memory Project/cache can help.
await sessionStorage.flushSessionStorage()
sessionStorage.resetProjectForTesting()
sessionStorage.clearSessionMessagesCache()

await Bun.write(
  Bun.stdout,
  `${JSON.stringify({
    sessionId,
    boundaryUuid,
    summaryUuid,
    oldUserUuid: oldUser.uuid,
    oldAssistantUuid: oldAssistant.uuid,
    keptUserUuid: keptUser.uuid,
    keptAssistantUuid: keptAssistant.uuid,
    terminalResult: envelopes.findLast(envelope => envelope.type === 'result'),
  })}\n`,
)

// QueryEngine imports own long-lived process services. This fixture models a
// CLI process boundary, so close only after the transcript and evidence bytes
// are durable; the parent then performs resume in a different process.
process.exit(0)
