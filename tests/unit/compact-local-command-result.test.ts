import { describe, expect, test } from 'bun:test'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import compactCommand from '../../src/commands/compact/index.js'
import { matchesCommandInvocation } from '../../src/types/command.js'

const REPO_ROOT = join(import.meta.dir, '..', '..')

type Envelope = {
  type: string
  data?: {
    type?: string
    phase?: string
    hookType?: string
  }
  toolUseID?: string
  parentToolUseID?: string
  subtype?: string
  is_error?: boolean
  result?: string
  errors?: string[]
  stop_reason?: string | null
  content?: string
  message?: { content?: string }
}

type ScenarioEvidence = {
  directEvents: Envelope[]
  envelopes: Envelope[]
}

function terminalResult(scenario: ScenarioEvidence): Envelope {
  const result = scenario.envelopes.findLast(
    envelope => envelope.type === 'result',
  )
  if (!result) throw new Error('fixture omitted terminal result')
  return result
}

function compactProgress(scenario: ScenarioEvidence): Envelope[] {
  return scenario.directEvents.filter(
    event =>
      event.type === 'progress' && event.data?.type === 'compact_progress',
  )
}

describe('compact local-command terminal result contract', () => {
  test('declares /com as an exact compact alias instead of a palette-only prefix', () => {
    expect(compactCommand.aliases).toEqual(['com'])
    expect(matchesCommandInvocation(compactCommand, 'com')).toBe(true)
    expect(matchesCommandInvocation(compactCommand, 'compact')).toBe(true)
  })

  test('success, empty history, API error, and cancellation keep distinct terminal truth', async () => {
    const auditRoot = mkdtempSync(join(tmpdir(), 'crabcode-compact-result-'))
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
        cmd: [
          process.execPath,
          join(
            REPO_ROOT,
            'tests/fixtures/compact-local-command-result.ts',
          ),
        ],
        cwd: REPO_ROOT,
        env: {
          ...process.env,
          HOME: homeDir,
          CRABCODE_CONFIG_DIR: configDir,
          CRABCODE_DISABLE_AUTO_MEMORY: '1',
          CRABCODE_DISABLE_TELEMETRY: '1',
          CRABCODE_FEATURE_COORDINATOR_MODE: '0',
          DISABLE_BACKGROUND_TASKS: '1',
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
      const evidence = JSON.parse(
        stdout.trim().split('\n').at(-1) ?? '',
      ) as Record<
        | 'success'
        | 'alias-success'
        | 'empty-history'
        | 'api-error'
        | 'cancelled',
        ScenarioEvidence
      >

      const success = terminalResult(evidence.success)
      expect(success).toMatchObject({
        type: 'result',
        subtype: 'success',
        is_error: false,
        stop_reason: null,
      })
      expect(
        evidence.success.directEvents.filter(
          event =>
            event.type === 'system' && event.subtype === 'compact_boundary',
        ),
      ).toHaveLength(1)
      expect(
        evidence.success.directEvents.some(
          event =>
            event.type === 'user' &&
            event.message?.content?.includes(
              '<local-command-stdout>压缩完成；原文已保存。</local-command-stdout>',
            ),
        ),
      ).toBe(true)

      const successProgress = compactProgress(evidence.success)
      expect(
        successProgress.map(event => ({
          phase: event.data?.phase,
          hookType: event.data?.hookType,
        })),
      ).toEqual([
        { phase: 'hooks_start', hookType: 'pre_compact' },
        { phase: 'compact_start', hookType: undefined },
        { phase: 'compact_end', hookType: undefined },
      ])
      expect(new Set(successProgress.map(event => event.toolUseID)).size).toBe(1)
      expect(
        successProgress.every(
          event =>
            event.toolUseID?.startsWith('compact-progress-') &&
            event.parentToolUseID === event.toolUseID,
        ),
      ).toBe(true)
      const boundaryIndex = evidence.success.directEvents.findIndex(
        event => event.type === 'system' && event.subtype === 'compact_boundary',
      )
      const progressEndIndex = evidence.success.directEvents.findIndex(
        event =>
          event.type === 'progress' &&
          event.data?.type === 'compact_progress' &&
          event.data.phase === 'compact_end',
      )
      expect(progressEndIndex).toBeGreaterThanOrEqual(0)
      expect(boundaryIndex).toBeGreaterThan(progressEndIndex)

      const aliasSuccess = terminalResult(evidence['alias-success'])
      expect(aliasSuccess).toMatchObject({
        type: 'result',
        subtype: 'success',
        is_error: false,
        stop_reason: null,
      })
      expect(
        evidence['alias-success'].directEvents.filter(
          event =>
            event.type === 'system' && event.subtype === 'compact_boundary',
        ),
      ).toHaveLength(1)

      expect(compactProgress(evidence['empty-history'])).toHaveLength(0)

      for (const [name, expected] of [
        ['empty-history', '没有可压缩的消息'],
        ['api-error', 'fixture API unavailable'],
      ] as const) {
        const scenario = evidence[name]
        const result = terminalResult(scenario)
        expect(result).toMatchObject({
          type: 'result',
          subtype: 'error_during_execution',
          is_error: true,
          stop_reason: null,
        })
        expect(result.errors?.join('\n')).toContain(expected)
        expect(
          scenario.envelopes.some(
            envelope =>
              envelope.type === 'result' && envelope.subtype === 'success',
          ),
        ).toBe(false)
        expect(
          scenario.directEvents.some(
            event =>
              event.type === 'system' &&
              event.subtype === 'local_command' &&
              event.content?.includes('<local-command-stderr>') &&
              event.content.includes(expected),
          ),
        ).toBe(true)
        if (name === 'api-error') {
          expect(
            compactProgress(scenario).map(event => event.data?.phase),
          ).toEqual(['hooks_start', 'compact_start', 'compact_end'])
        }
      }

      const cancelled = terminalResult(evidence.cancelled)
      expect(cancelled).toMatchObject({
        type: 'result',
        subtype: 'error_during_execution',
        is_error: true,
        stop_reason: 'interrupted',
      })
      expect(cancelled.errors?.join('\n')).toContain('压缩已取消')
      expect(
        evidence.cancelled.envelopes.some(
          envelope =>
            envelope.type === 'result' && envelope.subtype === 'success',
        ),
      ).toBe(false)
      expect(
        evidence.cancelled.directEvents.some(
          event =>
            event.type === 'system' &&
            event.subtype === 'local_command' &&
            event.content?.includes(
              '<local-command-stderr>Error: 压缩已取消</local-command-stderr>',
            ),
        ),
      ).toBe(true)
      expect(
        compactProgress(evidence.cancelled).map(event => event.data?.phase),
      ).toEqual(['hooks_start', 'compact_start', 'compact_end'])
    } finally {
      rmSync(auditRoot, { recursive: true, force: true })
    }
  })
})
