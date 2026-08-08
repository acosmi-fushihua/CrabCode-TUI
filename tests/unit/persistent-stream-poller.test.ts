import { describe, expect, test } from 'bun:test'

import { createPersistentStreamPoller } from '../../scripts/persistent-stream-poller.mjs'

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

describe('persistent stream poller', () => {
  test('reuses one pending read across repeated timeouts without dropping its chunk', async () => {
    const first = deferred<ReadableStreamReadResult<string>>()
    const second = deferred<ReadableStreamReadResult<string>>()
    const reads = [first, second]
    let readCalls = 0
    const poll = createPersistentStreamPoller({
      read() {
        const next = reads[readCalls]
        readCalls += 1
        if (!next) throw new Error('unexpected third read')
        return next.promise
      },
    })

    expect(await poll(1)).toEqual({ timeout: true })
    expect(await poll(1)).toEqual({ timeout: true })
    expect(await poll(1)).toEqual({ timeout: true })
    expect(readCalls).toBe(1)

    first.resolve({ done: false, value: 'initialize-response' })
    expect(await poll(100)).toEqual({
      done: false,
      value: 'initialize-response',
    })
    expect(readCalls).toBe(1)

    const secondPoll = poll(100)
    expect(readCalls).toBe(2)
    second.resolve({ done: true, value: undefined })
    expect(await secondPoll).toEqual({ done: true, value: undefined })
  })

  test('clears a failed read before the next poll', async () => {
    let readCalls = 0
    const poll = createPersistentStreamPoller({
      read() {
        readCalls += 1
        if (readCalls === 1) return Promise.reject(new Error('read failed'))
        return Promise.resolve({ done: true, value: undefined })
      },
    })

    await expect(poll(100)).rejects.toThrow('read failed')
    expect(await poll(100)).toEqual({ done: true, value: undefined })
    expect(readCalls).toBe(2)
  })
})
