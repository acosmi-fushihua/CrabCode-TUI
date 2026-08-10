import { afterEach, describe, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

import {
  RENAME_RETRY_BACKOFF_MS,
  TRANSIENT_RENAME_CODES,
  renameSyncWithTransientRetry,
  writeFileSyncAndFlush_DEPRECATED,
  writeFileSyncAtomicNoFallback,
  type AtomicReplaceFileSyncOperations,
} from '../../src/utils/file.js'
import {
  getFsImplementation,
  setFsImplementation,
  type FsOperations,
} from '../../src/utils/fsOperations.js'
import { createTestDir } from '../setup.js'

const errno = (code: string): Error & { code: string } =>
  Object.assign(new Error(`simulated ${code}`), { code })

/** A rename that fails `failures` times with `code`, then succeeds. */
function flakyRename(failures: number, code: string) {
  const calls: Array<[string, string]> = []
  return {
    calls,
    rename: (from: string, to: string): void => {
      calls.push([from, to])
      if (calls.length <= failures) throw errno(code)
    },
  }
}

function sleepRecorder() {
  const slept: number[] = []
  return { slept, sleep: (ms: number): void => void slept.push(ms) }
}

describe('renameSyncWithTransientRetry', () => {
  // Control: the instrument reads zero when nothing goes wrong. Without this,
  // a `slept` array that is empty for the wrong reason would look like a pass
  // in every negative case below.
  test('a rename that succeeds first try never sleeps', () => {
    const { calls, rename } = flakyRename(0, 'EPERM')
    const { slept, sleep } = sleepRecorder()

    renameSyncWithTransientRetry('a.tmp', 'a', rename, sleep)

    expect(calls).toEqual([['a.tmp', 'a']])
    expect(slept).toEqual([])
  })

  test.each([...TRANSIENT_RENAME_CODES])(
    '%s is ridden out and the rename eventually commits',
    code => {
      const { calls, rename } = flakyRename(2, code)
      const { slept, sleep } = sleepRecorder()

      renameSyncWithTransientRetry('a.tmp', 'a', rename, sleep)

      expect(calls).toHaveLength(3)
      expect(slept).toEqual(RENAME_RETRY_BACKOFF_MS.slice(0, 2))
    },
  )

  test('exhausting the ladder rethrows the last error after every backoff', () => {
    const { calls, rename } = flakyRename(Number.MAX_SAFE_INTEGER, 'EPERM')
    const { slept, sleep } = sleepRecorder()

    expect(() =>
      renameSyncWithTransientRetry('a.tmp', 'a', rename, sleep),
    ).toThrow('simulated EPERM')

    // One attempt more than there are backoffs: the ladder is the count of
    // *waits*, not of tries.
    expect(calls).toHaveLength(RENAME_RETRY_BACKOFF_MS.length + 1)
    expect(slept).toEqual([...RENAME_RETRY_BACKOFF_MS])
  })

  // The codes this must NOT retry. Each is a case where waiting cannot change
  // the answer, so a retry would only delay an honest failure — and, for
  // ENOENT, would keep re-reporting a temp file that is already gone.
  test.each(['ENOENT', 'EXDEV', 'ENOTEMPTY', 'EEXIST', 'ENOSPC'])(
    '%s is terminal — thrown on the first attempt, never slept on',
    code => {
      const { calls, rename } = flakyRename(Number.MAX_SAFE_INTEGER, code)
      const { slept, sleep } = sleepRecorder()

      expect(() =>
        renameSyncWithTransientRetry('a.tmp', 'a', rename, sleep),
      ).toThrow(`simulated ${code}`)

      expect(calls).toHaveLength(1)
      expect(slept).toEqual([])
    },
  )

  // Why the four pre-existing cases in `atomic-write-no-fallback.test.ts` are
  // unaffected by this change: they throw a bare Error with no `code`.
  test('an error without an errno code is terminal', () => {
    const calls: string[] = []
    const { slept, sleep } = sleepRecorder()

    expect(() =>
      renameSyncWithTransientRetry(
        'a.tmp',
        'a',
        () => {
          calls.push('rename')
          throw new Error('rename denied')
        },
        sleep,
      ),
    ).toThrow('rename denied')

    expect(calls).toHaveLength(1)
    expect(slept).toEqual([])
  })
})
describe('writeFileSyncAtomicNoFallback rides out a transient lock', () => {
  function makeOps(renameFailures: number, code = 'EPERM') {
    let target = 'old-generation'
    const staged = new Map<string, string>()
    const slept: number[] = []
    const unlinked: string[] = []
    let renameCalls = 0

    const ops: AtomicReplaceFileSyncOperations = {
      readlinkSync: () => {
        throw errno('EINVAL')
      },
      statSync: () => ({ mode: 0o600 }),
      writeFileSync: (path, content) => void staged.set(path, content),
      chmodSync: () => {},
      renameSync: (from, to) => {
        renameCalls += 1
        if (renameCalls <= renameFailures) throw errno(code)
        const value = staged.get(from)
        if (value === undefined) throw errno('ENOENT')
        target = value
        staged.delete(from)
        void to
      },
      unlinkSync: path => void unlinked.push(path),
      sleepSync: ms => void slept.push(ms),
    }

    return {
      ops,
      slept,
      unlinked,
      readTarget: () => target,
      renameCalls: () => renameCalls,
      stagedCount: () => staged.size,
    }
  }

  test('a lock that clears within the ladder still publishes the new generation', () => {
    const h = makeOps(2)

    writeFileSyncAtomicNoFallback(
      '/config/.credentials.json',
      'new-generation',
      { encoding: 'utf8', mode: 0o600 },
      h.ops,
    )

    expect(h.readTarget()).toBe('new-generation')
    expect(h.renameCalls()).toBe(3)
    expect(h.slept).toEqual(RENAME_RETRY_BACKOFF_MS.slice(0, 2))
    // The staged generation was committed, not cleaned up as a failure.
    expect(h.unlinked).toEqual([])
    expect(h.stagedCount()).toBe(0)
  })

  test('a lock that outlives the ladder keeps the prior generation and cleans up', () => {
    const h = makeOps(Number.MAX_SAFE_INTEGER)

    expect(() =>
      writeFileSyncAtomicNoFallback(
        '/config/.credentials.json',
        'new-generation',
        { encoding: 'utf8', mode: 0o600 },
        h.ops,
      ),
    ).toThrow('simulated EPERM')

    // The no-fallback contract is unchanged: previous target intact, staged
    // generation removed, original error surfaced.
    expect(h.readTarget()).toBe('old-generation')
    expect(h.unlinked).toHaveLength(1)
    expect(h.renameCalls()).toBe(RENAME_RETRY_BACKOFF_MS.length + 1)
  })

  // Unlike the two above, this one stays green whether or not the retry is
  // wired in — it is a correctness condition, not a reachability proof. It
  // earns its place by catching a *different* future mistake: a hand-rolled
  // retry here that skips the errno gate, or EXDEV being added to the shared
  // set. Both would leave the ladder-reachability tests green.
  test('a terminal rename error is not retried', () => {
    const h = makeOps(Number.MAX_SAFE_INTEGER, 'EXDEV')

    expect(() =>
      writeFileSyncAtomicNoFallback(
        '/config/.credentials.json',
        'new-generation',
        { encoding: 'utf8', mode: 0o600 },
        h.ops,
      ),
    ).toThrow('simulated EXDEV')

    expect(h.renameCalls()).toBe(1)
    expect(h.slept).toEqual([])
  })
})

describe('writeFileSyncAndFlush_DEPRECATED rides out a transient lock', () => {
  const realFs = getFsImplementation()
  afterEach(() => setFsImplementation(realFs))

  /**
   * Overrides only `renameSync` / `unlinkSync` on a prototype-linked clone, so
   * every other operation still hits the real filesystem and the temp file
   * genuinely lands on disk. A spread would drop anything defined on the
   * prototype; `Object.create` cannot.
   */
  function installFlakyFs(renameFailures: number) {
    const fake: FsOperations = Object.create(realFs)
    let renameCalls = 0
    const unlinked: string[] = []
    fake.renameSync = (from: string, to: string) => {
      renameCalls += 1
      if (renameCalls <= renameFailures) throw errno('EPERM')
      realFs.renameSync(from, to)
    }
    fake.unlinkSync = (path: string) => {
      unlinked.push(path)
      realFs.unlinkSync(path)
    }
    setFsImplementation(fake)
    return { renameCalls: () => renameCalls, unlinked }
  }

  test('a lock that clears within the ladder keeps the write atomic', () => {
    const dir = createTestDir('rename-retry-atomic')
    const target = join(dir, 'note.txt')
    const h = installFlakyFs(1)

    writeFileSyncAndFlush_DEPRECATED(target, 'committed', {
      encoding: 'utf-8',
    })

    expect(readFileSync(target, 'utf8')).toBe('committed')
    expect(h.renameCalls()).toBe(2)
    // Zero unlinks is what distinguishes "the retry committed it" from "the
    // non-atomic fallback rescued it" — the fallback path always unlinks the
    // staged temp file first.
    expect(h.unlinked).toEqual([])
  })

  test('a lock that outlives the ladder still reaches the non-atomic fallback', () => {
    // Costs the full ~630ms ladder in real time: this primitive has no sleep
    // seam, and proving the fallback survives retry exhaustion is worth it.
    const dir = createTestDir('rename-retry-fallback')
    const target = join(dir, 'note.txt')
    const h = installFlakyFs(Number.MAX_SAFE_INTEGER)

    writeFileSyncAndFlush_DEPRECATED(target, 'fell-back', {
      encoding: 'utf-8',
    })

    expect(readFileSync(target, 'utf8')).toBe('fell-back')
    expect(h.renameCalls()).toBe(RENAME_RETRY_BACKOFF_MS.length + 1)
    expect(h.unlinked).toHaveLength(1)
    expect(existsSync(h.unlinked[0]!)).toBe(false)
  })
})
