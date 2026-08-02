import { afterEach, describe, expect, test } from 'bun:test'
import { readFileSync } from 'fs'
import { resolve } from 'path'

import { getSessionId } from '../../src/bootstrap/state.js'
import { runWithDirectTuiProjectionOwner } from '../../src/cli/directTuiProjectionOwner.js'
import {
  _resetBackgroundAgentOwnersForTest,
  getActiveBackgroundTaskOwner,
  getBackgroundAgentOwnerThreadId,
  runWithBackgroundTaskOwner,
} from '../../src/services/agents/agentTaskMetadata.js'
import {
  registerAgentForeground,
  unregisterAgentForeground,
} from '../../src/tasks/LocalAgentTask/LocalAgentTask.js'
import type { AppState } from '../../src/state/AppState.js'

function createTaskStateHarness(): {
  getState: () => AppState
  setState: (updater: (previous: AppState) => AppState) => void
} {
  let state = { tasks: {} } as AppState
  return {
    getState: () => state,
    setState: updater => {
      state = updater(state)
    },
  }
}

describe('direct TUI projection owner boundary', () => {
  afterEach(() => {
    _resetBackgroundAgentOwnersForTest()
  })

  test('foreground agent registration receives the exact current session owner', async () => {
    const expectedOwner = getSessionId()
    const state = createTaskStateHarness()

    await runWithDirectTuiProjectionOwner(true, async () => {
      await Promise.resolve()
      expect(getActiveBackgroundTaskOwner()).toBe(expectedOwner)

      const registration = registerAgentForeground({
        agentId: 'direct-owner-agent',
        description: 'owner-bound direct agent',
        prompt: 'verify owner',
        selectedAgent: { agentType: 'general-purpose' } as never,
        setAppState: state.setState,
      })

      expect(registration.taskId).toBe('direct-owner-agent')
      expect(getBackgroundAgentOwnerThreadId(registration.taskId)).toBe(expectedOwner)
      unregisterAgentForeground(registration.taskId, state.setState)
      expect(state.getState().tasks[registration.taskId]).toBeUndefined()
    })

    expect(getActiveBackgroundTaskOwner()).toBeNull()
  })

  test('awaited and detached async descendants retain the owner without leaking to the caller', async () => {
    const expectedOwner = getSessionId()
    let releaseContinuation!: () => void
    const continuationGate = new Promise<void>(resolveGate => {
      releaseContinuation = resolveGate
    })
    let detachedOwner!: Promise<string | null>

    await runWithDirectTuiProjectionOwner(true, async () => {
      await Promise.resolve()
      expect(getActiveBackgroundTaskOwner()).toBe(expectedOwner)
      detachedOwner = (async () => {
        await continuationGate
        return getActiveBackgroundTaskOwner()
      })()
    })

    expect(getActiveBackgroundTaskOwner()).toBeNull()
    releaseContinuation()
    expect(await detachedOwner).toBe(expectedOwner)
    expect(getActiveBackgroundTaskOwner()).toBeNull()
  })

  test('standard route mode installs no new projection context', async () => {
    const observed = await runWithDirectTuiProjectionOwner(false, async () => {
      await Promise.resolve()
      return getActiveBackgroundTaskOwner()
    })
    const inherited = await runWithBackgroundTaskOwner(
      'pre-existing-owner',
      () =>
        runWithDirectTuiProjectionOwner(false, async () => {
          await Promise.resolve()
          return getActiveBackgroundTaskOwner()
        }),
    )

    expect(observed).toBeNull()
    expect(inherited).toBe('pre-existing-owner')
    expect(getActiveBackgroundTaskOwner()).toBeNull()
  })

  test('query execution gates ownership with the existing direct-only route fact', () => {
    const core = readFileSync(
      resolve(process.cwd(), 'src/cli/print/queryExecutionCore.ts'),
      'utf8',
    )
    expect(core).toContain(
      'runWithDirectTuiProjectionOwner(\n            routePolicy.directQueryEventDelivery,',
    )
    expect(core).toContain('directQueryEventDelivery: true')
    expect(core).toContain('directQueryEventDelivery: false')
  })
})
