import { readFileSync } from 'node:fs'

const EVIDENCE_FLUSH_DEADLINE_MS = 1_000

/**
 * Bun 1.3.x can resolve Subprocess.exited before a Bun.file stdout sink has
 * flushed its final bytes under cross-file test contention. Every caller's
 * fixture terminates its evidence with one newline-delimited JSON frame, so
 * wait only for that existing protocol boundary and keep the historical
 * last-non-empty-line parsing semantics.
 */
export async function readLastJsonEvidence<T>(path: string): Promise<T> {
  const deadline = performance.now() + EVIDENCE_FLUSH_DEADLINE_MS
  let lastJsonError: unknown

  while (true) {
    // A missing sink is a harness failure, not a flush race. Let readFileSync's
    // concrete filesystem error fail immediately instead of retrying it.
    const output = readFileSync(path, 'utf8')
    const lastLine = output.trim().split('\n').at(-1)
    if (output.endsWith('\n') && lastLine) {
      try {
        return JSON.parse(lastLine) as T
      } catch (error) {
        lastJsonError = error
      }
    }

    if (performance.now() >= deadline) {
      const detail =
        lastJsonError instanceof Error
          ? `: ${lastJsonError.message}`
          : ''
      throw new Error(
        `timed out waiting for newline-terminated JSON evidence at ${path}${detail}`,
        { cause: lastJsonError },
      )
    }

    // Yield to Bun's pending file-sink flush; do not hide the race behind an
    // unconditional wall-clock sleep.
    await new Promise<void>(resolve => setImmediate(resolve))
  }
}
