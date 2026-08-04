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
    expect(result.processObserverLease).not.toBeNull()

    // The lease must keep the Windows Bun Job/pipe ownership alive until the
    // package-level daemon lifecycle is explicitly complete.
    await Bun.sleep(250)
    expect(() => process.kill(descendantPid!, 0)).not.toThrow()
    process.kill(descendantPid!, 'SIGKILL')
    await result.processObserverLease!.finalize(2_000)
  })

  test('retains an explicitly requested observer after both streams reach EOF', async () => {
    const pidFile = join(scratch(), 'unused.pid')
    const result = await runProcess(
      process.execPath,
      [fixture, 'closed', pidFile],
      {
        timeoutMs: 2_000,
        streamDrainTimeoutMs: 100,
        retainProcessObserverUntilReleased: true,
      },
    )

    expect(result.streamsClosed).toBe(true)
    expect(JSON.parse(result.stdout)).toEqual({ closed: true })
    expect(result.processObserverLease).not.toBeNull()
    await result.processObserverLease!.finalize(2_000)
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

  test('bounds finalization when a deferred descendant never closes its pipes', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
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
    rememberDescendant(pidFile)
    const started = Date.now()

    await expect(result.processObserverLease!.finalize(100)).rejects.toThrow(
      'descendants kept stdio open after the package lifecycle completed',
    )
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

  test('rejects output emitted after a deferred contract was accepted', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const result = await runProcess(
      process.execPath,
      [fixture, 'late', pidFile],
      {
        timeoutMs: 2_000,
        streamDrainTimeoutMs: 100,
        allowInheritedPipeHandles: true,
        requiredStdoutSuffix: '\n',
      },
    )
    rememberDescendant(pidFile)

    expect(result.processObserverLease).not.toBeNull()
    await expect(result.processObserverLease!.finalize(2_000)).rejects.toThrow(
      'emitted data after its stdout/stderr contract was accepted',
    )
  })
})
