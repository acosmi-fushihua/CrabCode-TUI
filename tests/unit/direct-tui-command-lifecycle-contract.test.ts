import { describe, expect, setDefaultTimeout, test } from 'bun:test'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const VERIFIER = join(
  REPO_ROOT,
  'scripts',
  'verify-direct-tui-command-capabilities.mjs',
)

// The strict case deliberately executes the hermetic Rust and Bun evidence
// suites. It is a release gate, not a source-only manifest assertion.
setDefaultTimeout(180_000)

type CommandCoverage = {
  token: string
  profile: string
  ownerClass: 'rendererLocal' | 'runtimeCatalog' | 'failClosed'
  discovery:
    | 'visible'
    | 'hidden'
    | 'dynamic_hidden'
    | 'unadvertised_fail_closed'
  coverage: Record<string, string>
}

type AuditReport = {
  reference: number
  runtimeCatalog: number
  nonReferenceKnown: number
  referenceStatus: 'live_verified' | 'snapshot_only'
  lifecycle: {
    knownTokens: number
    profiles: number
    dimensions: number
    coverageCells: number
    coverageCounts: Record<string, number>
    incompleteProfiles: Array<{
      id: string
      tokens: number
      dimensions: string[]
    }>
    unverifiedProfiles: Array<{
      id: string
      tokens: number
      dimensions: string[]
    }>
    commands: CommandCoverage[]
  }
  status: 'verified' | 'incomplete' | 'unverified'
}

type VerifierResult = Omit<
  ReturnType<typeof Bun.spawnSync>,
  'stdout' | 'stderr'
> & {
  stdout: Buffer
  stderr: Buffer
}

function runVerifier(...args: string[]): VerifierResult {
  return runVerifierWithEnv({}, ...args)
}

function runVerifierWithEnv(
  environment: Record<string, string>,
  ...args: string[]
): VerifierResult {
  const childEnvironment = {
    ...process.env,
  }
  delete childEnvironment.CRABCODE_COMMAND_REFERENCE_REQUIRED
  delete childEnvironment.CRABCODE_COMMAND_REFERENCE_TEST_REPOSITORY
  Object.assign(childEnvironment, environment)

  const outputRoot = mkdtempSync(join(tmpdir(), 'crabcode-verifier-output-'))
  const stdoutPath = join(outputRoot, 'stdout')
  const stderrPath = join(outputRoot, 'stderr')
  try {
    const result = Bun.spawnSync({
      cmd: [process.execPath, VERIFIER, ...args],
      cwd: REPO_ROOT,
      env: childEnvironment,
      stdout: Bun.file(stdoutPath),
      stderr: Bun.file(stderrPath),
    })
    return {
      ...result,
      stdout: readFileSync(stdoutPath),
      stderr: readFileSync(stderrPath),
    }
  } finally {
    rmSync(outputRoot, { recursive: true, force: true })
  }
}

describe('direct TUI command full-lifecycle capability contract', () => {
  test('keeps optional reference checks hermetic and explicit live checks fail-closed', () => {
    const testReferenceEnvironment = {
      NODE_ENV: 'test',
      CRABCODE_COMMAND_REFERENCE_TEST_REPOSITORY: REPO_ROOT,
    }
    const ordinary = runVerifierWithEnv(
      testReferenceEnvironment,
      '--report',
    )
    expect(ordinary.exitCode, ordinary.stderr.toString()).toBe(0)
    expect(JSON.parse(ordinary.stdout.toString()).referenceStatus).toBe(
      'snapshot_only',
    )

    const required = runVerifierWithEnv(
      {
        ...testReferenceEnvironment,
        CRABCODE_COMMAND_REFERENCE_REQUIRED: '1',
      },
      '--report',
    )
    expect(required.exitCode).not.toBe(0)
    expect(required.stderr.toString()).toContain(
      'required pinned reference source is unavailable',
    )
  })

  test('expands every reference and extension token to one owner and nine lifecycle statuses', () => {
    const result = runVerifier('--report')
    expect(result.exitCode, result.stderr.toString()).toBe(0)

    const report = JSON.parse(result.stdout.toString()) as AuditReport
    expect(report).toMatchObject({
      reference: 96,
      runtimeCatalog: 24,
      nonReferenceKnown: 8,
      status: 'verified',
      lifecycle: {
        knownTokens: 104,
        profiles: 21,
        dimensions: 9,
        coverageCells: 936,
      },
    })
    expect(['live_verified', 'snapshot_only']).toContain(
      report.referenceStatus,
    )
    expect(report.lifecycle.commands).toHaveLength(104)
    expect(report.lifecycle.coverageCounts.unverified).toBe(0)
    expect(report.lifecycle.coverageCounts.shared_path_only).toBe(0)
    expect(report.lifecycle.incompleteProfiles).toEqual([])
    expect(report.lifecycle.unverifiedProfiles).toEqual([])
    expect(new Set(report.lifecycle.commands.map(command => command.token)).size).toBe(
      104,
    )
    expect(
      report.lifecycle.commands.every(
        command => Object.keys(command.coverage).length === 9,
      ),
    ).toBe(true)

    const byToken = new Map(
      report.lifecycle.commands.map(command => [command.token, command]),
    )
    expect(byToken.get('com')).toMatchObject({
      profile: 'runtime_compact',
      ownerClass: 'runtimeCatalog',
      discovery: 'visible',
      coverage: {
        success: 'verified',
        failure: 'verified',
        cancellation: 'verified',
        terminalState: 'verified',
        rendering: 'verified',
        persistenceRecovery: 'verified',
      },
    })
    expect(byToken.get('heapdump')).toMatchObject({
      ownerClass: 'runtimeCatalog',
      discovery: 'hidden',
    })
    expect(byToken.get('compact-history')).toMatchObject({
      profile: 'runtime_compact_history',
      coverage: {
        failure: 'verified',
        cancellation: 'verified',
      },
    })
    expect(byToken.get('clear')).toMatchObject({
      profile: 'runtime_clear_aliases',
      coverage: {
        failure: 'verified',
        cancellation: 'verified',
      },
    })
    expect(byToken.get('advisor')).toMatchObject({
      ownerClass: 'runtimeCatalog',
      discovery: 'dynamic_hidden',
    })
    expect(byToken.get('voice')).toMatchObject({
      ownerClass: 'failClosed',
      discovery: 'unadvertised_fail_closed',
      coverage: {
        failure: 'verified',
        terminalState: 'verified',
        rendering: 'verified',
      },
    })
    expect(byToken.get('logout')).toMatchObject({
      ownerClass: 'rendererLocal',
      discovery: 'visible',
    })

    expect(
      Object.values(report.lifecycle.coverageCounts).reduce(
        (total, count) => total + count,
        0,
      ),
    ).toBe(report.lifecycle.coverageCells)
  })

  test('ordinary gate rejects unverified cells and never labels shared-only coverage verified', () => {
    const rejected = runVerifier('--test-inject-unverified')
    expect(rejected.exitCode).not.toBe(0)
    expect(rejected.stderr.toString()).toContain(
      'verifier_fail_closed_self_test[failure]',
    )

    const diagnostic = runVerifier(
      '--report',
      '--test-inject-unverified',
    )
    expect(diagnostic.exitCode, diagnostic.stderr.toString()).toBe(0)
    const diagnosticReport = JSON.parse(
      diagnostic.stdout.toString(),
    ) as AuditReport
    expect(diagnosticReport.status).toBe('unverified')
    expect(diagnosticReport.lifecycle.unverifiedProfiles).toContainEqual({
      id: 'verifier_fail_closed_self_test',
      tokens: 0,
      dimensions: ['failure'],
    })

    const result = runVerifier()
    expect(result.exitCode, result.stderr.toString()).toBe(0)
    const report = JSON.parse(result.stdout.toString()) as AuditReport
    expect(report.status).toBe('verified')
    expect(report.lifecycle.coverageCounts.unverified).toBe(0)
    expect(report.lifecycle.coverageCounts.shared_path_only).toBe(0)
    expect(report.lifecycle.incompleteProfiles).toEqual([])
  })

  test('executes every referenced hermetic evidence suite before the strict gate passes', () => {
    const rejected = runVerifier(
      '--require-complete',
      '--test-inject-missing-rust-member',
    )
    expect(rejected.exitCode).not.toBe(0)
    expect(rejected.stderr.toString()).toContain(
      'does not execute referenced marker',
    )

    const missingAggregate = runVerifier(
      '--require-complete',
      '--test-inject-missing-rust-aggregate-test',
    )
    expect(missingAggregate.exitCode).not.toBe(0)
    expect(missingAggregate.stderr.toString()).toContain(
      'did not execute exactly once',
    )

    const missingTerminal = runVerifier(
      '--require-complete',
      '--test-inject-missing-terminal-test',
    )
    expect(missingTerminal.exitCode).not.toBe(0)
    expect(missingTerminal.stderr.toString()).toContain(
      'did not execute exactly once',
    )

    const missingBunTest = runVerifier(
      '--require-complete',
      '--test-inject-missing-bun-test',
    )
    expect(missingBunTest.exitCode).not.toBe(0)
    expect(missingBunTest.stderr.toString()).toContain(
      'did not execute exactly once as a passing test',
    )

    const skippedBunTest = runVerifier(
      '--require-complete',
      '--test-inject-skipped-bun-result',
    )
    expect(skippedBunTest.exitCode).not.toBe(0)
    expect(skippedBunTest.stderr.toString()).toContain(
      'passes=0 skipped=true',
    )

    const result = runVerifier('--require-complete')
    expect(result.exitCode, result.stderr.toString()).toBe(0)
    const report = JSON.parse(result.stdout.toString()) as AuditReport
    expect(report.status).toBe('verified')
    expect(report.lifecycle.coverageCounts).toMatchObject({
      shared_path_only: 0,
      unverified: 0,
    })
    expect(report.lifecycle.incompleteProfiles).toEqual([])
  })
})
