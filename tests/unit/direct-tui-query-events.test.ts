import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import {
  createDirectTuiQueryEventSink,
  DIRECT_TUI_RENDERER_EVENT_TYPES,
  isDirectTuiControlPlaneSdkMessage,
  isDirectTuiRendererEvent,
  publishDirectTuiInputEvents,
  type DirectTuiRendererEvent,
} from '../../src/cli/directTuiQueryEvents.js'
import {
  shouldRegisterSdkHookEventHandler,
  shouldTransformStreamOutput,
} from '../../src/cli/print/streamOutputPolicy.js'
import {
  clearHookEventState,
  emitHookProgress,
  emitHookResponse,
  emitHookStarted,
  registerHookEventHandler,
  suppressHookEventDelivery,
} from '../../src/utils/hooks/hookEvents.js'
import type { SDKMessage } from '../../src/entrypoints/agentSdkTypes.js'
import type { Message } from '../../src/types/message.js'

const ROOT = resolve(import.meta.dir, '../..')

function queryEvent(type: Message['type'], ordinal: number): Message {
  return { type, ordinal } as unknown as Message
}

describe('direct-TUI query event delivery', () => {
  test('passes the current nine renderer events in source order', () => {
    const active = DIRECT_TUI_RENDERER_EVENT_TYPES.map((type, index) =>
      queryEvent(type, index),
    )
    const delivered: DirectTuiRendererEvent[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))

    for (const event of active) sink(event)

    expect(DIRECT_TUI_RENDERER_EVENT_TYPES).toEqual([
      'assistant',
      'user',
      'progress',
      'attachment',
      'system',
      'stream_event',
      'stream_request_start',
      'tombstone',
      'tool_use_summary',
    ])
    expect(delivered).toEqual(active)
    for (let index = 0; index < active.length; index++) {
      expect(delivered[index]).toBe(active[index])
    }
  })

  test('marks only objects observed at the query boundary', () => {
    const observed = queryEvent('assistant', 1)
    const sameShape = { ...observed }
    const sink = createDirectTuiQueryEventSink(() => {})

    expect(isDirectTuiRendererEvent(observed)).toBe(false)
    sink(observed)
    expect(isDirectTuiRendererEvent(observed)).toBe(true)
    expect(isDirectTuiRendererEvent(sameShape)).toBe(false)
  })

  test('an inherited streamlined opt-in cannot rewrite the direct route', () => {
    const previous = process.env.CRABCODE_STREAMLINED_OUTPUT
    process.env.CRABCODE_STREAMLINED_OUTPUT = 'true'
    try {
      expect(
        shouldTransformStreamOutput({
          directQueryEventDelivery: true,
          streamlinedFeatureEnabled: true,
          streamlinedEnvironmentValue:
            process.env.CRABCODE_STREAMLINED_OUTPUT,
          outputFormat: 'stream-json',
        }),
      ).toBe(false)
      expect(
        shouldTransformStreamOutput({
          directQueryEventDelivery: false,
          streamlinedFeatureEnabled: true,
          streamlinedEnvironmentValue:
            process.env.CRABCODE_STREAMLINED_OUTPUT,
          outputFormat: 'stream-json',
        }),
      ).toBe(true)
    } finally {
      if (previous === undefined) {
        delete process.env.CRABCODE_STREAMLINED_OUTPUT
      } else {
        process.env.CRABCODE_STREAMLINED_OUTPUT = previous
      }
    }
  })

  test('SDK hook lifecycle registration is impossible on the direct route', () => {
    expect(
      shouldRegisterSdkHookEventHandler({
        directQueryEventDelivery: true,
        outputFormat: 'stream-json',
        verbose: true,
      }),
    ).toBe(false)
    expect(
      shouldRegisterSdkHookEventHandler({
        directQueryEventDelivery: false,
        outputFormat: 'stream-json',
        verbose: true,
      }),
    ).toBe(true)
    expect(
      shouldRegisterSdkHookEventHandler({
        directQueryEventDelivery: false,
        outputFormat: 'stream-json',
        verbose: false,
      }),
    ).toBe(false)
    expect(
      shouldRegisterSdkHookEventHandler({
        directQueryEventDelivery: false,
        outputFormat: 'json',
        verbose: true,
      }),
    ).toBe(false)
  })

  test('discard state isolates Hook events from both an earlier and a later query', () => {
    const earlier: string[] = []
    const later: string[] = []
    clearHookEventState()
    try {
      registerHookEventHandler(event => earlier.push(`${event.type}:old`))
      emitHookStarted('old', 'old-hook', 'SessionStart')
      expect(earlier).toEqual(['started:old'])

      suppressHookEventDelivery()
      emitHookStarted('direct', 'direct-hook', 'SessionStart')
      emitHookProgress({
        hookId: 'direct',
        hookName: 'direct-hook',
        hookEvent: 'SessionStart',
        stdout: 'private-direct-context',
        stderr: '',
        output: 'private-direct-context',
      })
      emitHookResponse({
        hookId: 'direct',
        hookName: 'direct-hook',
        hookEvent: 'SessionStart',
        output: 'private-direct-context',
        stdout: 'private-direct-context',
        stderr: '',
        exitCode: 0,
        outcome: 'success',
      })
      expect(earlier).toEqual(['started:old'])

      registerHookEventHandler(event => later.push(`${event.type}:new`))
      expect(later).toEqual([])
      emitHookStarted('new', 'new-hook', 'SessionStart')
      expect(later).toEqual(['started:new'])
    } finally {
      clearHookEventState()
    }
  })

  test('delivers tool_use_summary without rewriting it', () => {
    const delivered: DirectTuiRendererEvent[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))
    const summary = queryEvent('tool_use_summary', 8)

    sink(summary)

    expect(delivered).toEqual([summary])
    expect(delivered[0]).toBe(summary)
  })

  test('delivers an additive future presentation event for compatibility projection', () => {
    const delivered: DirectTuiRendererEvent[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))
    const future = {
      type: 'future_backend_event',
      payload: { presentation: 'preserve-me' },
    } as unknown as Message

    expect(() => sink(future)).not.toThrow()
    expect(delivered).toEqual([future])
    expect(delivered[0]).toBe(future)
  })

  test('fails closed only for malformed transport records', () => {
    const sink = createDirectTuiQueryEventSink(() => {})

    expect(() => sink({} as unknown as Message)).toThrow(
      'Malformed direct query event: missing string type',
    )
  })

  test('keeps only established init/result controls from later SDK projection', () => {
    const messages = [
      { type: 'system', subtype: 'init' },
      { type: 'result', subtype: 'success' },
      { type: 'assistant' },
      { type: 'user' },
      { type: 'stream_event' },
      { type: 'tool_use_summary' },
      { type: 'system', subtype: 'compact_boundary' },
    ] as unknown as SDKMessage[]

    expect(messages.map(isDirectTuiControlPlaneSdkMessage)).toEqual([
      true,
      true,
      false,
      false,
      false,
      false,
      false,
    ])
  })

  test('passes local-command output and compact boundary as original internal objects', () => {
    const localCommand = {
      type: 'system',
      subtype: 'local_command',
      content: '<local-command-stdout>ok</local-command-stdout>',
      uuid: 'local-command',
    } as unknown as Message
    const compactBoundary = {
      type: 'system',
      subtype: 'compact_boundary',
      content: 'summary',
      compactMetadata: {
        trigger: 'manual',
        preTokens: 100,
      },
      uuid: 'compact-boundary',
    } as unknown as Message
    const delivered: DirectTuiRendererEvent[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))

    sink(localCommand)
    sink(compactBoundary)

    expect(delivered).toEqual([localCommand, compactBoundary])
    expect(delivered[0]).toBe(localCommand)
    expect(delivered[1]).toBe(compactBoundary)
  })

  test('publishes the accepted user object before later assistant output without rewriting it', () => {
    const user = {
      type: 'user',
      message: {
        role: 'user',
        content: [
          { type: 'text', text: '精确保留用户输入' },
          {
            type: 'image',
            source: {
              type: 'base64',
              media_type: 'image/png',
              data: 'AA==',
            },
          },
        ],
      },
      uuid: 'accepted-user-uuid',
      timestamp: '2026-08-01T12:34:56.789Z',
    } as unknown as Message
    const assistant = {
      type: 'assistant',
      uuid: 'assistant-uuid',
      timestamp: '2026-08-01T12:34:57.000Z',
      message: { role: 'assistant', content: [] },
    } as unknown as Message
    const delivered: DirectTuiRendererEvent[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))

    publishDirectTuiInputEvents([user], sink)
    sink(assistant)

    expect(delivered).toEqual([user, assistant])
    expect(delivered[0]).toBe(user)
    expect(delivered[0]).toMatchObject({
      uuid: 'accepted-user-uuid',
      timestamp: '2026-08-01T12:34:56.789Z',
      message: {
        role: 'user',
        content: [
          { type: 'text', text: '精确保留用户输入' },
          {
            type: 'image',
            source: {
              type: 'base64',
              media_type: 'image/png',
              data: 'AA==',
            },
          },
        ],
      },
    })
  })

  test('publishes one local-command batch exactly once and is inert without an observer', () => {
    const commandInput = {
      type: 'user',
      uuid: 'local-input',
      timestamp: '2026-08-01T12:35:00.000Z',
      message: { role: 'user', content: '/status' },
    } as unknown as Message
    const commandOutput = {
      type: 'system',
      subtype: 'local_command',
      uuid: 'local-output',
      timestamp: '2026-08-01T12:35:00.001Z',
      content: 'status output',
    } as unknown as Message
    const batch = [commandInput, commandOutput]
    const delivered: DirectTuiRendererEvent[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))

    publishDirectTuiInputEvents(batch, sink)

    expect(delivered).toEqual(batch)
    expect(delivered.filter(event => event === commandInput)).toHaveLength(1)
    expect(delivered.filter(event => event === commandOutput)).toHaveLength(1)

    const unobserved = {
      type: 'user',
      uuid: 'unobserved-input',
      timestamp: '2026-08-01T12:35:01.000Z',
      message: { role: 'user', content: 'ordinary SDK input' },
    } as unknown as Message
    expect(isDirectTuiRendererEvent(unobserved)).toBe(false)
    expect(() =>
      publishDirectTuiInputEvents([unobserved], undefined),
    ).not.toThrow()
    expect(isDirectTuiRendererEvent(unobserved)).toBe(false)
  })

  test('observes raw query events before QueryEngine SDK normalization and wires only the direct route', () => {
    const queryEngine = readFileSync(resolve(ROOT, 'src/QueryEngine.ts'), 'utf8')
    const executionCore = readFileSync(
      resolve(ROOT, 'src/cli/print/queryExecutionCore.ts'),
      'utf8',
    )
    const bootstrap = readFileSync(
      resolve(ROOT, 'src/cli/tuiRuntimeBootstrap.ts'),
      'utf8',
    )

    const loopStart = queryEngine.indexOf(
      'for await (const message of query({',
    )
    const observation = queryEngine.indexOf(
      'this.config.onQueryEvent?.(message)',
      loopStart,
    )
    const sdkSwitch = queryEngine.indexOf('switch (message.type)', loopStart)
    expect(loopStart).toBeGreaterThanOrEqual(0)
    expect(observation).toBeGreaterThan(loopStart)
    expect(observation).toBeLessThan(sdkSwitch)
    expect(
      queryEngine.match(/this\.config\.onQueryEvent\?\.\(message\)/g),
    ).toHaveLength(1)

    const systemInitYield = queryEngine.indexOf('yield buildSystemInitMessage({')
    const inputObservation = queryEngine.indexOf(
      'publishDirectTuiInputEvents(',
      systemInitYield,
    )
    const localBranch = queryEngine.indexOf('if (!shouldQuery) {')
    const localSdkProjection = queryEngine.indexOf(
      'for (const msg of messagesFromUserInput)',
      localBranch,
    )
    expect(systemInitYield).toBeGreaterThanOrEqual(0)
    expect(inputObservation).toBeGreaterThan(systemInitYield)
    expect(inputObservation).toBeLessThan(localBranch)
    expect(localBranch).toBeGreaterThanOrEqual(0)
    expect(localSdkProjection).toBeGreaterThan(localBranch)
    expect(
      queryEngine.match(/publishDirectTuiInputEvents\(/g),
    ).toHaveLength(1)
    expect(queryEngine).not.toContain('localEventObserver')

    expect(executionCore).toContain('querySource: getQuerySourceForREPL()')
    expect(executionCore).toContain('directQueryEventDelivery: true')
    expect(executionCore).toContain('directQueryEventDelivery: false')
    expect(executionCore).toContain('suppressHookEventDelivery()')
    expect(bootstrap).toContain('suppressHookEventDelivery()')
    expect(bootstrap.indexOf('suppressHookEventDelivery()')).toBeLessThan(
      bootstrap.indexOf('const sessionStartHooksPromise ='),
    )
    expect(executionCore).toContain('onQueryEvent: directQueryEventSink')
    expect(executionCore).toContain('output.enqueue(event)')
    expect(executionCore).toContain('isDirectTuiRendererEvent(message)')
    expect(executionCore).toContain(
      'await structuredIO.write(message)',
    )
    expect(executionCore).not.toContain(
      'output.enqueue(event as unknown as StdoutMessage)',
    )
    expect(executionCore).toContain(
      '!isDirectTuiControlPlaneSdkMessage(message)',
    )
    const structuredIO = readFileSync(
      resolve(ROOT, 'src/cli/structuredIO.ts'),
      'utf8',
    )
    expect(structuredIO).toContain('| DirectTuiRendererEvent')
  })

  test('does not widen public SDK/control schemas for direct renderer events', () => {
    const publicSchemaPaths = [
      'src/entrypoints/sdk/coreSchemas.ts',
      'src/entrypoints/sdk/controlSchemas.ts',
      'src/entrypoints/sdk/controlTypes.ts',
    ]
    for (const path of publicSchemaPaths) {
      const schema = readFileSync(resolve(ROOT, path), 'utf8')
      expect(schema, path).not.toContain('DirectTuiRendererEvent')
      expect(schema, path).not.toContain('DIRECT_TUI_RENDERER_EVENT_TYPES')
      expect(schema, path).not.toContain('crabcode_tui_query_event')
      expect(schema, path).not.toContain('crabcode_tui_setup')
    }
  })

  test('restores the historical notification hook as a process-local ToolUseContext callback only', () => {
    const queryEngine = readFileSync(resolve(ROOT, 'src/QueryEngine.ts'), 'utf8')
    const executionCore = readFileSync(
      resolve(ROOT, 'src/cli/print/queryExecutionCore.ts'),
      'utf8',
    )
    const toolContract = readFileSync(
      resolve(ROOT, 'src/types/toolContracts.ts'),
      'utf8',
    )
    const privateRendererProtocol = readFileSync(
      resolve(ROOT, 'src/cli/crabcodeTuiBridgeProtocol.ts'),
      'utf8',
    )

    expect(
      queryEngine.match(
        /sendOSNotification: this\.config\.sendOSNotification/g,
      ),
    ).toHaveLength(2)
    expect(executionCore).toContain(
      'sendOSNotification: routePolicy.directQueryEventDelivery',
    )
    expect(executionCore).toContain('void executeNotificationHooks(opts)')
    expect(toolContract).not.toContain('sendOSNotification')
    expect(privateRendererProtocol).not.toContain('sendOSNotification')
  })
})
