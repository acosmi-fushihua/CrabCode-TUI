import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  setDefaultTimeout,
  test,
} from 'bun:test'
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import {
  runBoundedProcessInventory,
  runProcess,
} from '../../scripts/release-package-smoke.mjs'

const fixtureSource = resolve(
  import.meta.dir,
  '../fixtures/inherited-stdio-process.rs',
)
const fixtureExecutionTimeoutMs = 10_000
const boundedFixtureCompletionMs = 12_000
let fixtureRoot = ''
let fixture = ''
const roots: string[] = []
const descendants = new Set<number>()

setDefaultTimeout(20_000)

beforeAll(() => {
  fixtureRoot = mkdtempSync(join(tmpdir(), 'crabcode-native-process-fixture-'))
  fixture = join(
    fixtureRoot,
    process.platform === 'win32'
      ? 'inherited-stdio-process.exe'
      : 'inherited-stdio-process',
  )
  const compilation = Bun.spawnSync({
    cmd: [
      'rustc',
      '--edition=2021',
      '-C',
      'debuginfo=0',
      fixtureSource,
      '-o',
      fixture,
    ],
    stdin: 'ignore',
    stdout: 'pipe',
    stderr: 'pipe',
  })
  if (compilation.exitCode !== 0) {
    throw new Error(
      `failed to compile native inherited-stdio fixture: ${new TextDecoder().decode(compilation.stderr)}`,
    )
  }
})

afterAll(() => {
  if (fixtureRoot) rmSync(fixtureRoot, { recursive: true, force: true })
})

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

async function waitForDescendantExit(pid: number, timeoutMs = 2_000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      process.kill(pid, 0)
      await Bun.sleep(10)
    } catch {
      return
    }
  }
  throw new Error(`native fixture descendant ${pid} survived cleanup`)
}

afterEach(async () => {
  for (const root of roots) {
    rememberDescendant(join(root, 'descendant.pid'))
  }
  const pendingDescendants = [...descendants]
  try {
    for (const pid of pendingDescendants) {
      try {
        process.kill(pid, 'SIGKILL')
      } catch {
        // The fixture descendant has already exited.
      }
    }
    await Promise.all(pendingDescendants.map(pid => waitForDescendantExit(pid)))
  } finally {
    descendants.clear()
    for (const root of roots.splice(0)) {
      rmSync(root, { recursive: true, force: true })
    }
  }
})

describe('release package bounded process runner', () => {
  test('retries only transient process inventory timeouts', () => {
    const timeout = {
      status: null,
      signal: 'SIGTERM',
      stdout: '',
      stderr: '',
      error: Object.assign(new Error('spawnSync tasklist ETIMEDOUT'), {
        code: 'ETIMEDOUT',
      }),
    }
    const success = {
      status: 0,
      signal: null,
      stdout: 'inventory',
      stderr: '',
    }
    let calls = 0
    const result = runBoundedProcessInventory(
      'tasklist',
      ['/FO', 'CSV', '/NH'],
      { timeout: 10_000 },
      () => {
        calls += 1
        return calls === 1 ? timeout : success
      },
    )

    expect(result).toBe(success)
    expect(calls).toBe(2)
  })

  test('bounds repeated inventory timeouts and does not retry other failures', () => {
    const timeout = {
      status: null,
      signal: 'SIGTERM',
      stdout: '',
      stderr: '',
      error: Object.assign(new Error('spawnSync tasklist ETIMEDOUT'), {
        code: 'ETIMEDOUT',
      }),
    }
    let timeoutCalls = 0
    expect(() =>
      runBoundedProcessInventory(
        'tasklist',
        [],
        { timeout: 10_000 },
        () => {
          timeoutCalls += 1
          return timeout
        },
      ),
    ).toThrow('process inventory failed after bounded retries')
    expect(timeoutCalls).toBe(3)

    let hardFailureCalls = 0
    expect(() =>
      runBoundedProcessInventory('tasklist', [], {}, () => {
        hardFailureCalls += 1
        return { status: 1, signal: null, stdout: '', stderr: 'denied' }
      }),
    ).toThrow('denied')
    expect(hardFailureCalls).toBe(1)
  })

  test('accepts a complete contract when a successful descendant keeps stdio open', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const started = Date.now()
    const result = await runProcess(
      fixture,
      ['exit', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
        streamDrainTimeoutMs: 100,
        allowInheritedPipeHandles: true,
        requiredStdoutSuffix: '\n',
      },
    )
    const descendantPid = rememberDescendant(pidFile)

    expect(Date.now() - started).toBeLessThan(boundedFixtureCompletionMs)
    expect(result.streamsClosed).toBe(false)
    expect(JSON.parse(result.stdout)).toEqual({ descendantPid })
    expect(result.stderr).toBe('')
    expect(result.processObserverLease).not.toBeNull()

    // The native fixture matches the Rust launcher's cross-platform process
    // semantics: its descendant owns the inherited pipes after it exits.
    await Bun.sleep(250)
    expect(() => process.kill(descendantPid!, 0)).not.toThrow()
    process.kill(descendantPid!, 'SIGKILL')
    await result.processObserverLease!.finalize(2_000)
  })

  test('retains an explicitly requested observer after both streams reach EOF', async () => {
    const pidFile = join(scratch(), 'unused.pid')
    const result = await runProcess(
      fixture,
      ['closed', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
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
      fixture,
      ['exit', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
        streamDrainTimeoutMs: 100,
      },
    )

    await expect(outcome).rejects.toThrow('descendant processes kept stdio open')
    rememberDescendant(pidFile)
    expect(Date.now() - started).toBeLessThan(boundedFixtureCompletionMs)
  })

  test('bounds finalization when a deferred descendant never closes its pipes', async () => {
    const pidFile = join(scratch(), 'descendant.pid')
    const result = await runProcess(
      fixture,
      ['exit', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
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
      fixture,
      ['partial', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
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
      fixture,
      ['stderr', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
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
      fixture,
      ['hang', pidFile],
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
      fixture,
      ['late', pidFile],
      {
        timeoutMs: fixtureExecutionTimeoutMs,
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
