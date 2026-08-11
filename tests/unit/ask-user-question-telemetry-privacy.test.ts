import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import type { Span } from '@opentelemetry/api'

import { shouldRedactToolTelemetryContent } from '../../src/services/tools/toolExecution.js'
import {
  addBetaLLMRequestAttributes,
  addBetaToolInputAttributes,
  addBetaToolResultAttributes,
  clearBetaTracingState,
  shouldRedactToolTraceContent,
} from '../../src/utils/telemetry/betaSessionTracing.js'
import {
  createAssistantMessage,
  createUserMessage,
} from '../../src/utils/messages/factory.js'

const originalTracingEnv = {
  enabled: process.env.ENABLE_BETA_TRACING_DETAILED,
  endpoint: process.env.BETA_TRACING_ENDPOINT,
  userType: process.env.USER_TYPE,
}

beforeEach(() => {
  clearBetaTracingState()
  process.env.ENABLE_BETA_TRACING_DETAILED = '1'
  process.env.BETA_TRACING_ENDPOINT = 'http://telemetry.invalid'
  process.env.USER_TYPE = 'ant'
})

afterEach(() => {
  clearBetaTracingState()
  restoreEnv('ENABLE_BETA_TRACING_DETAILED', originalTracingEnv.enabled)
  restoreEnv('BETA_TRACING_ENDPOINT', originalTracingEnv.endpoint)
  restoreEnv('USER_TYPE', originalTracingEnv.userType)
})

describe('AskUserQuestion telemetry privacy', () => {
  test('redacts question content from ordinary tool telemetry', () => {
    expect(shouldRedactToolTelemetryContent('AskUserQuestion')).toBe(true)
    expect(shouldRedactToolTelemetryContent('Read')).toBe(false)
  })

  test('redacts detailed tool input and result content at the tracing sink', () => {
    const capturedInput: Record<string, unknown> = {}
    const span = {
      setAttributes(attributes: Record<string, unknown>) {
        Object.assign(capturedInput, attributes)
        return this
      },
    } as unknown as Span
    const capturedResult: Record<string, string | number | boolean> = {}
    const secret = 'private Other answer and notes'

    expect(shouldRedactToolTraceContent('AskUserQuestion')).toBe(true)
    addBetaToolInputAttributes(span, 'AskUserQuestion', secret)
    addBetaToolResultAttributes(
      capturedResult,
      'AskUserQuestion',
      secret,
    )

    expect(capturedInput).toEqual({})
    expect(capturedResult).toEqual({})
  })

  test('does not disable detailed tracing for unrelated tools', () => {
    const captured: Record<string, unknown> = {}
    const span = {
      setAttributes(attributes: Record<string, unknown>) {
        Object.assign(captured, attributes)
        return this
      },
    } as unknown as Span

    addBetaToolInputAttributes(span, 'Read', '{"file_path":"safe.txt"}')

    expect(shouldRedactToolTraceContent('Read')).toBe(false)
    expect(captured.tool_input).toContain('[TOOL INPUT: Read]')
  })

  test('redacts Ask results from LLM new_context while retaining ordinary tool results', () => {
    const firstRequestAttributes: Record<string, unknown> = {}
    const secondRequestAttributes: Record<string, unknown> = {}
    const askSecret = 'private selected option, Other answer, and notes'
    const readResult = 'ordinary Read result remains useful in tracing'
    const askToolUseID = 'toolu_ask_private'
    const readToolUseID = 'toolu_read_safe'
    const querySource = 'privacy-regression-cross-window'

    const priorHistory = [
      createUserMessage({ content: 'initial user message' }),
      createAssistantMessage({
        content: [
          {
            type: 'tool_use',
            id: askToolUseID,
            name: 'AskUserQuestion',
            input: { questions: [{ question: 'private question' }] },
          },
          {
            type: 'tool_use',
            id: readToolUseID,
            name: 'Read',
            input: { file_path: 'safe.txt' },
          },
        ],
      }),
    ]

    // Establish an incremental boundary after both assistant tool_use blocks.
    addBetaLLMRequestAttributes(
      createCapturingSpan(firstRequestAttributes),
      { querySource },
      priorHistory,
    )

    addBetaLLMRequestAttributes(
      createCapturingSpan(secondRequestAttributes),
      { querySource },
      [
        ...priorHistory,
        createUserMessage({
          content: [
            {
              type: 'tool_result',
              tool_use_id: askToolUseID,
              content: askSecret,
            },
            {
              type: 'tool_result',
              tool_use_id: readToolUseID,
              content: readResult,
            },
          ],
        }),
      ],
    )

    const serializedAttributes = JSON.stringify(secondRequestAttributes)
    expect(serializedAttributes).not.toContain(askSecret)
    expect(serializedAttributes).toContain(
      '[INTERACTIVE QUESTION RESULT REDACTED]',
    )
    expect(serializedAttributes).toContain(readResult)
    expect(secondRequestAttributes.new_context_message_count).toBe(1)
  })
})

function createCapturingSpan(attributes: Record<string, unknown>): Span {
  return {
    setAttribute(name: string, value: unknown) {
      attributes[name] = value
      return this
    },
    setAttributes(values: Record<string, unknown>) {
      Object.assign(attributes, values)
      return this
    },
  } as unknown as Span
}

function restoreEnv(name: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name]
  } else {
    process.env[name] = value
  }
}
