import { describe, expect, test } from 'bun:test'

import { createCompactProgressLifecycle } from '../../src/commands/compact/progressLifecycle.js'
import type { CompactProgressEvent } from '../../src/Tool.js'

function phases(events: CompactProgressEvent[]): string[] {
  return events.map(event =>
    event.type === 'hooks_start'
      ? `${event.type}:${event.hookType}`
      : event.type,
  )
}

describe('manual compact progress lifecycle', () => {
  test('session-memory success closes once without inventing terminal success', () => {
    const events: CompactProgressEvent[] = []
    const lifecycle = createCompactProgressLifecycle(event => events.push(event))

    lifecycle.emit({ type: 'compact_start' })
    lifecycle.emit({ type: 'hooks_start', hookType: 'session_start' })
    lifecycle.finish()
    lifecycle.finish()

    expect(phases(events)).toEqual([
      'compact_start',
      'hooks_start:session_start',
      'compact_end',
    ])
  })

  test('session-memory fallback continues into legacy phases without duplicates', () => {
    const events: CompactProgressEvent[] = []
    const lifecycle = createCompactProgressLifecycle(event => events.push(event))

    // Session-memory probe.
    lifecycle.emit({ type: 'compact_start' })
    // Legacy fallback reuses the same lifecycle.
    lifecycle.emit({ type: 'hooks_start', hookType: 'pre_compact' })
    lifecycle.emit({ type: 'compact_start' })
    lifecycle.emit({ type: 'hooks_start', hookType: 'session_start' })
    lifecycle.emit({ type: 'hooks_start', hookType: 'post_compact' })
    lifecycle.emit({ type: 'compact_end' })
    lifecycle.finish()

    expect(phases(events)).toEqual([
      'compact_start',
      'hooks_start:pre_compact',
      'hooks_start:session_start',
      'hooks_start:post_compact',
      'compact_end',
    ])
  })

  test('error cleanup cannot leave a row hanging or accept late success phases', () => {
    const events: CompactProgressEvent[] = []
    const lifecycle = createCompactProgressLifecycle(event => events.push(event))

    lifecycle.emit({ type: 'compact_start' })
    lifecycle.finish()
    lifecycle.emit({ type: 'hooks_start', hookType: 'post_compact' })
    lifecycle.emit({ type: 'compact_start' })
    lifecycle.finish()

    expect(phases(events)).toEqual(['compact_start', 'compact_end'])
  })

  test('an ineligible path emits no orphan compact_end', () => {
    const events: CompactProgressEvent[] = []
    createCompactProgressLifecycle(event => events.push(event)).finish()
    expect(events).toEqual([])
  })
})
