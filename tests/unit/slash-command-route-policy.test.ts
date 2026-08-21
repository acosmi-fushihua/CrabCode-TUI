import { describe, expect, test } from 'bun:test'

import {
  commandInventoryForRoute,
  commandLoaderForRoute,
} from '../../src/cli/print/slashCommandRoutePolicy.js'
import type { ToolPresentationNode } from '../../src/Tool.js'
import type { Command } from '../../src/types/command.js'
import { processSlashCommandCore } from '../../src/utils/processUserInput/processSlashCommandCore.js'
import type { ProcessUserInputContext } from '../../src/utils/processUserInput/processUserInputCore.js'

const command = (name: string) => ({ name }) as Command

function dispatcherContext(commands: Command[]): ProcessUserInputContext {
  const state = {
    mcp: { clients: [] },
    toolPermissionContext: { mode: 'default' },
  }
  return {
    abortController: new AbortController(),
    getAppState: () => state,
    messages: [],
    options: {
      agentDefinitions: { activeAgents: [], allAgents: [] },
      commands,
      ideInstallationStatus: null,
      isNonInteractiveSession: false,
      mainLoopModel: 'disabled-slash-fixture-model',
      theme: 'dark',
      tools: [],
    },
    onChangeAPIKey: () => {},
    setAppState: () => {},
    setMessages: () => {},
    setResponseLength: () => {},
  } as unknown as ProcessUserInputContext
}

describe('slash-command route policy', () => {
  test('keeps initial and later MCP inventories empty while disabled', () => {
    expect(
      commandInventoryForRoute(
        false,
        [command('builtin')],
        [command('mcp__server__initial')],
      ),
    ).toEqual([])

    expect(
      commandInventoryForRoute(
        false,
        [],
        [command('mcp__server__connected_later')],
      ),
    ).toEqual([])
  })

  test('keeps every asynchronous refresh empty without invoking discovery', async () => {
    let discoveryCalls = 0
    const loader = commandLoaderForRoute(false, async currentCwd => {
      discoveryCalls += 1
      return [command(`skill:${currentCwd}`)]
    })

    expect(await loader('/first')).toEqual([])
    expect(await loader('/after-plugin-auth-refresh')).toEqual([])
    expect(discoveryCalls).toBe(0)
  })

  test('keeps refreshed skill and MCP tokens on the real unknown-skill dispatcher path', async () => {
    let executionCount = 0
    const executable = (name: string) =>
      ({
        name,
        type: 'prompt',
        async getPromptForCommand() {
          executionCount += 1
          return 'must not execute'
        },
      }) as Command
    const commands = commandInventoryForRoute(
      false,
      [executable('plugin:refreshed-skill')],
      [executable('mcp__server__connected_later')],
    )

    for (const token of [
      'plugin:refreshed-skill',
      'mcp__server__connected_later',
      'server:connected-later (MCP)',
    ]) {
      const result = await processSlashCommandCore(
        `/${token}`,
        [],
        [],
        [],
        dispatcherContext(commands),
        (_presentation: ToolPresentationNode | null) => {},
        undefined,
        false,
        undefined,
        {
          isBuiltInCommandName: () => false,
          isSubscriberGatedCommandName: () => false,
          failClosedUnknownMcp: () => true,
        },
      )

      expect(result.shouldQuery).toBe(false)
      expect(result.resultText).toBe(`Unknown skill: ${token}`)
    }
    expect(executionCount).toBe(0)
  })

  test('fails a disconnected friendly MCP command closed and preserves its args', async () => {
    const result = await processSlashCommandCore(
      '/server:disconnected (MCP) preserve these args',
      [],
      [],
      [],
      dispatcherContext([]),
      (_presentation: ToolPresentationNode | null) => {},
      undefined,
      false,
      undefined,
      {
        isBuiltInCommandName: () => false,
        isSubscriberGatedCommandName: () => false,
        failClosedUnknownMcp: () => true,
      },
    )

    expect(result.shouldQuery).toBe(false)
    expect(result.resultText).toBe(
      'Unknown skill: server:disconnected (MCP)',
    )
    expect(JSON.stringify(result.messages)).toContain(
      'Args from unknown skill: preserve these args',
    )
  })

  test('preserves standard-route inventory and loader behavior when enabled', async () => {
    const builtin = command('builtin')
    const mcp = command('mcp__server__prompt')
    const loader = async (currentCwd: string) => [
      command(`skill:${currentCwd}`),
    ]

    expect(commandInventoryForRoute(true, [builtin], [mcp])).toEqual([
      builtin,
      mcp,
    ])
    expect(commandLoaderForRoute(true, loader)).toBe(loader)
    expect(await commandLoaderForRoute(true, loader)('/workspace')).toEqual([
      command('skill:/workspace'),
    ])
  })

  test('preserves standard unknown-friendly MCP fallthrough', async () => {
    const result = await processSlashCommandCore(
      '/server:disconnected (MCP) preserve these args',
      [],
      [],
      [],
      dispatcherContext([]),
      (_presentation: ToolPresentationNode | null) => {},
      undefined,
      false,
      undefined,
      {
        isBuiltInCommandName: () => false,
        isSubscriberGatedCommandName: () => false,
      },
    )

    expect(result.shouldQuery).toBe(true)
    expect(result.resultText).toBeUndefined()
  })
})
