import { describe, expect, setDefaultTimeout, test } from 'bun:test'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const FIXTURE = join(
  REPO_ROOT,
  'tests/fixtures/direct-tui-compact-history-query-engine.ts',
)

setDefaultTimeout(30_000)

type FixtureEvidence = {
  mode: string
  readCount: number
  envelopes: Array<{
    type: string
    subtype?: string
    is_error?: boolean
    stop_reason?: string | null
    errors?: string[]
  }>
}

async function runFixture(mode: string): Promise<FixtureEvidence> {
  const auditRoot = mkdtempSync(join(tmpdir(), 'crabcode-compact-history-'))
  const configDir = join(auditRoot, 'config')
  const homeDir = join(auditRoot, 'home')
  mkdirSync(configDir, { recursive: true })
  mkdirSync(homeDir, { recursive: true })
  writeFileSync(
    join(configDir, '.crabcode.json'),
    JSON.stringify({ theme: 'dark', hasCompletedOnboarding: true }),
  )
  writeFileSync(
    join(configDir, 'settings.json'),
    JSON.stringify({ autoMemoryEnabled: false, disableAllHooks: true }),
  )

  try {
    const child = Bun.spawn({
      cmd: [process.execPath, FIXTURE],
      cwd: REPO_ROOT,
      env: {
        ...process.env,
        HOME: homeDir,
        CRABCODE_CONFIG_DIR: configDir,
        CRABCODE_DISABLE_AUTO_MEMORY: '1',
        CRABCODE_DISABLE_TELEMETRY: '1',
        CRABCODE_FEATURE_COORDINATOR_MODE: '0',
        DISABLE_BACKGROUND_TASKS: '1',
        DIRECT_TUI_COMPACT_HISTORY_MODE: mode,
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
    return JSON.parse(
      stdout.trim().split('\n').at(-1) ?? '',
    ) as FixtureEvidence
  } finally {
    rmSync(auditRoot, { recursive: true, force: true })
  }
}

function terminalResult(evidence: FixtureEvidence) {
  return evidence.envelopes.at(-1)
}

describe('direct TUI compact-history terminal lifecycle', () => {
  test('projects unreadable transcript failure through the real catalog and QueryEngine', async () => {
    const evidence = await runFixture('failure')
    expect(evidence.readCount).toBe(1)
    expect(terminalResult(evidence)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: null,
    })
    expect(terminalResult(evidence)?.errors?.[0]).toContain(
      'Unable to read compact-history transcript (EACCES)',
    )
  })

  test('projects pre-read cancellation as interrupted without touching disk', async () => {
    const evidence = await runFixture('pre-abort')
    expect(evidence.readCount).toBe(0)
    expect(terminalResult(evidence)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: 'interrupted',
    })
  })

  test('projects in-flight read cancellation as interrupted', async () => {
    const evidence = await runFixture('read-abort')
    expect(evidence.readCount).toBe(1)
    expect(terminalResult(evidence)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: 'interrupted',
    })
  })

  test('projects the post-read abort race as interrupted before synchronous scanning', async () => {
    const evidence = await runFixture('post-read-abort')
    expect(evidence.readCount).toBe(1)
    expect(terminalResult(evidence)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: 'interrupted',
    })
  })
})
