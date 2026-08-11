import type { CompactProgressEvent } from '../../Tool.js'

export type CompactProgressLifecycle = {
  emit: (event: CompactProgressEvent) => void
  finish: () => void
}

/**
 * One logical manual compaction can probe session memory and then fall back to
 * the legacy summarizer. Both implementations report through this lifecycle
 * so the renderer sees one progress row, one occurrence of each phase, and
 * exactly one cleanup event after progress has actually started.
 *
 * `compact_end` is deliberately a lifecycle cleanup signal, not a success
 * signal. The renderer removes the transient row; command stdout/boundary or
 * stderr/result envelopes own the terminal truth.
 */
export function createCompactProgressLifecycle(
  observer: ((event: CompactProgressEvent) => void) | undefined,
): CompactProgressLifecycle {
  let started = false
  let finished = false
  const observedPhases = new Set<string>()

  const emit = (event: CompactProgressEvent): void => {
    if (finished) return

    if (event.type === 'compact_end') {
      if (!started) return
      finished = true
      observer?.(event)
      return
    }

    const phaseKey =
      event.type === 'hooks_start'
        ? `${event.type}:${event.hookType}`
        : event.type
    if (observedPhases.has(phaseKey)) return

    started = true
    observedPhases.add(phaseKey)
    observer?.(event)
  }

  return {
    emit,
    finish: () => emit({ type: 'compact_end' }),
  }
}
