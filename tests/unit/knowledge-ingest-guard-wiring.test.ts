import { describe, expect, test } from 'bun:test'
import { ingestHopUsesGuardedLookup } from '../../src/services/knowledgeIngest/safeIngestFetch.js'

describe('knowledge ingestion reuses the WebFetch SSRF guard', () => {
  const target = new URL('https://example.com/document')

  test('direct connections use the connect-time guard', () => {
    expect(ingestHopUsesGuardedLookup(target, undefined, undefined)).toBe(true)
  })

  test('NO_PROXY targets remain guarded even when a proxy is configured', () => {
    expect(
      ingestHopUsesGuardedLookup(
        target,
        'http://127.0.0.1:17897',
        'example.com',
      ),
    ).toBe(true)
  })

  test('targets handled by the proxy do not validate the proxy address as the target', () => {
    expect(
      ingestHopUsesGuardedLookup(
        target,
        'http://127.0.0.1:17897',
        'localhost,127.0.0.1,::1',
      ),
    ).toBe(false)
  })
})
