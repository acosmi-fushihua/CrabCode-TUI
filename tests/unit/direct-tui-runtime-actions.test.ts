import { describe, expect, test } from 'bun:test'

import {
  CRABCODE_TUI_RUNTIME_ACTION_TYPE,
  CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
  CRABCODE_TUI_RUNTIME_RESULT_TYPE,
  createDirectTuiRuntimeActionRouter,
  DirectTuiRuntimeActionRequestSchema,
  DirectTuiRuntimeActionResultSchema,
  routeDirectTuiRuntimeAction,
} from '../../src/cli/directTuiRuntimeActions.js'
import { StructuredIO } from '../../src/cli/structuredIO.js'

const request = (requestId: string) => ({
  type: CRABCODE_TUI_RUNTIME_ACTION_TYPE,
  protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
  request_id: requestId,
  action: { kind: 'health_snapshot' as const },
})

describe('direct native TUI private runtime actions', () => {
  test('health snapshot roundtrip preserves the exact request id', async () => {
    const value = request('health-1')
    expect(DirectTuiRuntimeActionRequestSchema.parse(value)).toEqual(value)

    const routed = await routeDirectTuiRuntimeAction(value)
    expect(routed).toEqual({
      handled: true,
      response: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'health-1',
        result: { kind: 'health_snapshot', status: 'ready' },
      },
    })
    expect(() =>
      DirectTuiRuntimeActionResultSchema.parse(routed.response),
    ).not.toThrow()
  })

  test('unknown and malformed private actions fail closed without throwing', async () => {
    expect(
      await routeDirectTuiRuntimeAction({
        ...request('unknown-1'),
        action: { kind: 'arbitrary_method', args: { unsafe: true } },
      }),
    ).toEqual({
      handled: true,
      response: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'unknown-1',
        result: { kind: 'runtime_action_error', code: 'unknown_action' },
      },
    })

    expect(
      await routeDirectTuiRuntimeAction({
        ...request('malformed-1'),
        protocol_version: 2,
      }),
    ).toEqual({
      handled: true,
      response: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'malformed-1',
        result: { kind: 'runtime_action_error', code: 'invalid_request' },
      },
    })

    expect(
      await routeDirectTuiRuntimeAction({
        type: CRABCODE_TUI_RUNTIME_ACTION_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        action: { kind: 'health_snapshot' },
      }),
    ).toEqual({ handled: true })

    for (const [requestId, action] of [
      [
        'malformed-model',
        {
          kind: 'model.custom.test_draft',
          apiKey: 'must-not-be-logged',
        },
      ],
      ['malformed-plugin', { kind: 'plugin_install', plugin_id: 'p' }],
    ] as const) {
      expect(
        await routeDirectTuiRuntimeAction({
          ...request(requestId),
          action,
        }),
      ).toEqual({
        handled: true,
        response: {
          type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
          protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
          request_id: requestId,
          result: { kind: 'runtime_action_error', code: 'invalid_request' },
        },
      })
    }
  })

  test('StructuredIO keeps one reader, emits the private result on its FIFO, and continues', async () => {
    const inbound = lines([
      JSON.stringify(request('health-through-io')),
      JSON.stringify({
        type: 'user',
        session_id: '',
        message: { role: 'user', content: 'after private request' },
        parent_tool_use_id: null,
      }),
    ])
    const io = new StructuredIO(inbound)
    io.setDirectTuiRuntimeActionRouter(routeDirectTuiRuntimeAction)

    const messages = []
    for await (const message of io.structuredInput) messages.push(message)

    expect(messages).toHaveLength(1)
    expect(messages[0]).toMatchObject({
      type: 'user',
      message: { role: 'user', content: 'after private request' },
    })
    expect(await io.outbound.next()).toEqual({
      done: false,
      value: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'health-through-io',
        result: { kind: 'health_snapshot', status: 'ready' },
      },
    })
  })

  test('a slow bug submission settles behind the FIFO without blocking later input or private control', async () => {
    let releaseSubmission:
      | ((value: { feedbackId: string }) => void)
      | undefined
    let submissionCalls = 0
    const pendingSubmission = new Promise<{ feedbackId: string }>(resolve => {
      releaseSubmission = resolve
    })
    const inbound = lines([
      JSON.stringify({
        ...request('bug-slow'),
        action: {
          kind: 'bug_report_submit',
          description: 'the terminal remains responsive',
        },
      }),
      JSON.stringify(request('health-after-slow-bug')),
      JSON.stringify({
        type: 'user',
        session_id: '',
        message: { role: 'user', content: 'input after slow bug report' },
        parent_tool_use_id: null,
      }),
    ])
    const io = new StructuredIO(inbound)
    io.setDirectTuiRuntimeActionRouter(
      createDirectTuiRuntimeActionRouter({
        bugReportDependencies: {
          submitBugReport: description => {
            submissionCalls += 1
            expect(description).toBe('the terminal remains responsive')
            return pendingSubmission
          },
        },
      }),
    )

    const firstInput = await resolvesBeforeDeadline(
      io.structuredInput.next(),
      'later stdin remained blocked behind bug submission',
    )
    expect(firstInput).toMatchObject({
      done: false,
      value: {
        type: 'user',
        message: { content: 'input after slow bug report' },
      },
    })
    expect(submissionCalls).toBe(1)

    expect(await resolvesBeforeDeadline(io.outbound.next(), 'health result')).toEqual({
      done: false,
      value: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'health-after-slow-bug',
        result: { kind: 'health_snapshot', status: 'ready' },
      },
    })

    // Finish the finite input first: a later background completion must still
    // be observed and must not create an unhandled rejection after input close.
    expect(await io.structuredInput.next()).toEqual({
      done: true,
      value: undefined,
    })
    releaseSubmission?.({ feedbackId: 'feedback-after-input-close' })
    expect(await resolvesBeforeDeadline(io.outbound.next(), 'bug result')).toEqual({
      done: false,
      value: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'bug-slow',
        result: {
          kind: 'bug_report_submitted',
          feedback_id: 'feedback-after-input-close',
        },
      },
    })
  })

  test('a background bug failure after input close becomes one correlated result', async () => {
    let rejectSubmission: ((reason: Error) => void) | undefined
    const pendingSubmission = new Promise<{ feedbackId: string }>(
      (_resolve, reject) => {
        rejectSubmission = reject
      },
    )
    const io = new StructuredIO(
      lines([
        JSON.stringify({
          ...request('bug-fails-after-close'),
          action: {
            kind: 'bug_report_submit',
            description: 'network failure after stdin closes',
          },
        }),
        JSON.stringify({
          type: 'user',
          session_id: '',
          message: { role: 'user', content: 'reader still progressed' },
          parent_tool_use_id: null,
        }),
      ]),
    )
    io.setDirectTuiRuntimeActionRouter(
      createDirectTuiRuntimeActionRouter({
        bugReportDependencies: {
          submitBugReport: () => pendingSubmission,
        },
      }),
    )

    expect(
      await resolvesBeforeDeadline(io.structuredInput.next(), 'later input'),
    ).toMatchObject({
      done: false,
      value: { type: 'user', message: { content: 'reader still progressed' } },
    })
    expect(await io.structuredInput.next()).toEqual({
      done: true,
      value: undefined,
    })

    rejectSubmission?.(new Error('service unavailable'))
    expect(
      await resolvesBeforeDeadline(io.outbound.next(), 'correlated failure'),
    ).toEqual({
      done: false,
      value: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'bug-fails-after-close',
        result: { kind: 'runtime_action_error', code: 'action_failed' },
      },
    })
  })

  test('a malformed correlated private action does not terminate the following normal message', async () => {
    const inbound = lines([
      JSON.stringify({
        ...request('bad-through-io'),
        extra_public_method: 'forbidden',
      }),
      JSON.stringify({
        type: 'user',
        session_id: '',
        message: { role: 'user', content: 'still alive' },
        parent_tool_use_id: null,
      }),
    ])
    const io = new StructuredIO(inbound)
    io.setDirectTuiRuntimeActionRouter(routeDirectTuiRuntimeAction)

    const messages = []
    for await (const message of io.structuredInput) messages.push(message)

    expect(messages).toHaveLength(1)
    expect(messages[0]).toMatchObject({
      type: 'user',
      message: { content: 'still alive' },
    })
    expect((await io.outbound.next()).value).toEqual({
      type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
      protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
      request_id: 'bad-through-io',
      result: { kind: 'runtime_action_error', code: 'invalid_request' },
    })
  })

  test('the private router can only be installed once', () => {
    const io = new StructuredIO(lines([]))
    io.setDirectTuiRuntimeActionRouter(routeDirectTuiRuntimeAction)
    expect(() =>
      io.setDirectTuiRuntimeActionRouter(routeDirectTuiRuntimeAction),
    ).toThrow('may be installed once')
  })

  test('the standard SDK StructuredIO path never accepts a private envelope', async () => {
    const io = new StructuredIO(
      lines([
        JSON.stringify(request('must-not-enter-standard-sdk')),
        JSON.stringify({
          type: 'user',
          session_id: '',
          message: { role: 'user', content: 'ordinary standard input' },
          parent_tool_use_id: null,
        }),
      ]),
    )

    const messages = []
    for await (const message of io.structuredInput) messages.push(message)

    expect(messages).toHaveLength(1)
    expect(messages[0]).toMatchObject({
      type: 'user',
      message: { content: 'ordinary standard input' },
    })
  })
})

async function* lines(values: string[]): AsyncGenerator<string> {
  for (const value of values) yield `${value}\n`
}

async function resolvesBeforeDeadline<T>(
  promise: Promise<T>,
  label: string,
): Promise<T> {
  let timeout: ReturnType<typeof setTimeout> | undefined
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_resolve, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`timed out waiting for ${label}`)),
          1_000,
        )
      }),
    ])
  } finally {
    if (timeout !== undefined) clearTimeout(timeout)
  }
}
