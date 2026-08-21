import { describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { readLastJsonEvidence } from '../helpers/readLastJsonEvidence.js'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const FIXTURE = join(
  REPO_ROOT,
  'tests',
  'fixtures',
  'session-memory-compact-progress.ts',
)

type Evidence = {
  outcome: 'success' | 'fallback' | 'error'
  phases: string[]
  statuses: Array<string | null>
}

async function runScenario(
  root: string,
  scenario: Evidence['outcome'],
): Promise<Evidence> {
  const configDir = join(root, scenario)
  const outputPath = join(configDir, 'evidence.json')
  const errorPath = join(configDir, 'fixture.stderr')
  await mkdir(configDir, { recursive: true })
  await Promise.all([
    writeFile(
      join(configDir, '.crabcode.json'),
      JSON.stringify({ theme: 'dark', hasCompletedOnboarding: true }),
    ),
    writeFile(
      join(configDir, 'settings.json'),
      JSON.stringify({ autoMemoryEnabled: false, disableAllHooks: true }),
    ),
  ])

  const child = Bun.spawn({
    cmd: [process.execPath, FIXTURE],
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      COMPACT_SM_CONFIG_DIR: configDir,
      COMPACT_SM_PROGRESS_SCENARIO: scenario,
      CRABCODE_DISABLE_TELEMETRY: '1',
      CRABCODE_FEATURE_COORDINATOR_MODE: '0',
      DISABLE_BACKGROUND_TASKS: '1',
    },
    stdout: Bun.file(outputPath),
    stderr: Bun.file(errorPath),
  })
  const exitCode = await child.exited
  const stderr = await Bun.file(errorPath).text()
  expect(exitCode, stderr).toBe(0)
  return readLastJsonEvidence<Evidence>(outputPath)
}

describe('/compact session-memory progress', () => {
  test('success, fallback, and internal-error fallback share one closed lifecycle', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-sm-progress-'))
    try {
      const [success, fallback, error] = await Promise.all([
        runScenario(root, 'success'),
        runScenario(root, 'fallback'),
        runScenario(root, 'error'),
      ])

      expect(success).toEqual({
        outcome: 'success',
        phases: [
          'compact_start',
          'hooks_start:session_start',
          'compact_end',
        ],
        statuses: ['compacting', null],
      })
      expect(fallback).toEqual({
        outcome: 'fallback',
        phases: [
          'compact_start',
          'hooks_start:pre_compact',
          'hooks_start:session_start',
          'hooks_start:post_compact',
          'compact_end',
        ],
        statuses: ['compacting', null],
      })
      expect(error).toEqual({
        outcome: 'error',
        phases: [
          'compact_start',
          'hooks_start:pre_compact',
          'compact_end',
        ],
        statuses: ['compacting', null],
      })

      for (const evidence of [success, fallback, error]) {
        expect(
          evidence.phases.filter(phase => phase === 'compact_start'),
        ).toHaveLength(1)
        expect(
          evidence.phases.filter(phase => phase === 'compact_end'),
        ).toHaveLength(1)
        expect(evidence.phases.at(-1)).toBe('compact_end')
      }
    } finally {
      await rm(root, { recursive: true, force: true })
    }
  })
})
