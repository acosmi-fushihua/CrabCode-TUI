import { afterEach, describe, expect, test } from 'bun:test'

import {
  resetStateForTests,
  setIsInteractive,
  setUsesStructuredIoTransport,
} from '../../src/bootstrap/state.js'
import {
  drainSdkEvents,
  enqueueSdkEvent,
} from '../../src/utils/sdkEventQueue.js'

afterEach(() => {
  drainSdkEvents()
  resetStateForTests()
})

describe('StructuredIO transport state', () => {
  test('SDK lifecycle delivery follows the transport, not product interactivity', () => {
    const cases = [
      { interactive: true, structured: false, delivered: false },
      // CrabCode's non-interactive/print route owns an SDK-event drainer even
      // without the native TUI's explicit StructuredIO flag.
      { interactive: false, structured: false, delivered: true },
      { interactive: true, structured: true, delivered: true },
      { interactive: false, structured: true, delivered: true },
    ] as const

    for (const [index, current] of cases.entries()) {
      drainSdkEvents()
      resetStateForTests()
      setIsInteractive(current.interactive)
      setUsesStructuredIoTransport(current.structured)
      enqueueSdkEvent({
        type: 'system',
        subtype: 'session_state_changed',
        state: 'running',
      })

      const events = drainSdkEvents()
      expect(events.length, `case ${index}`).toBe(
        current.delivered ? 1 : 0,
      )
      if (current.delivered) {
        expect(events[0]?.subtype).toBe('session_state_changed')
      }
    }
  })
})
