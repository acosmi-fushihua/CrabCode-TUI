await import('../setup.js')

import type { Command } from '../../src/types/command.js'
import type { ToolUseContext } from '../../src/Tool.js'

const [
  { QueryEngine },
  state,
  { getDefaultAppState },
  { createCompactBoundaryMessage, createUserMessage },
] = await Promise.all([
  import('../../src/QueryEngine.js'),
  import('../../src/bootstrap/state.js'),
  import('../../src/state/AppStateStore.js'),
  import('../../src/utils/messages.js'),
])

state.resetStateForTests()
state.setSessionPersistenceDisabled(true)
state.setIsInteractive(true)

type Scenario =
  | 'success'
  | 'alias-success'
  | 'empty-history'
  | 'api-error'
  | 'cancelled'
const scenarios: Scenario[] = [
  'success',
  'alias-success',
  'empty-history',
  'api-error',
  'cancelled',
]

const evidence: Record<
  Scenario,
  { directEvents: unknown[]; envelopes: unknown[] }
> = {} as Record<
  Scenario,
  { directEvents: unknown[]; envelopes: unknown[] }
>

for (const scenario of scenarios) {
  let appState = getDefaultAppState()
  const abortController = new AbortController()
  const seed = createUserMessage({ content: `seed-${scenario}` })
  const directEvents: unknown[] = []

  const compactCommand = {
    type: 'local',
    name: 'compact',
    aliases: ['com'],
    description: 'Hermetic compact result-contract fixture',
    argumentHint: '',
    supportsNonInteractive: true,
    load: async () => ({
      call: async (_args: string, context: ToolUseContext) => {
        switch (scenario) {
          case 'empty-history':
            if (context.messages.length !== 0) {
              throw new Error('fixture expected an empty history')
            }
            throw new Error('没有可压缩的消息')
          default:
            context.onCompactProgress?.({
              type: 'hooks_start',
              hookType: 'pre_compact',
            })
            context.onCompactProgress?.({ type: 'compact_start' })
            try {
              switch (scenario) {
                case 'api-error':
                  throw new Error(
                    'Error during compaction: fixture API unavailable',
                  )
                case 'cancelled':
                  context.abortController.abort('fixture compact cancellation')
                  throw new Error('压缩已取消')
                case 'success':
                case 'alias-success':
                  return {
                    type: 'compact' as const,
                    compactionResult: {
                      boundaryMarker: createCompactBoundaryMessage(
                        'manual',
                        1_024,
                        seed.uuid,
                        undefined,
                        1,
                      ),
                      summaryMessages: [
                        createUserMessage({
                          content: 'fixture compact summary',
                          isCompactSummary: true,
                          isVisibleInTranscriptOnly: true,
                        }),
                      ],
                      attachments: [],
                      hookResults: [],
                    },
                    displayText: '压缩完成；原文已保存。',
                  }
              }
            } finally {
              context.onCompactProgress?.({ type: 'compact_end' })
            }
        }
      },
    }),
  } satisfies Command

  const engine = new QueryEngine({
    cwd: process.cwd(),
    tools: [],
    commands: [compactCommand],
    mcpClients: [],
    agents: [],
    canUseTool: async () => ({
      behavior: 'deny',
      message: 'compact fixture does not execute model tools',
    }),
    getAppState: () => appState,
    setAppState: update => {
      appState = update(appState)
    },
    readFileCache: new Map(),
    initialMessages: scenario === 'empty-history' ? [] : [seed],
    abortController,
    interactive: true,
    querySource: 'repl_main_thread',
    onQueryEvent: event => directEvents.push(event),
  })

  const envelopes: unknown[] = []
  const invocation =
    scenario === 'alias-success' ? '/com preserve goals' : '/compact'
  for await (const envelope of engine.submitMessage(invocation)) {
    envelopes.push(envelope)
  }
  evidence[scenario] = { directEvents, envelopes }
}

console.log(JSON.stringify(evidence))
