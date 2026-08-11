await import('../setup.js')

import { mkdir, writeFile } from 'node:fs/promises'
import { dirname } from 'node:path'

import type { CompactProgressEvent } from '../../src/Tool.js'

const scenario = process.env.COMPACT_SM_PROGRESS_SCENARIO
const configDir = process.env.COMPACT_SM_CONFIG_DIR
if (
  !configDir ||
  (scenario !== 'success' && scenario !== 'fallback' && scenario !== 'error')
) {
  throw new Error(
    'COMPACT_SM_CONFIG_DIR and a success/fallback/error scenario are required',
  )
}

process.env.CRABCODE_CONFIG_DIR = configDir
process.env.ENABLE_CRABCODE_SM_COMPACT = '1'
process.env.CRABCODE_DISABLE_AUTO_MEMORY = '1'

const [
  state,
  { createCompactProgressLifecycle },
  { trySessionMemoryCompaction },
  { createUserMessage },
  { getSessionMemoryPath },
  { setLastSummarizedMessageId },
] = await Promise.all([
  import('../../src/bootstrap/state.js'),
  import('../../src/commands/compact/progressLifecycle.js'),
  import('../../src/services/compact/sessionMemoryCompact.js'),
  import('../../src/utils/messages.js'),
  import('../../src/utils/permissions/filesystem.js'),
  import('../../src/services/SessionMemory/sessionMemoryUtils.js'),
])

state.resetStateForTests()
state.setSessionPersistenceDisabled(true)

const seed = createUserMessage({ content: `session-memory-${scenario}` })
let messages = [seed]
const memoryPath = getSessionMemoryPath()

if (scenario !== 'fallback') {
  await mkdir(dirname(memoryPath), { recursive: true })
  await writeFile(
    memoryPath,
    '# Session memory\n\n- A real extracted fact for compaction tests.\n',
  )
}

if (scenario === 'error') {
  setLastSummarizedMessageId(seed.uuid)
  messages = new Proxy(messages, {
    get(target, property, receiver) {
      if (property === 'findIndex') {
        return () => {
          throw new Error('fixture session-memory selection failure')
        }
      }
      return Reflect.get(target, property, receiver) as unknown
    },
  })
} else {
  setLastSummarizedMessageId(undefined)
}

const events: CompactProgressEvent[] = []
const statuses: Array<string | null> = []
const lifecycle = createCompactProgressLifecycle(event => events.push(event))
let outcome: 'success' | 'fallback' | 'error' = scenario

try {
  const result = await trySessionMemoryCompaction(
    messages,
    undefined,
    undefined,
    {
      onCompactProgress: lifecycle.emit,
      setSDKStatus: status => statuses.push(status),
    },
  )
  outcome = result ? 'success' : scenario === 'error' ? 'error' : 'fallback'

  if (!result) {
    // Model the legacy path without an API call. It reports into the same
    // lifecycle that the real /compact command passes to compactConversation.
    lifecycle.emit({ type: 'hooks_start', hookType: 'pre_compact' })
    lifecycle.emit({ type: 'compact_start' })
    if (scenario === 'fallback') {
      lifecycle.emit({ type: 'hooks_start', hookType: 'session_start' })
      lifecycle.emit({ type: 'hooks_start', hookType: 'post_compact' })
    }
    lifecycle.emit({ type: 'compact_end' })
  }
} finally {
  lifecycle.finish()
  statuses.push(null)
}

console.log(
  JSON.stringify({
    outcome,
    phases: events.map(event =>
      event.type === 'hooks_start'
        ? `${event.type}:${event.hookType}`
        : event.type,
    ),
    statuses,
  }),
)
