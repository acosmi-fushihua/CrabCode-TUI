/**
 * Poll a Web Streams reader without ever creating overlapping read() calls.
 *
 * A timeout observes the same pending read again on the next poll. Starting a
 * new read after a timeout would leave the old promise alive; that abandoned
 * promise can consume the next chunk even though its Promise.race already
 * settled, permanently dropping protocol data.
 */
export function createPersistentStreamPoller(reader) {
  if (!reader || typeof reader.read !== 'function') {
    throw new TypeError('reader must provide read()')
  }

  let pendingRead
  return async function poll(timeoutMs) {
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0) {
      throw new TypeError('timeoutMs must be a non-negative safe integer')
    }

    pendingRead ??= beginRead(reader).then(
      value => ({ kind: 'read', value }),
      error => ({ kind: 'error', error }),
    )

    let timeoutId
    const outcome = await Promise.race([
      pendingRead,
      new Promise(resolvePoll => {
        timeoutId = setTimeout(
          () => resolvePoll({ kind: 'timeout' }),
          timeoutMs,
        )
      }),
    ])
    if (timeoutId !== undefined) clearTimeout(timeoutId)
    if (outcome.kind === 'timeout') return { timeout: true }

    pendingRead = undefined
    if (outcome.kind === 'error') throw outcome.error
    return outcome.value
  }
}

function beginRead(reader) {
  try {
    return Promise.resolve(reader.read())
  } catch (error) {
    return Promise.reject(error)
  }
}
