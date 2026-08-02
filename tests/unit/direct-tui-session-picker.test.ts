import { describe, expect, test } from 'bun:test'
import type { UUID } from 'node:crypto'
import type { ZodType } from 'zod/v4'

import {
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
  CrabCodeTuiSessionPickerEntrySchema,
  CrabCodeTuiSessionPickerInteractionResponseSchema,
  type CrabCodeTuiSetupRequest,
} from '../../src/cli/crabcodeTuiBridgeProtocol.js'
import {
  runDirectTuiStartupSessionPicker,
  type SessionPickerDependencies,
} from '../../src/cli/directTuiSessionPicker.js'
import type { TuiRuntimeOptions } from '../../src/cli/tuiRuntimeOptions.js'
import {
  NATIVE_TUI_MAX_FRAME_BYTES,
  type NativeTuiRendererSession,
} from '../../src/entrypoints/nativeTuiRendererSession.js'
import type { LogOption, SerializedMessage } from '../../src/types/logs.js'

const SESSION_ONE = '30000000-0000-4000-8000-000000000001'
const SESSION_TWO = '30000000-0000-4000-8000-000000000002'

function log(
  sessionId: string,
  overrides: Partial<LogOption> = {},
): LogOption {
  return {
    date: '2026-07-29',
    messages: [],
    fullPath: `/workspace/${sessionId}.jsonl`,
    value: 0,
    created: new Date('2026-07-29T00:00:00.000Z'),
    modified: new Date('2026-07-29T01:00:00.000Z'),
    firstPrompt: `prompt ${sessionId}`,
    messageCount: 1,
    isSidechain: false,
    sessionId,
    gitBranch: 'main',
    ...overrides,
  }
}

function message(sessionId: string): SerializedMessage {
  return {
    type: 'user',
    uuid: '40000000-0000-4000-8000-000000000001' as UUID,
    parentUuid: null,
    isSidechain: false,
    userType: 'external',
    cwd: '/workspace',
    sessionId,
    version: '1.0.0',
    gitBranch: 'main',
    timestamp: '2026-07-29T00:00:00.000Z',
    message: {
      role: 'user',
      content: 'preview body',
    },
  } as SerializedMessage
}

function dependencies(
  first: LogOption,
  overrides: Partial<SessionPickerDependencies> = {},
): SessionPickerDependencies {
  return {
    getOriginalCwd: () => '/workspace',
    getCurrentSessionId: () => 'current-session',
    getWorktreePaths: async () => ['/workspace'],
    getBranch: async () => 'main',
    searchSessionsByCustomTitle: async () => [],
    loadSameRepoMessageLogsProgressive: async () => ({
      logs: [first],
      allStatLogs: [first],
      nextIndex: 1,
    }),
    loadAllProjectsMessageLogsProgressive: async () => ({
      logs: [first],
      allStatLogs: [first],
      nextIndex: 1,
    }),
    enrichLogs: async () => ({ logs: [], nextIndex: 1 }),
    loadFullLog: async selected => selected,
    saveCustomTitle: async () => {},
    isCustomTitleEnabled: () => true,
    ...overrides,
  }
}

function interaction(
  decision: Record<string, unknown>,
): Record<string, unknown> {
  return {
    protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
    kind: 'session_picker',
    phase: 'interaction',
    ...decision,
  }
}

function requester(
  requests: CrabCodeTuiSetupRequest[],
  catalogResponses: Record<string, unknown>[],
  previewResponses: Record<string, unknown>[] = [],
): NativeTuiRendererSession['requestSetup'] {
  return async function requestSetup<Response>(
    request: CrabCodeTuiSetupRequest,
    responseSchema: ZodType<Response>,
  ): Promise<Response> {
    requests.push(request)
    if (request.kind !== 'session_picker') {
      throw new Error(`unexpected setup kind ${request.kind}`)
    }
    const response =
      request.phase === 'catalog_show'
        ? catalogResponses.shift()
        : request.phase === 'preview_complete' ||
            request.phase === 'preview_failed'
          ? previewResponses.shift()
          : {
              protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
              kind: 'session_picker',
              phase: request.phase,
              decision: 'received',
            }
    if (!response) {
      throw new Error(`missing response for session picker ${request.phase}`)
    }
    return responseSchema.parse(response)
  }
}

function decodeCatalogChunks(
  requests: CrabCodeTuiSetupRequest[],
): unknown {
  const encoded = requests
    .filter(
      request =>
        request.kind === 'session_picker' &&
        request.phase === 'catalog_chunk',
    )
    .map(request => {
      if (
        request.kind !== 'session_picker' ||
        request.phase !== 'catalog_chunk'
      ) {
        throw new Error('catalog request narrowed incorrectly')
      }
      return request.data_base64
    })
    .join('')
  return JSON.parse(Buffer.from(encoded, 'base64').toString('utf8'))
}

describe('direct startup session picker', () => {
  test('private picker DTO rejects redundant project and selection state', () => {
    expect(
      CrabCodeTuiSessionPickerEntrySchema.safeParse({
        id: 'session-1',
        title: 'Session',
        search_text: 'Session main',
        metadata: 'now · main',
        tag: null,
        branch: 'main',
        group_id: SESSION_ONE,
        in_current_project: true,
        in_current_worktree: true,
      }).success,
    ).toBe(false)
    expect(
      CrabCodeTuiSessionPickerInteractionResponseSchema.safeParse({
        ...interaction({
          decision: 'select',
          id: 'session-1',
        }),
        all_projects: false,
      }).success,
    ).toBe(false)
  })

  test('bare --resume selects through existing direct session storage and rewrites only the existing resume option', async () => {
    const selected = log(SESSION_ONE, {
      firstPrompt: '  prompt\n\twith   historical whitespace  ',
    })
    const options: TuiRuntimeOptions = { resume: true }
    const requests: CrabCodeTuiSetupRequest[] = []

    await runDirectTuiStartupSessionPicker(
      options,
      requester(requests, [
        interaction({
          decision: 'select',
          id: 'session-1',
        }),
      ]),
      dependencies(selected),
    )

    expect(options.resume).toBe(selected.fullPath)
    expect(
      requests.map(request =>
        request.kind === 'session_picker' ? request.phase : request.kind,
      ),
    ).toEqual([
      'loading',
      'catalog_start',
      'catalog_chunk',
      'catalog_show',
      'resolved',
    ])
    expect(decodeCatalogChunks(requests)).toEqual([
      expect.objectContaining({
        id: 'session-1',
        title: 'prompt with historical whitespace',
        branch: 'main',
        group_id: SESSION_ONE,
        in_current_worktree: true,
      }),
    ])
  })

  test('one exact custom-title match preserves the historical picker and cross-project-policy bypass', async () => {
    const selected = log(SESSION_ONE, {
      customTitle: 'Named session',
      projectPath: '/different/project',
    })
    const options: TuiRuntimeOptions = { resume: '  Named session  ' }
    const requests: CrabCodeTuiSetupRequest[] = []
    const searches: Array<{
      query: string
      options: { exact: true }
    }> = []

    await runDirectTuiStartupSessionPicker(
      options,
      requester(requests, []),
      dependencies(selected, {
        searchSessionsByCustomTitle: async (query, searchOptions) => {
          searches.push({ query, options: searchOptions })
          return [selected]
        },
      }),
    )

    expect(searches).toEqual([
      { query: 'Named session', options: { exact: true } },
    ])
    expect(options.resume).toBe(selected.fullPath)
    expect(
      requests.map(request =>
        request.kind === 'session_picker' ? request.phase : request.kind,
      ),
    ).toEqual(['loading', 'resolved'])
  })

  test('preview, back, all-project reload, and progressive append remain renderer actions over direct authorities', async () => {
    const first = log(SESSION_ONE)
    const skipped = log(
      '30000000-0000-4000-8000-000000000003',
      {
        isSidechain: true,
        value: 1,
      },
    )
    const second = log(SESSION_TWO, {
      value: 2,
      fullPath: undefined,
      projectPath: undefined,
    })
    const options: TuiRuntimeOptions = { resume: true }
    const requests: CrabCodeTuiSetupRequest[] = []
    const full = {
      ...first,
      messages: [message(SESSION_ONE)],
      messageCount: 1,
    }
    let allProjectLoads = 0
    const enrichCalls: Array<{ startIndex: number; count: number }> = []

    await runDirectTuiStartupSessionPicker(
      options,
      requester(
        requests,
        [
          interaction({ decision: 'preview', id: 'session-1' }),
          interaction({ decision: 'reload', all_projects: true }),
          interaction({ decision: 'load_more', count: 7 }),
          interaction({
            decision: 'select',
            id: 'session-2',
          }),
        ],
        [interaction({ decision: 'back' })],
      ),
      dependencies(first, {
        loadFullLog: async () => full,
        loadAllProjectsMessageLogsProgressive: async () => {
          allProjectLoads += 1
          return {
            logs: [first],
            allStatLogs: [first, skipped, second],
            nextIndex: 1,
          }
        },
        enrichLogs: async (_logs, startIndex, count) => {
          enrichCalls.push({ startIndex, count })
          return startIndex === 1
            ? { logs: [], nextIndex: 2 }
            : { logs: [second], nextIndex: 3 }
        },
      }),
    )

    expect(allProjectLoads).toBe(1)
    expect(enrichCalls).toEqual([
      { startIndex: 1, count: 7 },
      { startIndex: 2, count: 7 },
    ])
    expect(options.resume).toBe(SESSION_TWO)
    expect(
      requests
        .filter(
          request =>
            request.kind === 'session_picker' &&
            request.phase === 'catalog_start',
        )
        .map(request => {
          if (
            request.kind !== 'session_picker' ||
            request.phase !== 'catalog_start'
          ) {
            throw new Error('catalog request narrowed incorrectly')
          }
          return [request.update, request.all_projects]
        }),
    ).toEqual([
      ['replace', false],
      ['replace', true],
      ['append', true],
    ])

    const previewBytes = requests
      .filter(
        request =>
          request.kind === 'session_picker' &&
          request.phase === 'preview_message_chunk',
      )
      .map(request => {
        if (
          request.kind !== 'session_picker' ||
          request.phase !== 'preview_message_chunk'
        ) {
          throw new Error('preview request narrowed incorrectly')
        }
        return request.data_base64
      })
      .join('')
    expect(
      JSON.parse(Buffer.from(previewBytes, 'base64').toString('utf8')),
    ).toEqual(message(SESSION_ONE))
  })

  test('preview authority failures return to the existing catalog without inventing backend state', async () => {
    const selected = log(SESSION_ONE)
    const options: TuiRuntimeOptions = { resume: true }
    const requests: CrabCodeTuiSetupRequest[] = []

    await runDirectTuiStartupSessionPicker(
      options,
      requester(
        requests,
        [
          interaction({ decision: 'preview', id: 'session-1' }),
          interaction({ decision: 'select', id: 'session-1' }),
        ],
        [interaction({ decision: 'back' })],
      ),
      dependencies(selected, {
        loadFullLog: async () => {
          throw new Error('preview authority\nfailed')
        },
      }),
    )

    expect(options.resume).toBe(selected.fullPath)
    expect(
      requests.map(request =>
        request.kind === 'session_picker' ? request.phase : request.kind,
      ),
    ).toContain('preview_failed')
    const failed = requests.find(
      request =>
        request.kind === 'session_picker' &&
        request.phase === 'preview_failed',
    )
    expect(failed).toEqual(
      expect.objectContaining({
        id: 'session-1',
        error: 'preview authority failed',
      }),
    )
  })

  test('different-project selection emits the historical command notice and waits for renderer-owned exit', async () => {
    const selected = log(SESSION_ONE, {
      projectPath: '/different/project',
    })
    const options: TuiRuntimeOptions = { resume: true }
    const requests: CrabCodeTuiSetupRequest[] = []
    const catalogResponses = [
      interaction({ decision: 'reload', all_projects: true }),
      interaction({ decision: 'select', id: 'session-1' }),
    ]
    let observeCrossProject!: () => void
    const crossProjectSeen = new Promise<void>(resolve => {
      observeCrossProject = resolve
    })

    void runDirectTuiStartupSessionPicker(
      options,
      async function requestSetup<Response>(
        request: CrabCodeTuiSetupRequest,
        responseSchema: ZodType<Response>,
      ): Promise<Response> {
        requests.push(request)
        if (
          request.kind === 'session_picker' &&
          request.phase === 'cross_project'
        ) {
          observeCrossProject()
          return new Promise<Response>(() => {})
        }
        const response =
          request.kind === 'session_picker' &&
          request.phase === 'catalog_show'
            ? catalogResponses.shift()
            : request.kind === 'session_picker'
              ? {
                  protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
                  kind: 'session_picker',
                  phase: request.phase,
                  decision: 'received',
                }
              : undefined
        if (!response) throw new Error('missing session picker fixture response')
        return responseSchema.parse(response)
      },
      dependencies(selected),
    )
    await crossProjectSeen

    const crossProject = requests.at(-1)
    expect(crossProject).toEqual(
      expect.objectContaining({
        kind: 'session_picker',
        phase: 'cross_project',
        command: `cd /different/project && crabcode --resume ${SESSION_ONE}`,
      }),
    )
    expect(options.resume).toBe(true)
  })

  test('historical visibility filtering is feature-gated while sidechains are always excluded', async () => {
    const emptyVisible = log(SESSION_ONE, {
      messages: [],
      firstPrompt: '',
      customTitle: undefined,
    })
    const hiddenSidechain = log(SESSION_TWO, {
      isSidechain: true,
      customTitle: 'must not leak into the main-session picker',
    })
    const requests: CrabCodeTuiSetupRequest[] = []
    const options: TuiRuntimeOptions = { resume: true }

    await runDirectTuiStartupSessionPicker(
      options,
      requester(requests, [
        interaction({
          decision: 'select',
          id: 'session-1',
        }),
      ]),
      dependencies(emptyVisible, {
        isCustomTitleEnabled: () => false,
        loadSameRepoMessageLogsProgressive: async () => ({
          logs: [hiddenSidechain, emptyVisible],
          allStatLogs: [hiddenSidechain, emptyVisible],
          nextIndex: 2,
        }),
      }),
    )

    expect(decodeCatalogChunks(requests)).toEqual([
      expect.objectContaining({
        id: 'session-1',
        group_id: SESSION_ONE,
      }),
    ])
    expect(options.resume).toBe(emptyVisible.fullPath)
  })

  test('canonical UUID resume stays owned by the backend path', async () => {
    const options: TuiRuntimeOptions = { resume: SESSION_ONE }
    const requests: CrabCodeTuiSetupRequest[] = []
    await runDirectTuiStartupSessionPicker(
      options,
      requester(requests, []),
      dependencies(log(SESSION_ONE)),
    )
    expect(requests).toEqual([])
    expect(options.resume).toBe(SESSION_ONE)
  })

  test('chunking cannot turn one logical catalog into an unbounded private transfer', async () => {
    const selected = log(SESSION_ONE, {
      firstPrompt: 'x'.repeat(NATIVE_TUI_MAX_FRAME_BYTES),
    })
    const options: TuiRuntimeOptions = { resume: true }
    const requests: CrabCodeTuiSetupRequest[] = []

    await expect(
      runDirectTuiStartupSessionPicker(
        options,
        requester(requests, []),
        dependencies(selected),
      ),
    ).rejects.toThrow(
      `transport limit is ${NATIVE_TUI_MAX_FRAME_BYTES}`,
    )
    expect(
      requests.some(
        request =>
          request.kind === 'session_picker' &&
          request.phase === 'catalog_chunk',
      ),
    ).toBe(false)
  })
})
