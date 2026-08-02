import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import {
  NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
  startNativeTuiRendererSession,
} from '../../src/entrypoints/nativeTuiRendererSession.js'
import {
  CRABCODE_TUI_SETUP_SUBTYPE,
  CrabCodeTuiRendererScrollSpeedResponseSchema,
  type CrabCodeTuiSetupRequest,
} from '../../src/cli/crabcodeTuiBridgeProtocol.js'
import { Stream } from '../../src/utils/stream.js'

const ROOT = resolve(import.meta.dir, '../..')

type WireRequest = {
  type: 'control_request'
  request_id: string
  request: CrabCodeTuiSetupRequest
}

const initializeLine = JSON.stringify({
  type: 'control_request',
  request_id: 'rust-initialize',
  request: { subtype: 'initialize' },
})

function success(request: WireRequest, response: unknown): string {
  return `${JSON.stringify({
    type: 'control_response',
    response: {
      subtype: 'success',
      request_id: request.request_id,
      response,
    },
  })}\n`
}

function fixture(
  options: {
    trusted?: boolean
    trustDecision?: 'accept' | 'reject'
    initialInput?: string
    responseRequestId?: string
  } = {},
) {
  const input = new Stream<string>()
  const writes: WireRequest[] = []
  const accepted: Array<{ cwd: string; home: boolean }> = []
  let activations = 0
  let configReads = 0
  let liveCwd = '/workspace/source'
  if (options.initialInput !== undefined) {
    input.enqueue(options.initialInput)
  } else {
    input.enqueue(`${initializeLine}\n`)
  }
  return {
    input,
    writes,
    accepted,
    get activations() {
      return activations
    },
    get configReads() {
      return configReads
    },
    setCwd(cwd: string) {
      liveCwd = cwd
    },
    dependencies: {
      canonicalize: async (path: string) => path,
      currentWorkingDirectory: () => liveCwd,
      homeDirectory: () => '/home/alice',
      authority: {
        isWorkspacePathTrusted: () => options.trusted ?? false,
        getRendererConfiguration: () => {
          configReads += 1
          return {
            verbose: true,
            preferredNotificationChannel: 'iterm2_with_bell' as const,
            messageIdleNotificationThresholdMs: 60_000,
            uiLanguage: 'zh-CN' as const,
            themeSetting: 'dark' as const,
            syntaxHighlightingDisabled: false,
          }
        },
        acceptWorkspace: (cwd: string, home: boolean) => {
          accepted.push({ cwd, home })
        },
        activateInteractiveSession: () => {
          activations += 1
        },
      },
      transport: {
        input,
        async writeLine(line: string) {
          const request = JSON.parse(line) as WireRequest
          writes.push(request)
          expect(request.request.subtype).toBe(CRABCODE_TUI_SETUP_SUBTYPE)
          const response =
            request.request.kind === 'renderer_context'
              ? {
                  protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
                  kind: 'renderer_context',
                  decision: 'received',
                }
              : request.request.kind === 'workspace_trust'
                ? {
                    protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
                    kind: 'workspace_trust',
                    decision: options.trustDecision ?? 'accept',
                  }
                : request.request.kind === 'renderer_scroll_speed'
                  ? {
                      protocol_version:
                        NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
                      kind: 'renderer_scroll_speed',
                      decision: 'received',
                    }
                : undefined
          const lineResponse = success(request, response)
          if (options.responseRequestId) {
            input.enqueue(
              lineResponse.replace(
                request.request_id,
                options.responseRequestId,
              ),
            )
          } else {
            input.enqueue(lineResponse)
          }
        },
      },
    },
  }
}

async function collect(input: AsyncIterable<string>): Promise<string[]> {
  const values: string[] = []
  for await (const value of input) values.push(value)
  return values
}

describe('native TUI renderer session', () => {
  test('owns stdin early but defers config-backed renderer context until bootstrap init', async () => {
    const testFixture = fixture()
    const session = await startNativeTuiRendererSession(
      testFixture.dependencies,
    )

    expect(testFixture.configReads).toBe(0)
    expect(testFixture.writes).toEqual([])

    await session.bindRendererContext()
    expect(testFixture.configReads).toBe(1)
    expect(testFixture.writes.map(write => write.request.kind)).toEqual([
      'renderer_context',
    ])
    await expect(session.bindRendererContext()).rejects.toThrow('only once')
    expect(testFixture.configReads).toBe(1)
  })

  test('binds renderer context to setup-selected final cwd before the one trust interaction', async () => {
    const testFixture = fixture()
    const session = await startNativeTuiRendererSession(
      testFixture.dependencies,
    )
    testFixture.setCwd('/workspace/final')
    await session.bindRendererContext()
    await session.ensureWorkspaceTrust('/workspace/final')

    expect(testFixture.writes.map(write => write.request)).toEqual([
      {
        subtype: CRABCODE_TUI_SETUP_SUBTYPE,
        protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
        kind: 'renderer_context',
        cwd: '/workspace/final',
        config_verbose: true,
        preferred_notification_channel: 'iterm2_with_bell',
        message_idle_notification_threshold_ms: 60_000,
        ui_language: 'zh-CN',
        theme_setting: 'dark',
        syntax_highlighting_disabled: false,
      },
      {
        subtype: CRABCODE_TUI_SETUP_SUBTYPE,
        protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
        kind: 'workspace_trust',
      },
    ])
    expect(testFixture.accepted).toEqual([
      { cwd: '/workspace/final', home: false },
    ])
    expect(testFixture.activations).toBe(1)
    await expect(
      session.ensureWorkspaceTrust('/workspace/final'),
    ).rejects.toThrow('only once')
  })

  test('trusted final cwd emits no renderer interaction and never rewrites trust config', async () => {
    const testFixture = fixture({ trusted: true })
    const session = await startNativeTuiRendererSession(
      testFixture.dependencies,
    )
    await session.bindRendererContext()
    await session.ensureWorkspaceTrust('/workspace/source')
    expect(testFixture.writes.map(write => write.request.kind)).toEqual([
      'renderer_context',
    ])
    expect(testFixture.accepted).toEqual([])
    expect(testFixture.activations).toBe(1)
  })

  test('projects the exact post-trust scroll value once before handoff', async () => {
    const testFixture = fixture({ trusted: true })
    const session = await startNativeTuiRendererSession(
      testFixture.dependencies,
    )
    await session.bindRendererContext()
    await session.ensureWorkspaceTrust('/workspace/source')
    await session.projectRendererScrollSpeed('\u00a03rows')

    expect(testFixture.writes.at(-1)?.request).toEqual({
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
      kind: 'renderer_scroll_speed',
      raw_value: '\u00a03rows',
    })
    await expect(session.projectRendererScrollSpeed(undefined)).rejects.toThrow(
      'only once',
    )

    const absent = fixture({ trusted: true })
    const absentSession = await startNativeTuiRendererSession(
      absent.dependencies,
    )
    await absentSession.bindRendererContext()
    await absentSession.ensureWorkspaceTrust('/workspace/source')
    await absentSession.projectRendererScrollSpeed(undefined)
    expect(absent.writes.at(-1)?.request).toMatchObject({
      kind: 'renderer_scroll_speed',
      raw_value: null,
    })
    await absentSession.finishSetup()
    await expect(
      absentSession.requestSetup(
        {
          subtype: CRABCODE_TUI_SETUP_SUBTYPE,
          protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
          kind: 'renderer_scroll_speed',
          raw_value: '4',
        },
        CrabCodeTuiRendererScrollSpeedResponseSchema,
      ),
    ).rejects.toThrow('after runtime handoff')
  })

  test('rejects a cwd transition after renderer context is bound', async () => {
    const testFixture = fixture()
    const session = await startNativeTuiRendererSession(
      testFixture.dependencies,
    )
    await session.bindRendererContext()
    testFixture.setCwd('/workspace/changed-after-bind')
    await expect(
      session.ensureWorkspaceTrust('/workspace/changed-after-bind'),
    ).rejects.toThrow('renderer context /workspace/source')
    expect(testFixture.writes.map(write => write.request.kind)).toEqual([
      'renderer_context',
    ])
    expect(testFixture.activations).toBe(0)
  })

  test('home acceptance remains session-only and rejection never activates', async () => {
    const home = fixture()
    home.setCwd('/home/alice')
    const homeSession = await startNativeTuiRendererSession(home.dependencies)
    await homeSession.bindRendererContext()
    await homeSession.ensureWorkspaceTrust('/home/alice')
    expect(home.accepted).toEqual([{ cwd: '/home/alice', home: true }])

    const rejected = fixture({ trustDecision: 'reject' })
    const rejectedSession = await startNativeTuiRendererSession(
      rejected.dependencies,
    )
    await rejectedSession.bindRendererContext()
    await expect(
      rejectedSession.ensureWorkspaceTrust('/workspace/source'),
    ).rejects.toThrow('Workspace trust was declined')
    expect(rejected.activations).toBe(0)
  })

  test('stashes initialize and hands it to StructuredIO before every live stdin line', async () => {
    const testFixture = fixture()
    const session = await startNativeTuiRendererSession(
      testFixture.dependencies,
    )
    await session.bindRendererContext()
    await session.ensureWorkspaceTrust('/workspace/source')
    const runtimeInput = await session.finishSetup()
    const userLine = JSON.stringify({
      type: 'user',
      session_id: '',
      message: { role: 'user', content: 'hello' },
      parent_tool_use_id: null,
    })
    testFixture.input.enqueue(`${userLine}\n{"type":"keep_alive"}\n`)
    testFixture.input.done()

    expect(await collect(runtimeInput)).toEqual([
      `${initializeLine}\n`,
      `${userLine}\n`,
      '{"type":"keep_alive"}\n',
    ])
  })

  test('routes initialize plus a setup response from one input block without dropping either line', async () => {
    const input = new Stream<string>()
    const writes: WireRequest[] = []
    const starting = startNativeTuiRendererSession({
      canonicalize: async path => path,
      currentWorkingDirectory: () => '/workspace',
      homeDirectory: () => '/home/alice',
      authority: {
        isWorkspacePathTrusted: () => false,
        getRendererConfiguration: () => ({
          verbose: false,
          preferredNotificationChannel: 'notifications_disabled',
          messageIdleNotificationThresholdMs: 60_000,
          uiLanguage: 'en-US',
          themeSetting: 'dark',
          syntaxHighlightingDisabled: false,
        }),
        acceptWorkspace: () => {},
        activateInteractiveSession: () => {},
      },
      transport: {
        input,
        async writeLine(line) {
          const request = JSON.parse(line) as WireRequest
          writes.push(request)
          if (request.request.kind === 'renderer_context') {
            input.enqueue(
              `${initializeLine}\n${success(request, {
                protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
                kind: 'renderer_context',
                decision: 'received',
              })}`,
            )
          } else if (request.request.kind === 'workspace_trust') {
            input.enqueue(
              success(request, {
                protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
                kind: 'workspace_trust',
                decision: 'accept',
              }),
            )
          }
        },
      },
    })
    const session = await starting
    await session.bindRendererContext()
    await session.ensureWorkspaceTrust('/workspace')
    const runtime = await session.finishSetup()
    input.done()
    expect(await collect(runtime)).toEqual([`${initializeLine}\n`])
  })

  test('fails closed on duplicate initialize, unrelated setup-time input, and response mismatch', async () => {
    for (const extra of [
      initializeLine,
      JSON.stringify({ type: 'keep_alive' }),
    ]) {
      const invalid = fixture({
        initialInput: `${initializeLine}\n${extra}\n`,
      })
      const invalidSession = await startNativeTuiRendererSession(
        invalid.dependencies,
      )
      await expect(
        invalidSession.bindRendererContext(),
      ).rejects.toThrow()
    }

    const mismatch = fixture({ responseRequestId: 'wrong-request' })
    const mismatchSession = await startNativeTuiRendererSession(
      mismatch.dependencies,
    )
    await expect(
      mismatchSession.bindRendererContext(),
    ).rejects.toThrow('correlation mismatch')
  })

  test('source contracts contain no invented trust commit or startup barriers', () => {
    const protocol = readFileSync(
      resolve(ROOT, 'src/cli/crabcodeTuiBridgeProtocol.ts'),
      'utf8',
    )
    const session = readFileSync(
      resolve(ROOT, 'src/entrypoints/nativeTuiRendererSession.ts'),
      'utf8',
    )
    const setup = readFileSync(resolve(ROOT, 'src/setup.ts'), 'utf8')
    const bootstrap = readFileSync(
      resolve(ROOT, 'src/cli/tuiRuntimeBootstrap.ts'),
      'utf8',
    )
    const trustHelpers = readFileSync(
      resolve(ROOT, 'src/utils/workspaceTrustConfig.ts'),
      'utf8',
    )

    expect(protocol).not.toContain('setup_complete')
    expect(protocol).not.toContain('startup_ready')
    expect(protocol).not.toContain("phase: z.literal('commit')")
    expect(protocol).not.toContain('nonce')
    expect(protocol).not.toContain('sequence')
    expect(protocol).not.toContain('trust_state')
    expect(protocol).not.toContain("'continue', 'accept', 'reject'")
    expect(session).not.toContain('readOneProcessStdinLine')
    expect(session).toContain('const config = configAuthority.getGlobalConfig()')
    expect(session).toContain(
      'preferredNotificationChannel: config.preferredNotifChannel',
    )
    expect(session).toContain(
      'config.messageIdleNotifThresholdMs',
    )
    expect(session).toContain('normalizeThemeSetting(config.theme)')
    expect(session).toContain(
      'settingsAuthority.getInitialSettings()',
    )
    expect(session).toContain('configAuthority.saveProjectConfigForPath')
    expect(session).toContain('runtimeState.setSessionTrustAccepted(true)')
    expect(trustHelpers).not.toContain('atomicReplaceConfig')
    expect(trustHelpers).not.toContain('PersistenceObserver')
    expect(setup).not.toContain('ensureFinalWorkspaceTrust')
    expect(bootstrap).toContain(
      'await rendererSession.ensureWorkspaceTrust(getCwd())',
    )
    expect(bootstrap.indexOf('await init()')).toBeLessThan(
      bootstrap.indexOf('await setup('),
    )
    expect(bootstrap.indexOf('await setup(')).toBeLessThan(
      bootstrap.indexOf('await rendererSession.bindRendererContext()'),
    )
    expect(
      bootstrap.indexOf('await rendererSession.bindRendererContext()'),
    ).toBeLessThan(
      bootstrap.indexOf('await runDirectTuiPreTrustOnboarding'),
    )
  })
})
