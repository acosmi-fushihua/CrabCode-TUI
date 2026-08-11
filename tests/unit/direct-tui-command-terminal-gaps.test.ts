import {
  afterAll,
  describe,
  expect,
  setDefaultTimeout,
  test,
} from 'bun:test'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const FIXTURE = join(
  REPO_ROOT,
  'tests',
  'fixtures',
  'direct-tui-command-terminal-gaps.ts',
)

setDefaultTimeout(60_000)

type Envelope = {
  type?: string
  subtype?: string
  session_id?: string
  uuid?: string
  content?: string
  message?: { content?: unknown; role?: string }
  is_error?: boolean
  stop_reason?: string | null
  errors?: string[]
  result?: string
}

type Scenario = {
  scenario: string
  invocation?: string
  oldSessionId?: string
  newSessionId?: string
  sessionId?: string
  seedUuid?: string
  transcriptPath?: string
  oldTranscriptPath?: string
  newTranscriptPath?: string
  oldRaw?: string
  oldRawAfter?: string
  newRaw?: string
  initialRaw?: string
  finalRaw?: string
  directEvents: Envelope[]
  envelopes: Envelope[]
  messagesAfter: Envelope[]
}

type Matrix = {
  compact: {
    success: Scenario
    failure: Scenario
    cancellation: Scenario
  }
  clear: Array<{
    invocation: string
    success: Scenario
    failure: Scenario
    cancellation: Scenario
  }>
  clearInFlight: Scenario & { hookAbortObserved: boolean }
}

type Recovery = {
  recovered: Array<{
    kind: 'compact-history' | 'clear'
    path: string
    expectedSessionId: string
    sessionIds: string[]
    messageCount: number
    messageTypes: string[]
    contents: unknown[]
  }>
  compactHistoryReplay?: {
    directEvents: Envelope[]
    envelopes: Envelope[]
    messagesAfter: Envelope[]
  }
}

function terminalResult(scenario: Pick<Scenario, 'envelopes'>): Envelope {
  const result = scenario.envelopes.findLast(
    envelope => envelope.type === 'result',
  )
  if (!result) throw new Error('fixture omitted terminal result')
  return result
}

function localCommandEvent(
  scenario: Pick<Scenario, 'directEvents'>,
): Envelope {
  const event = scenario.directEvents.find(
    candidate =>
      candidate.type === 'system' && candidate.subtype === 'local_command',
  )
  if (!event) throw new Error('fixture omitted local-command render event')
  return event
}

function assistantText(scenario: Pick<Scenario, 'envelopes'>): string {
  return JSON.stringify(
    scenario.envelopes
      .filter(envelope => envelope.type === 'assistant')
      .map(envelope => envelope.message?.content),
  )
}

function parseLastJson(stdout: string): unknown {
  return JSON.parse(stdout.trim().split('\n').at(-1) ?? '')
}

async function spawnFixture(options: {
  auditRoot: string
  workspace: string
  configDir: string
  homeDir: string
  mode: 'matrix' | 'recover'
  recovery?: unknown
}): Promise<unknown> {
  const child = Bun.spawn({
    cmd: [process.execPath, FIXTURE],
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      HOME: options.homeDir,
      CRABCODE_CONFIG_DIR: options.configDir,
      CRABCODE_DISABLE_AUTO_MEMORY: '1',
      CRABCODE_DISABLE_TELEMETRY: '1',
      CRABCODE_FEATURE_COORDINATOR_MODE: '0',
      DISABLE_BACKGROUND_TASKS: '1',
      TEST_ENABLE_SESSION_PERSISTENCE: '1',
      TERMINAL_GAPS_MODE: options.mode,
      TERMINAL_GAPS_WORKSPACE: options.workspace,
      TERMINAL_GAPS_CONFIG_DIR: options.configDir,
      ...(options.recovery === undefined
        ? {}
        : { TERMINAL_GAPS_RECOVERY: JSON.stringify(options.recovery) }),
    },
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ])
  expect(exitCode, stderr).toBe(0)
  return parseLastJson(stdout)
}

describe('direct TUI compact-history and clear alias terminal truth', () => {
  const auditRoot = mkdtempSync(join(tmpdir(), 'crabcode-command-gaps-'))
  const configDir = join(auditRoot, 'config')
  const homeDir = join(auditRoot, 'home')
  const workspace = join(auditRoot, 'workspace')
  mkdirSync(configDir, { recursive: true })
  mkdirSync(homeDir, { recursive: true })
  mkdirSync(workspace, { recursive: true })
  writeFileSync(
    join(configDir, '.crabcode.json'),
    JSON.stringify({ theme: 'dark', hasCompletedOnboarding: true }),
  )
  writeFileSync(
    join(configDir, 'settings.json'),
    JSON.stringify({
      autoMemoryEnabled: false,
      disableAllHooks: true,
    }),
  )

  let matrixPromise: Promise<Matrix> | undefined
  const matrix = (): Promise<Matrix> => {
    matrixPromise ??= spawnFixture({
      auditRoot,
      workspace,
      configDir,
      homeDir,
      mode: 'matrix',
    }) as Promise<Matrix>
    return matrixPromise
  }

  test('compact-history QueryEngine preserves malformed JSONL, renders one valid boundary, and resumes it in a fresh process', async () => {
    const evidence = await matrix()
    const success = evidence.compact.success
    const terminal = terminalResult(success)
    expect(terminal).toMatchObject({
      type: 'result',
      subtype: 'success',
      is_error: false,
      stop_reason: null,
      session_id: success.sessionId,
    })
    expect(terminal.result).toContain('12.3k')
    expect(terminal.result).toContain('7')
    expect(terminal.result).toContain(success.transcriptPath)

    const rendered = localCommandEvent(success)
    expect(rendered.content).toContain('<local-command-stdout>')
    expect(rendered.content).toContain('12.3k')
    expect(rendered.content).toContain('7')
    expect(rendered.content).not.toContain('<local-command-stderr>')
    expect(assistantText(success)).toContain('12.3k')

    expect(success.finalRaw).toStartWith(success.initialRaw ?? '')
    expect(success.finalRaw).toContain('MALFORMED-SENTINEL')
    expect(success.finalRaw).toContain('COMPACT-HISTORY-ORIGINAL-SENTINEL')
    expect(success.finalRaw).toContain('<local-command-stdout>')

    const recovery = (await spawnFixture({
      auditRoot,
      workspace,
      configDir,
      homeDir,
      mode: 'recover',
      recovery: [
        {
          kind: 'compact-history',
          path: success.transcriptPath,
          sessionId: success.sessionId,
        },
      ],
    })) as Recovery
    expect(recovery.recovered).toHaveLength(1)
    expect(recovery.recovered[0]).toMatchObject({
      kind: 'compact-history',
      expectedSessionId: success.sessionId,
      sessionIds: [success.sessionId],
    })
    expect(recovery.recovered[0]?.messageTypes).toContain(
      'system:compact_boundary',
    )
    // Resume deliberately excludes local-command output from model context;
    // the raw JSONL assertion above proves durability, while the fresh
    // /compact-history replay below proves the boundary remains discoverable.
    expect(recovery.recovered[0]?.messageTypes).toContain('user')
    expect(JSON.stringify(recovery.recovered[0]?.contents)).toContain(
      '<command-name>/compact-history</command-name>',
    )
    if (!recovery.compactHistoryReplay) {
      throw new Error('fresh recovery omitted compact-history replay')
    }
    expect(terminalResult(recovery.compactHistoryReplay)).toMatchObject({
      type: 'result',
      subtype: 'success',
      is_error: false,
      session_id: success.sessionId,
    })
    expect(localCommandEvent(recovery.compactHistoryReplay).content).toContain(
      '12.3k',
    )
  })

  test('compact-history physical I/O failure and cancellation produce distinct stderr terminals', async () => {
    const evidence = await matrix()
    const failure = evidence.compact.failure
    expect(terminalResult(failure)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: null,
      session_id: failure.oldSessionId,
    })
    expect(terminalResult(failure).errors?.join('\n')).toContain(
      'Unable to read compact-history transcript (EISDIR)',
    )
    expect(localCommandEvent(failure).content).toContain(
      '<local-command-stderr>Error: Unable to read compact-history transcript (EISDIR)',
    )
    expect(assistantText(failure)).toContain(
      'Unable to read compact-history transcript (EISDIR)',
    )
    expect(failure.newSessionId).toBe(failure.oldSessionId)
    expect(
      failure.messagesAfter.some(message => message.uuid === failure.seedUuid),
    ).toBe(true)

    const cancellation = evidence.compact.cancellation
    expect(terminalResult(cancellation)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: 'interrupted',
      session_id: cancellation.oldSessionId,
    })
    expect(terminalResult(cancellation).errors?.join('\n')).toContain(
      'AbortError',
    )
    expect(localCommandEvent(cancellation).content).toContain(
      '<local-command-stderr>AbortError',
    )
    expect(assistantText(cancellation)).toContain('AbortError')
    expect(cancellation.newSessionId).toBe(cancellation.oldSessionId)
    expect(
      cancellation.messagesAfter.some(
        message => message.uuid === cancellation.seedUuid,
      ),
    ).toBe(true)
  })

  test('clear/new/reset QueryEngine success commits durable new sessions recoverable in a fresh process', async () => {
    const evidence = await matrix()
    const recoveryItems: Array<{
      kind: 'clear'
      path: string
      sessionId: string
    }> = []

    for (const alias of evidence.clear) {
      const success = alias.success
      expect(success.newSessionId).not.toBe(success.oldSessionId)
      expect(success.newTranscriptPath).not.toBe(success.oldTranscriptPath)
      expect(terminalResult(success)).toMatchObject({
        type: 'result',
        subtype: 'success',
        is_error: false,
        stop_reason: null,
        session_id: success.newSessionId,
      })
      expect(
        success.envelopes
          .filter(envelope => envelope.session_id !== undefined)
          .every(
            envelope => envelope.session_id === success.newSessionId,
          ),
      ).toBe(true)
      expect(localCommandEvent(success).content).toContain(
        '<local-command-stdout></local-command-stdout>',
      )
      expect(
        success.envelopes.some(envelope => envelope.type === 'assistant'),
      ).toBe(true)
      expect(success.oldRawAfter).toBe(success.oldRaw)
      expect(success.newRaw).toContain(success.newSessionId)
      expect(success.newRaw).toContain(
        '<local-command-stdout></local-command-stdout>',
      )
      expect(
        success.messagesAfter.some(
          message => message.type === 'system' && message.subtype === 'local_command',
        ),
      ).toBe(true)
      recoveryItems.push({
        kind: 'clear',
        path: success.newTranscriptPath!,
        sessionId: success.newSessionId!,
      })
    }

    expect(evidence.clear.map(item => item.invocation)).toEqual([
      '/clear',
      '/new',
      '/reset',
    ])
    const recovery = (await spawnFixture({
      auditRoot,
      workspace,
      configDir,
      homeDir,
      mode: 'recover',
      recovery: recoveryItems,
    })) as Recovery
    expect(recovery.recovered).toHaveLength(3)
    for (const item of recovery.recovered) {
      expect(item.kind).toBe('clear')
      expect(item.sessionIds).toEqual([item.expectedSessionId])
      expect(item.messageCount).toBeGreaterThanOrEqual(2)
      expect(item.messageTypes).toContain('user')
      expect(JSON.stringify(item.contents)).toContain(
        '<command-name>/clear</command-name>',
      )
    }
  })

  test('clear/new/reset preflight failure and cancellation preserve the old session and render error terminals', async () => {
    const evidence = await matrix()
    for (const alias of evidence.clear) {
      const failure = alias.failure
      expect(failure.newSessionId).toBe(failure.oldSessionId)
      expect(terminalResult(failure)).toMatchObject({
        type: 'result',
        subtype: 'error_during_execution',
        is_error: true,
        stop_reason: null,
        session_id: failure.oldSessionId,
      })
      expect(terminalResult(failure).errors?.join('\n')).toContain(
        'does not exist',
      )
      expect(localCommandEvent(failure).content).toContain(
        '<local-command-stderr>Error: Path',
      )
      expect(assistantText(failure)).toContain('does not exist')
      expect(
        failure.messagesAfter.some(message => message.uuid === failure.seedUuid),
      ).toBe(true)

      const cancellation = alias.cancellation
      expect(cancellation.newSessionId).toBe(cancellation.oldSessionId)
      expect(terminalResult(cancellation)).toMatchObject({
        type: 'result',
        subtype: 'error_during_execution',
        is_error: true,
        stop_reason: 'interrupted',
        session_id: cancellation.oldSessionId,
      })
      expect(terminalResult(cancellation).errors?.join('\n')).toContain(
        'AbortError',
      )
      expect(localCommandEvent(cancellation).content).toContain(
        '<local-command-stderr>AbortError',
      )
      expect(assistantText(cancellation)).toContain('AbortError')
      expect(
        cancellation.messagesAfter.some(
          message => message.uuid === cancellation.seedUuid,
        ),
      ).toBe(true)
    }

    const inFlight = evidence.clearInFlight
    expect(inFlight.hookAbortObserved).toBe(true)
    expect(inFlight.newSessionId).toBe(inFlight.oldSessionId)
    expect(terminalResult(inFlight)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: 'interrupted',
      session_id: inFlight.oldSessionId,
    })
    expect(terminalResult(inFlight).errors?.join('\n')).toContain(
      'fixture in-flight clear abort',
    )
    expect(localCommandEvent(inFlight).content).toContain(
      '<local-command-stderr>AbortError: fixture in-flight clear abort',
    )
    expect(
      inFlight.messagesAfter.some(
        message => message.uuid === inFlight.seedUuid,
      ),
    ).toBe(true)
  })

  afterAll(() => {
    rmSync(auditRoot, { recursive: true, force: true })
  })
})
