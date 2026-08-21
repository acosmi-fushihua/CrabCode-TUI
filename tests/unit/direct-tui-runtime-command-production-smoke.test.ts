import { describe, expect, test } from 'bun:test'
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { readLastJsonEvidence } from '../helpers/readLastJsonEvidence.js'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const FIXTURE = join(
  REPO_ROOT,
  'tests/fixtures/direct-tui-runtime-command-production-smoke.ts',
)

async function runFixture<T>(
  mode: string,
  options: { initializedProject?: boolean } = {},
): Promise<T> {
  const root = mkdtempSync(join(tmpdir(), 'direct-tui-production-smoke-'))
  const config = join(root, 'config')
  const workspace = join(root, 'workspace')
  const output = join(root, 'evidence.json')
  const error = join(root, 'fixture.stderr')
  mkdirSync(config, { recursive: true })
  if (options.initializedProject) {
    mkdirSync(workspace, { recursive: true })
    writeFileSync(join(workspace, 'CRABCODE.md'), '# Fixture project\n')
  }
  try {
    const child = Bun.spawn({
      cmd: [process.execPath, FIXTURE],
      cwd: options.initializedProject ? workspace : REPO_ROOT,
      env: {
        ...process.env,
        ACOSMI_API_KEY: 'production-smoke-fixture',
        CRABCODE_CONFIG_DIR: config,
        CRABCODE_DISABLE_TELEMETRY: '1',
        CRABCODE_FEATURE_COORDINATOR_MODE: '0',
        DIRECT_TUI_PRODUCTION_SMOKE_MODE: mode,
        DISABLE_BACKGROUND_TASKS: '1',
        NODE_ENV: 'test',
        TERM_PROGRAM: 'crabcode-production-smoke-unsupported',
        USER_TYPE: 'external',
      },
      stdout: Bun.file(output),
      stderr: Bun.file(error),
    })
    const exitCode = await child.exited
    const stderr = readFileSync(error, 'utf8')
    expect(exitCode, stderr).toBe(0)
    return await readLastJsonEvidence<T>(output)
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
}

describe('direct TUI runtime production command smoke', () => {
  test('loads every audited production definition and its real lazy handler', async () => {
    const rows = await runFixture<
      Array<{ token: string; type: string; handler: string }>
    >('inventory')
    expect(rows).toHaveLength(19)
    expect(rows.map(row => row.token).sort()).toEqual([
      'advisor',
      'cost',
      'extra-usage',
      'files',
      'heapdump',
      'init',
      'insights',
      'install-slack-app',
      'local-models',
      'output-style',
      'pr-comments',
      'proxy',
      'release-notes',
      'review',
      'security-review',
      'smallmodel',
      'statusline',
      'terminal-setup',
      'vision',
    ])
    expect(rows.every(row => row.handler === 'function')).toBe(true)
  })

  test('executes bounded safe handlers and prompt builders without substituting them', async () => {
    const evidence = await runFixture<Record<string, unknown>>('safe')
    expect(Object.keys(evidence).sort()).toEqual([
      'advisor',
      'cost',
      'files',
      'local-models',
      'output-style',
      'pr-comments',
      'proxy',
      'review',
      'statusline',
      'vision',
    ])
    expect(JSON.stringify(evidence)).toContain('No files in context')
    expect(JSON.stringify(evidence)).toContain('invalid-smoke-argument')
    expect(JSON.stringify(evidence)).toContain('PR number: 42')
  })

  test('executes the real init builder across project-onboarding persistence success and failure', async () => {
    const evidence = await runFixture<{
      succeeded: {
        shouldQuery: boolean
        outcome?: unknown
        containsInitPrompt: boolean
      }
      failed: {
        shouldQuery: boolean
        outcome: { status: string; message: string }
        resultText: string
      }
      saveCalls: number
      writtenConfig: { hasCompletedProjectOnboarding?: boolean }
    }>('init-onboarding', { initializedProject: true })
    expect(evidence.succeeded).toEqual({
      shouldQuery: true,
      containsInitPrompt: true,
    })
    expect(evidence.writtenConfig.hasCompletedProjectOnboarding).toBe(true)
    expect(evidence.failed).toEqual({
      shouldQuery: false,
      outcome: {
        status: 'error',
        message: 'Error: fixture init onboarding persistence failed',
      },
      resultText: 'Error: fixture init onboarding persistence failed',
    })
    expect(evidence.saveCalls).toBe(2)
  })

  test('executes real side-effect handlers with only external authorities mocked', async () => {
    const evidence = await runFixture<{
      heapSuccess: { value: string }
      heapFailure: { value: string }
      releaseNotes: { value: string }
      localModelsSuccess: { value: string }
      localModelsFailure: { value: string }
      proxySuccess: { value: string }
      proxyFailure: { value: string }
      visionSuccess: { value: string }
      visionFailure: { value: string }
    }>('external')
    expect(evidence.heapSuccess.value).toContain('/fixture/heap.heapsnapshot')
    expect(evidence.heapFailure.value).toContain('fixture heap failure')
    expect(evidence.releaseNotes.value).toContain(
      'https://fixture.invalid/changelog',
    )
    expect(evidence.localModelsSuccess.value).toContain('Local models')
    expect(evidence.localModelsFailure.value).toContain(
      'fixture local-model authority failed',
    )
    expect(evidence.proxySuccess.value).toContain('已写入用户级设置')
    expect(evidence.proxyFailure.value).toContain('fixture proxy write failed')
    expect(evidence.visionSuccess.value).toContain('已授权视觉兜底')
    expect(evidence.visionFailure.value).toContain('fixture vision write failed')
    const installSlack = await runFixture<{
      result: { value: string }
      openedUrls: string[]
    }>('install-slack')
    expect(installSlack.result.value).toContain("Couldn't open browser")
    expect(installSlack.openedUrls).toEqual([
      'https://slack.com/marketplace/A08SF47R6P4-crabcode',
    ])

    const extraUsage = await runFixture<{
      result: { value: string }
      openedUrls: string[]
    }>('extra-usage')
    expect(extraUsage.result.value).toContain('Please visit')
    expect(extraUsage.openedUrls).toEqual([
      'https://acosmi.com/settings/usage',
    ])

    const terminalFailure = await runFixture<{
      status: string
      message: string
    }>('terminal-failure')
    expect(terminalFailure).toEqual({
      status: 'error',
      message: 'Error: fixture terminal setup failed',
    })
  })

  test('executes the real security-review builder across the shell authority seam', async () => {
    const evidence = await runFixture<{
      result: Array<{ type: string; text: string }>
      shellCalls: number
    }>('security')
    expect(evidence.shellCalls).toBe(1)
    expect(evidence.result).toEqual([
      expect.objectContaining({
        type: 'text',
        text: expect.stringContaining('security-shell-boundary:'),
      }),
    ])

    const failure = await runFixture<{
      outcome: { status: string; message: string }
      resultText: string
    }>('security-failure')
    expect(failure.outcome).toEqual({
      status: 'error',
      message: 'Error: fixture security review failed',
    })
    expect(failure.resultText).toContain('fixture security review failed')

    const cancellation = await runFixture<{
      outcome: { status: string; message: string }
      resultText: string
    }>('security-cancel')
    expect(cancellation.outcome).toEqual({
      status: 'cancelled',
      message: 'AbortError: fixture security review cancelled',
    })
    expect(cancellation.resultText).toContain(
      'fixture security review cancelled',
    )
  })

  test('keeps advisor AppState behind successful persistence and projects write failure as terminal error', async () => {
    const evidence = await runFixture<{
      failed: {
        outcome: { status: string; message: string }
        resultText: string
      }
      stateAfterFailure: string
      succeeded: { outcome?: unknown; resultText: string }
      stateAfterSuccess: string | null
      settingsCalls: number
    }>('advisor-persistence')
    expect(evidence.failed.outcome).toEqual({
      status: 'error',
      message: 'Error: Failed to disable advisor: fixture advisor persistence failed',
    })
    expect(evidence.failed.resultText).toContain(
      'Failed to disable advisor: fixture advisor persistence failed',
    )
    expect(evidence.stateAfterFailure).toBe('persisted-advisor')
    expect(evidence.succeeded.outcome).toBeUndefined()
    expect(evidence.succeeded.resultText).toBe(
      'Advisor disabled (was persisted-advisor).',
    )
    expect(evidence.stateAfterSuccess).toBeNull()
    expect(evidence.settingsCalls).toBe(2)
  })

  test('executes insights production phases and honors pre-phase and inter-phase cancellation', async () => {
    const success = await runFixture<{
      status: string
      generationCalls: number
      result: unknown
    }>('insights-success')
    expect(success.status).toBe('success')
    expect(success.generationCalls).toBe(1)
    expect(JSON.stringify(success.result)).toContain('production smoke')

    const preAbort = await runFixture<{
      status: string
      generationCalls: number
      message: string
    }>('insights-pre-abort')
    expect(preAbort).toMatchObject({
      status: 'AbortError',
      generationCalls: 0,
    })
    expect(preAbort.message).toContain('Insights generation cancelled')

    const phaseAbort = await runFixture<{
      status: string
      generationCalls: number
      message: string
    }>('insights-phase-abort')
    expect(phaseAbort).toMatchObject({
      status: 'AbortError',
      generationCalls: 1,
    })
    expect(phaseAbort.message).toContain('Insights generation cancelled')

    const failure = await runFixture<{
      status: string
      generationCalls: number
      message: string
    }>('insights-error')
    expect(failure).toMatchObject({
      status: 'Error',
      generationCalls: 1,
    })
    expect(failure.message).toContain('fixture insights generation failed')
  })
})
