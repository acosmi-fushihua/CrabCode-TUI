import { describe, expect, test } from 'bun:test'

import {
  buildDirectTuiCommandCatalogChangedRequest,
  DirectTuiCommandCatalogLifecycle,
  DirectTuiCommandCatalogPublisher,
  DirectTuiCommandCatalogChangedRequestSchema,
  DirectTuiCommandCatalogChangedResponseSchema,
} from '../../src/cli/directTuiCommandCatalogRefresh.js'
import { StructuredIO } from '../../src/cli/structuredIO.js'

describe('direct TUI command catalog refresh private contract', () => {
  test('uses the existing correlated StructuredIO FIFO and validates the ACK', async () => {
    let deliverLine: ((line: string) => void) | undefined
    const line = new Promise<string>(resolve => {
      deliverLine = resolve
    })
    async function* input(): AsyncGenerator<string> {
      yield await line
    }
    const io = new StructuredIO(input())
    const inputCompletion = io.structuredInput.next()
    const result = io.requestDirectTuiCommandCatalogChanged([
      {
        name: 'compact',
        description: 'Compact',
        argumentHint: '',
        builtin: true,
      },
    ])
    const outbound = await io.outbound.next()
    expect(outbound.value).toMatchObject({
      type: 'control_request',
      request: {
        subtype: 'crabcode_tui_command_catalog_changed',
        protocol_version: 1,
        commands: [{ name: 'compact', builtin: true }],
      },
    })
    const requestId = (outbound.value as { request_id: string }).request_id
    deliverLine?.(
      JSON.stringify({
        type: 'control_response',
        response: {
          subtype: 'success',
          request_id: requestId,
          response: { protocol_version: 1, received: true },
        },
      }),
    )

    expect(await result).toEqual({ protocol_version: 1, received: true })
    expect(await inputCompletion).toEqual({ done: true, value: undefined })
  })

  test('preserves dispatch and presentation metadata in one closed request', () => {
    expect(
      buildDirectTuiCommandCatalogChangedRequest([
        {
          name: 'compact',
          description: 'Compact the conversation',
          argumentHint: '',
          builtin: true,
        },
        {
          name: 'hidden-skill',
          description: 'Exact dispatch only',
          argumentHint: '<value>',
          hidden: true,
        },
      ]),
    ).toEqual({
      subtype: 'crabcode_tui_command_catalog_changed',
      protocol_version: 1,
      commands: [
        {
          name: 'compact',
          description: 'Compact the conversation',
          argumentHint: '',
          builtin: true,
        },
        {
          name: 'hidden-skill',
          description: 'Exact dispatch only',
          argumentHint: '<value>',
          hidden: true,
        },
      ],
    })
  })

  test('rejects unknown fields, invalid optional booleans, and loose ACKs', () => {
    expect(
      DirectTuiCommandCatalogChangedRequestSchema.safeParse({
        subtype: 'crabcode_tui_command_catalog_changed',
        protocol_version: 1,
        commands: [
          {
            name: 'mcp:tool (MCP)',
            description: 'MCP command',
            argumentHint: '',
          },
        ],
      }).success,
    ).toBe(true)
    expect(
      DirectTuiCommandCatalogChangedRequestSchema.safeParse({
        subtype: 'crabcode_tui_command_catalog_changed',
        protocol_version: 1,
        commands: [
          {
            name: 'compact',
            description: 'Compact',
            argumentHint: '',
            hidden: false,
          },
        ],
      }).success,
    ).toBe(false)
    for (const name of [
      ' compact',
      'compact ',
      'compact\targument',
      'compact\u00a0argument',
      'mcp:tool  (MCP)',
    ]) {
      expect(
        DirectTuiCommandCatalogChangedRequestSchema.safeParse({
          subtype: 'crabcode_tui_command_catalog_changed',
          protocol_version: 1,
          commands: [{ name, description: '', argumentHint: '' }],
        }).success,
        name,
      ).toBe(false)
    }
    expect(
      DirectTuiCommandCatalogChangedRequestSchema.safeParse({
        subtype: 'crabcode_tui_command_catalog_changed',
        protocol_version: 1,
        commands: [
          { name: '   ', description: '', argumentHint: '' },
        ],
      }).success,
    ).toBe(false)
    expect(
      DirectTuiCommandCatalogChangedRequestSchema.safeParse({
        subtype: 'crabcode_tui_command_catalog_changed',
        protocol_version: 1,
        commands: [
          { name: 'compact', description: '', argumentHint: '' },
          { name: 'compact', description: '', argumentHint: '' },
        ],
      }).success,
    ).toBe(false)
    expect(
      DirectTuiCommandCatalogChangedRequestSchema.safeParse({
        subtype: 'crabcode_tui_command_catalog_changed',
        protocol_version: 1,
        commands: [],
        future: true,
      }).success,
    ).toBe(false)
    expect(
      DirectTuiCommandCatalogChangedResponseSchema.safeParse({
        protocol_version: 1,
        received: true,
      }).success,
    ).toBe(true)
    expect(
      DirectTuiCommandCatalogChangedResponseSchema.safeParse({
        protocol_version: 1,
        received: true,
        ignored: true,
      }).success,
    ).toBe(false)
  })

  test('holds pre-initialize changes and coalesces in-flight updates latest-wins', async () => {
    const sends: string[][] = []
    const releases: Array<() => void> = []
    const publisher = new DirectTuiCommandCatalogPublisher(
      commands => {
        sends.push(commands.map(command => command.name))
        return new Promise<void>(resolve => releases.push(resolve))
      },
      error => {
        throw error
      },
    )
    const entry = (name: string) => ({
      name,
      description: name,
      argumentHint: '',
    })

    publisher.update([entry('before-a')])
    publisher.update([entry('before-b')])
    expect(sends).toEqual([])

    publisher.markInitialized()
    await Promise.resolve()
    expect(sends).toEqual([['before-b']])

    publisher.update([entry('during-a')])
    publisher.update([entry('during-latest')])
    expect(sends).toEqual([['before-b']])
    releases.shift()?.()
    await Promise.resolve()
    await Promise.resolve()
    expect(sends).toEqual([['before-b'], ['during-latest']])
    releases.shift()?.()
    await publisher.whenIdle()

    publisher.close()
    publisher.update([entry('after-close')])
    await publisher.whenIdle()
    expect(sends).toEqual([['before-b'], ['during-latest']])
  })

  test('queues an owning auth response before its follow-up reverse refresh', async () => {
    const order: string[] = []
    const publisher = new DirectTuiCommandCatalogPublisher(
      async () => {
        order.push('catalog-refresh')
      },
      error => {
        throw error
      },
    )
    publisher.markInitialized()

    publisher.update([
      { name: 'account-command', description: '', argumentHint: '' },
    ])
    order.push('auth-control-response')
    await publisher.whenIdle()

    expect(order).toEqual(['auth-control-response', 'catalog-refresh'])
  })

  test('does not retry a failed snapshot until a newer change arrives', async () => {
    const attempts: string[][] = []
    const errors: string[] = []
    let fail = true
    const publisher = new DirectTuiCommandCatalogPublisher(
      async commands => {
        attempts.push(commands.map(command => command.name))
        if (fail) throw new Error('fixture transport closed')
      },
      error => errors.push(String(error)),
    )
    const entry = (name: string) => ({
      name,
      description: name,
      argumentHint: '',
    })

    publisher.markInitialized()
    publisher.update([entry('failed')])
    await publisher.whenIdle()
    expect(attempts).toEqual([['failed']])
    expect(errors).toEqual(['Error: fixture transport closed'])

    fail = false
    publisher.update([entry('newer')])
    await publisher.whenIdle()
    expect(attempts).toEqual([['failed'], ['newer']])
  })

  test('keeps async plugin, skill, reload, login, and logout producers on one newest-successful lifecycle', async () => {
    type Entry = { name: string }
    let releaseBackgroundPlugin: ((commands: Entry[]) => void) | undefined
    const backgroundPlugin = new Promise<Entry[]>(resolve => {
      releaseBackgroundPlugin = resolve
    })
    const commits: string[][] = []
    const lifecycle = new DirectTuiCommandCatalogLifecycle<Entry>(
      [{ name: 'initial' }],
      commands => commits.push(commands.map(command => command.name)),
    )

    // Non-sync installation begins first but completes after the skill watch.
    // Its older snapshot must not overwrite the newer successful refresh.
    const nonSyncInstall = lifecycle.refresh(() => backgroundPlugin)
    await lifecycle.refresh(async () => [{ name: 'skill-change' }])
    releaseBackgroundPlugin?.([{ name: 'late-plugin' }])
    await nonSyncInstall
    expect(lifecycle.snapshot()).toEqual([{ name: 'skill-change' }])

    // The sync install path awaits the same refresh seam before the first ask.
    await lifecycle.refresh(async () => [{ name: 'sync-plugin' }])

    // Manual reload and auth transitions can return commands from helpers;
    // replace() still assigns a lifecycle revision and publishes atomically.
    lifecycle.replace([{ name: 'manual-reload' }])
    lifecycle.replace([{ name: 'login-only-command' }])
    lifecycle.replace([{ name: 'logout-only-command' }])

    expect(commits).toEqual([
      ['skill-change'],
      ['sync-plugin'],
      ['manual-reload'],
      ['login-only-command'],
      ['logout-only-command'],
    ])
    expect(lifecycle.snapshot()).toEqual([{ name: 'logout-only-command' }])
  })

  test('does not let close races publish a late background refresh', async () => {
    let release: ((commands: Array<{ name: string }>) => void) | undefined
    const loaded = new Promise<Array<{ name: string }>>(resolve => {
      release = resolve
    })
    const commits: string[][] = []
    const lifecycle = new DirectTuiCommandCatalogLifecycle(
      [{ name: 'initial' }],
      commands => commits.push(commands.map(command => command.name)),
    )

    const refresh = lifecycle.refresh(() => loaded)
    lifecycle.close()
    release?.([{ name: 'too-late' }])
    await refresh

    expect(commits).toEqual([])
    expect(lifecycle.snapshot()).toEqual([{ name: 'initial' }])
    expect(lifecycle.replace([{ name: 'after-close' }])).toBe(false)
  })

  test('allows an older successful load to recover when a newer load fails', async () => {
    let releaseOlder: ((commands: Array<{ name: string }>) => void) | undefined
    const olderLoad = new Promise<Array<{ name: string }>>(resolve => {
      releaseOlder = resolve
    })
    const commits: string[][] = []
    const lifecycle = new DirectTuiCommandCatalogLifecycle(
      [{ name: 'initial' }],
      commands => commits.push(commands.map(command => command.name)),
    )

    const olderRefresh = lifecycle.refresh(() => olderLoad)
    await expect(
      lifecycle.refresh(async () => {
        throw new Error('newer skill snapshot is malformed')
      }),
    ).rejects.toThrow('newer skill snapshot is malformed')
    releaseOlder?.([{ name: 'older-valid' }])
    await olderRefresh

    expect(commits).toEqual([['older-valid']])
    expect(lifecycle.snapshot()).toEqual([{ name: 'older-valid' }])
  })

  test('contains diagnostic callback failures and sends nothing after pre-init close', async () => {
    const attempts: string[][] = []
    const publisher = new DirectTuiCommandCatalogPublisher(
      async commands => {
        attempts.push(commands.map(command => command.name))
        throw new Error('transport failed')
      },
      () => {
        throw new Error('diagnostics failed')
      },
    )

    publisher.markInitialized()
    publisher.update([
      { name: 'attempt', description: '', argumentHint: '' },
    ])
    await publisher.whenIdle()
    expect(attempts).toEqual([['attempt']])

    const preInitAttempts: string[][] = []
    const preInit = new DirectTuiCommandCatalogPublisher(
      async commands => {
        preInitAttempts.push(commands.map(command => command.name))
      },
      () => {},
    )
    preInit.update([
      { name: 'held', description: '', argumentHint: '' },
    ])
    preInit.close()
    preInit.markInitialized()
    await preInit.whenIdle()
    expect(preInitAttempts).toEqual([])

    let releaseInFlight: (() => void) | undefined
    const inFlightAttempts: string[][] = []
    const inFlight = new DirectTuiCommandCatalogPublisher(
      commands => {
        inFlightAttempts.push(commands.map(command => command.name))
        return new Promise<void>(resolve => {
          releaseInFlight = resolve
        })
      },
      () => {},
    )
    inFlight.markInitialized()
    inFlight.update([
      { name: 'already-sent', description: '', argumentHint: '' },
    ])
    await Promise.resolve()
    expect(inFlightAttempts).toEqual([['already-sent']])
    inFlight.update([
      { name: 'queued-before-close', description: '', argumentHint: '' },
    ])
    inFlight.close()
    releaseInFlight?.()
    await inFlight.whenIdle()
    expect(inFlightAttempts).toEqual([['already-sent']])
  })
})
