import { afterEach, beforeEach, describe, expect, spyOn, test } from 'bun:test'
import type { StreamEvent } from '@acosmi/sdk-ts'
import {
  _resetAcosmiStreamDiagnosticsForTesting,
  AcosmiStreamDecodeError,
  normalizeAcosmiChatStreamEvent,
} from '../../src/services/acosmi/client.js'

const stream = (event: string, value: unknown): StreamEvent => ({
  event,
  data: typeof value === 'string' ? value : JSON.stringify(value),
})

beforeEach(() => {
  _resetAcosmiStreamDiagnosticsForTesting()
})

afterEach(() => {
  _resetAcosmiStreamDiagnosticsForTesting()
})

describe('normalizeAcosmiChatStreamEvent', () => {
  test('suppresses explicit and SSE-name empty sources', () => {
    const warning = spyOn(console, 'warn').mockImplementation(() => {})
    expect(
      normalizeAcosmiChatStreamEvent(
        stream('message', { type: 'sources', sources: [] }),
      ),
    ).toBeUndefined()
    expect(
      normalizeAcosmiChatStreamEvent(stream('sources', { sources: [] })),
    ).toBeUndefined()
    expect(warning).toHaveBeenCalledTimes(1)
    expect(warning.mock.calls[0]?.[0]).toContain('sources_empty_noop')
    warning.mockRestore()
  })

  test('canonicalizes both non-empty sources forms', () => {
    const value = {
      sources: [{ title: 'A', url: 'https://example.test/a' }],
      session_id: 'session-1',
    }
    expect(normalizeAcosmiChatStreamEvent(stream('sources', value))).toEqual({
      ...value,
      type: 'sources',
    })
    expect(
      normalizeAcosmiChatStreamEvent(
        stream('message', { type: 'sources', ...value }),
      ),
    ).toEqual({ type: 'sources', ...value })
  })

  test('suppresses malformed sources with a bounded issue code', () => {
    const warning = spyOn(console, 'warn').mockImplementation(() => {})
    expect(
      normalizeAcosmiChatStreamEvent(
        stream('sources', { sources: [{ title: 'A', url: 7 }] }),
      ),
    ).toBeUndefined()
    expect(warning).toHaveBeenCalledTimes(1)
    expect(warning.mock.calls[0]?.[0]).toContain('source_url_invalid')
    expect(warning.mock.calls[0]?.[0]).not.toContain('example')
    warning.mockRestore()
  })

  test('preserves ordinary JSON events', () => {
    const event = { type: 'message_stop' }
    expect(normalizeAcosmiChatStreamEvent(stream('message', event))).toEqual(
      event,
    )
  })

  test('accepts the SSE done sentinel as a no-op', () => {
    expect(
      normalizeAcosmiChatStreamEvent({ event: 'message', data: '[DONE]' }),
    ).toBeUndefined()
  })

  test('raises a typed turn-scoped error for ordinary invalid JSON', () => {
    try {
      normalizeAcosmiChatStreamEvent({
        event: 'private-user-content',
        data: '{',
      })
      throw new Error('expected AcosmiStreamDecodeError')
    } catch (error) {
      expect(error).toBeInstanceOf(AcosmiStreamDecodeError)
      expect(error).toMatchObject({
        eventNameEncodedLength: 20,
      })
      expect(String(error)).not.toContain('private-user-content')
    }
  })
})
