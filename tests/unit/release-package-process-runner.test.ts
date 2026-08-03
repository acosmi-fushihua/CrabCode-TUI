import { afterEach, describe, expect, test } from 'bun:test'
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { runProcess } from '../../scripts/release-package-smoke.mjs'

const fixture = resolve(
  import.meta.dir,
  '../fixtures/inherited-stdio-process.ts',
)
const roots: string[] = []
const descendants = new Set<number>()

function scratch() {
  const root = mkdtempSync(join(tmpdir(), 'crabcode-process-runner-test-'))
  roots.push(root)
  return root
}

function rememberDescendant(pidFile: string) {
  if (!existsSync(pidFile)) return null
  const pid = Number(readFileSync(pidFile, 'utf8'))
  if (Number.isSafeInteger(pid)) descendants.add(pid)
  return pid
}

afterEach(() => {
  for (const pid of descendants) {
    try {
      process.kill(pid, 'SIGKILL')
    } catch {
      // The fixture descendant has already exited.
    }
  }
  descendants.clear()
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true })
  }
})

describe('release package bounded process runner', () => {
  test('accepts a complete contract when a successful descendant keeps stdio open', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const started = Date.now()
    const result = await runProcess(
      process.execPath,
      [fixture, 'exit', pidFile],
      {
        timeoutMs: 2_000,
        streamDrainTimeoutMs: 100,
        allowInheritedPipeHandles: true,
        requiredStdoutSuffix: '\n',
      },
    )
    const descendantPid = rememberDescendant(pidFile)

    expect(Date.now() - started).toBeLessThan(1_500)
    expect(result.streamsClosed).toBe(false)
    expect(JSON.parse(result.stdout)).toEqual({ descendantPid })
    expect(result.stderr).toBe('')
  })

  test('fails closed when inherited pipe handles are not authorized', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const started = Date.now()
    const outcome = runProcess(
      process.execPath,
      [fixture, 'exit', pidFile],
      {
        timeoutMs: 2_000,
        streamDrainTimeoutMs: 100,
      },
    )

    await expect(outcome).rejects.toThrow('descendant processes kept stdio open')
    rememberDescendant(pidFile)
    expect(Date.now() - started).toBeLessThan(1_500)
  })

  test('rejects truncated output even when inherited pipe handles are authorized', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const outcome = runProcess(
      process.execPath,
      [fixture, 'partial', pidFile],
      {
        timeoutMs: 2_000,
        streamDrainTimeoutMs: 100,
        allowInheritedPipeHandles: true,
        requiredStdoutSuffix: '\n',
      },
    )

    await expect(outcome).rejects.toThrow(
      'left stdio open before stdout completed its contract',
    )
    rememberDescendant(pidFile)
  })

  test('rejects stderr even when inherited pipe handles are authorized', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const outcome = runProcess(
      process.execPath,
      [fixture, 'stderr', pidFile],
      {
        timeoutMs: 2_000,
        streamDrainTimeoutMs: 100,
        allowInheritedPipeHandles: true,
        requiredStdoutSuffix: '\n',
      },
    )

    await expect(outcome).rejects.toThrow('left stdio open with stderr')
    rememberDescendant(pidFile)
  })

  test('bounds timeout cleanup even when a descendant retains both pipes', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const started = Date.now()
    const outcome = runProcess(
      process.execPath,
      [fixture, 'hang', pidFile],
      {
        timeoutMs: 100,
        streamDrainTimeoutMs: 100,
      },
    )

    await expect(outcome).rejects.toThrow('timed out after 100ms')
    rememberDescendant(pidFile)
    expect(Date.now() - started).toBeLessThan(1_500)
  })
})
