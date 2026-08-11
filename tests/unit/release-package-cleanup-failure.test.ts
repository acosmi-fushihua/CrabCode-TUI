import { describe, expect, test } from 'bun:test'
import { combinePrimaryAndCleanupFailures } from '../../scripts/release-package-smoke.mjs'

describe('release package cleanup failure precedence', () => {
  test('keeps the product failure primary and attaches cleanup diagnostics', () => {
    const primary = new Error('stable launcher failed')
    const cleanup = new Error('process inventory timed out')

    const result = combinePrimaryAndCleanupFailures(primary, [cleanup], 'cleanup failed')

    expect(result).toBe(primary)
    expect(result?.message).toBe('stable launcher failed')
    expect(result?.cause).toBe(cleanup)
  })

  test('still fails closed when cleanup is the only failure', () => {
    const cleanup = new Error('process leak survived')

    expect(combinePrimaryAndCleanupFailures(null, [cleanup], 'cleanup failed')).toBe(cleanup)
    expect(combinePrimaryAndCleanupFailures(null, [], 'cleanup failed')).toBeNull()
  })
})
