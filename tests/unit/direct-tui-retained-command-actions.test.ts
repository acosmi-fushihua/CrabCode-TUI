import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

import {
  DirectTuiRetainedCommandActionSchema,
  DirectTuiRetainedCommandResultSchema,
  handleDirectTuiRetainedCommandAction,
  type DirectTuiRetainedCommandDependencies,
  type DirectTuiRetainedCommandSurface,
} from '../../src/cli/directTuiRetainedCommandActions.js'
import {
  CRABCODE_TUI_RUNTIME_ACTION_TYPE,
  CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
  CRABCODE_TUI_RUNTIME_RESULT_TYPE,
  routeDirectTuiRuntimeAction,
} from '../../src/cli/directTuiRuntimeActions.js'
import {
  getDefaultAppState,
  type AppState,
} from '../../src/state/AppStateStore.js'

function harness(initial?: (state: AppState) => AppState): {
  surface: DirectTuiRetainedCommandSurface
  getState: () => AppState
  appendedMetaMessages: string[]
} {
  let state = initial?.(getDefaultAppState()) ?? getDefaultAppState()
  const appendedMetaMessages: string[] = []
  return {
    surface: {
      getAppState: () => state,
      setAppState: updater => {
        state = updater(state)
      },
      getMessages: () => [],
      appendMetaMessages: contents => {
        appendedMetaMessages.push(...contents)
      },
    },
    getState: () => state,
    appendedMetaMessages,
  }
}

function dependencies(
  overrides: Partial<DirectTuiRetainedCommandDependencies> = {},
): DirectTuiRetainedCommandDependencies {
  return {
    isTeammate: () => false,
    isBriefCommandEnabled: () => true,
    isBriefEntitled: () => true,
    async invokeColor(surface, argument) {
      const normalized = argument.trim().toLowerCase()
      const color = ['default', 'reset', 'none', 'gray', 'grey'].includes(
        normalized,
      )
        ? undefined
        : (normalized as 'red')
      surface.setAppState(previous => ({
        ...previous,
        standaloneAgentContext: {
          ...previous.standaloneAgentContext,
          name: previous.standaloneAgentContext?.name ?? '',
          color,
        },
      }))
      return {
        completionCalled: true,
        stateUpdateObserved: true,
        metaMessages: [],
      }
    },
    async invokeRename(surface, argument) {
      const name = argument.trim() || 'Generated session name'
      surface.setAppState(previous => ({
        ...previous,
        standaloneAgentContext: {
          ...previous.standaloneAgentContext,
          name,
        },
      }))
      return {
        completionCalled: true,
        stateUpdateObserved: true,
        metaMessages: [],
      }
    },
    async invokeVim() {
      return { editorMode: 'vim' }
    },
    async invokeBrief(surface) {
      surface.setAppState(previous => ({
        ...previous,
        isBriefOnly: !previous.isBriefOnly,
      }))
      return {
        completionCalled: true,
        stateUpdateObserved: true,
        metaMessages: ['exact fixed-owner reminder'],
      }
    },
    reportError: () => {},
    ...overrides,
  }
}

describe('direct native TUI retained command actions', () => {
  test('the adapter imports exact retained owners and no AppServer/public protocol', async () => {
    const source = await readFile(
      new URL(
        '../../src/cli/directTuiRetainedCommandActions.ts',
        import.meta.url,
      ),
      'utf8',
    )
    for (const owner of [
      '../commands/color/color.js',
      '../commands/rename/rename.js',
      '../commands/vim/vim.js',
      '../commands/brief.js',
    ]) {
      expect(source).toContain(owner)
    }
    expect(source).not.toMatch(/appServer|agentSdkTypes|control_request/)
    expect(source).not.toMatch(/method\s*:\s*z\.|args\s*:\s*z\./)
  })

  test('the closed schema exposes only the proven retained authority actions', () => {
    for (const action of [
      { kind: 'retained.identity.snapshot' },
      { kind: 'retained.color.apply', argument: 'blue' },
      { kind: 'retained.rename.apply', argument: 'name' },
      { kind: 'retained.vim.toggle' },
      { kind: 'retained.brief.toggle' },
    ]) {
      expect(DirectTuiRetainedCommandActionSchema.safeParse(action).success).toBe(
        true,
      )
    }

    for (const kind of [
      'retained.branch.apply',
      'retained.fork.apply',
      'retained.rewind.open',
      'retained.checkpoint.open',
      'arbitrary_method',
    ]) {
      expect(
        DirectTuiRetainedCommandActionSchema.safeParse({
          kind,
          args: { opaque: true },
        }).success,
      ).toBe(false)
    }
  })

  test('identity snapshot restores the exact existing standalone state without mutation', async () => {
    const { surface, getState } = harness(state => ({
      ...state,
      standaloneAgentContext: { name: 'Restored title', color: 'purple' },
    }))
    const before = getState()

    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.identity.snapshot' },
        surface,
        dependencies(),
      ),
    ).toEqual({
      kind: 'retained.identity.snapshot',
      name: 'Restored title',
      color: 'purple',
    })
    expect(getState()).toBe(before)
  })

  test('color validates exact historical values before invoking its owner', async () => {
    const { surface } = harness()
    let invocationCount = 0
    const deps = dependencies({
      async invokeColor(target, argument) {
        invocationCount += 1
        return dependencies().invokeColor(target, argument)
      },
    })

    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.color.apply', argument: ' BLUE ' },
        surface,
        deps,
      ),
    ).toEqual({ kind: 'retained.color.updated', color: 'blue' })
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.color.apply', argument: 'reset' },
        surface,
        deps,
      ),
    ).toEqual({ kind: 'retained.color.updated', color: null })
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.color.apply', argument: '' },
        surface,
        deps,
      ),
    ).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.color.apply',
      code: 'argument_required',
    })
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.color.apply', argument: 'ultraviolet' },
        surface,
        deps,
      ),
    ).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.color.apply',
      code: 'invalid_argument',
    })
    expect(invocationCount).toBe(2)
  })

  test('teammate restrictions remain backend-owned and do not mutate state', async () => {
    const { surface } = harness()
    let invocationCount = 0
    const deps = dependencies({
      isTeammate: () => true,
      async invokeColor(target, argument) {
        invocationCount += 1
        return dependencies().invokeColor(target, argument)
      },
      async invokeRename(target, argument) {
        invocationCount += 1
        return dependencies().invokeRename(target, argument)
      },
    })

    for (const action of [
      { kind: 'retained.color.apply', argument: 'red' } as const,
      { kind: 'retained.rename.apply', argument: 'leader' } as const,
    ]) {
      expect(
        await handleDirectTuiRetainedCommandAction(action, surface, deps),
      ).toMatchObject({
        kind: 'retained_command_error',
        code: 'teammate_restricted',
      })
    }
    expect(invocationCount).toBe(0)
  })

  test('rename returns the exact state committed by the existing owner', async () => {
    const { surface } = harness()
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.rename.apply', argument: '  New title  ' },
        surface,
        dependencies(),
      ),
    ).toEqual({ kind: 'retained.rename.updated', name: 'New title' })

    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.rename.apply', argument: '' },
        surface,
        dependencies({
          invokeRename: async () => ({
            completionCalled: true,
            stateUpdateObserved: false,
            metaMessages: [],
          }),
        }),
      ),
    ).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.rename.apply',
      code: 'name_generation_unavailable',
    })
  })

  test('brief preserves the off escape, entitlement gate, and one meta injection', async () => {
    const enabledHarness = harness()
    const disabledGate = dependencies({ isBriefCommandEnabled: () => false })
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.brief.toggle' },
        enabledHarness.surface,
        disabledGate,
      ),
    ).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.brief.toggle',
      code: 'command_unavailable',
    })

    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.brief.toggle' },
        enabledHarness.surface,
        dependencies({ isBriefEntitled: () => false }),
      ),
    ).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.brief.toggle',
      code: 'not_entitled',
    })

    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.brief.toggle' },
        enabledHarness.surface,
        dependencies(),
      ),
    ).toEqual({
      kind: 'retained.brief.updated',
      enabled: true,
      reminder_injected: true,
    })
    expect(enabledHarness.appendedMetaMessages).toEqual([
      'exact fixed-owner reminder',
    ])

    const alreadyOn = harness(state => ({ ...state, isBriefOnly: true }))
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.brief.toggle' },
        alreadyOn.surface,
        dependencies({
          isBriefCommandEnabled: () => false,
          isBriefEntitled: () => false,
        }),
      ),
    ).toMatchObject({ kind: 'retained.brief.updated', enabled: false })
  })

  test('vim is surface-free while other actions fail request-locally', async () => {
    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.vim.toggle' },
        undefined,
        dependencies({ invokeVim: async () => ({ editorMode: 'normal' }) }),
      ),
    ).toEqual({ kind: 'retained.vim.updated', editor_mode: 'normal' })

    expect(
      await handleDirectTuiRetainedCommandAction(
        { kind: 'retained.rename.apply', argument: 'x' },
        undefined,
        dependencies(),
      ),
    ).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.rename.apply',
      code: 'surface_unavailable',
    })
  })

  test('authority failures return a typed DTO without raw exception data', async () => {
    const { surface } = harness()
    const observedErrors: unknown[] = []
    const result = await handleDirectTuiRetainedCommandAction(
      { kind: 'retained.color.apply', argument: 'cyan' },
      surface,
      dependencies({
        invokeColor: async () => {
          throw new Error('private path and credential must not cross the wire')
        },
        reportError: error => {
          observedErrors.push(error)
        },
      }),
    )

    expect(result).toEqual({
      kind: 'retained_command_error',
      action_kind: 'retained.color.apply',
      code: 'authority_failure',
    })
    expect(JSON.stringify(result)).not.toContain('private path')
    expect(observedErrors).toHaveLength(1)
    expect(() => DirectTuiRetainedCommandResultSchema.parse(result)).not.toThrow()
  })

  test('the central private router dispatches the retained union without widening public control', async () => {
    const { surface } = harness()
    expect(
      await routeDirectTuiRuntimeAction(
        {
          type: CRABCODE_TUI_RUNTIME_ACTION_TYPE,
          protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
          request_id: 'retained-color-1',
          action: { kind: 'retained.color.apply', argument: 'green' },
        },
        {
          retainedCommandSurface: surface,
          retainedCommandDependencies: dependencies(),
        },
      ),
    ).toEqual({
      handled: true,
      response: {
        type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'retained-color-1',
        result: { kind: 'retained.color.updated', color: 'green' },
      },
    })
  })
})
