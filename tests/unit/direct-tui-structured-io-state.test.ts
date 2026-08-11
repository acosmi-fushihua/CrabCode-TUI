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
import {
  assertInteractivePermissionDecisionResolved,
  DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME,
  UNRESOLVED_DIRECT_TUI_PERMISSION_MESSAGE,
  withDirectTuiPermissionBridge,
} from '../../src/cli/directTuiPermissionBridge.js'

afterEach(() => {
  drainSdkEvents()
  resetStateForTests()
})

describe('StructuredIO transport state', () => {
  test('native TUI permission transport is fixed and cannot be overridden by caller options', () => {
    expect(withDirectTuiPermissionBridge({}).permissionPromptToolName).toBe(
      DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME,
    )
    expect(
      withDirectTuiPermissionBridge({
        permissionPromptToolName: 'mcp__host__prompt',
      }).permissionPromptToolName,
    ).toBe('stdio')
  })

  test('interactive StructuredIO fails closed if an unresolved ask reaches execution', () => {
    const ask = {
      behavior: 'ask' as const,
      message: 'requires approval',
      decisionReason: {
        type: 'safetyCheck' as const,
        reason: 'protected configuration',
        classifierApprovable: true,
      },
    }

    expect(() =>
      assertInteractivePermissionDecisionResolved(ask, {
        interactive: true,
        structuredIo: true,
      }),
    ).toThrow(UNRESOLVED_DIRECT_TUI_PERMISSION_MESSAGE)
    expect(() =>
      assertInteractivePermissionDecisionResolved(ask, {
        interactive: false,
        structuredIo: true,
      }),
    ).not.toThrow()
    expect(() =>
      assertInteractivePermissionDecisionResolved(ask, {
        interactive: true,
        structuredIo: false,
      }),
    ).not.toThrow()
    expect(() =>
      assertInteractivePermissionDecisionResolved(
        {
          behavior: 'deny',
          message: 'denied',
          decisionReason: { type: 'other', reason: 'denied' },
        },
        { interactive: true, structuredIo: true },
      ),
    ).not.toThrow()
  })

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
