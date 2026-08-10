import { describe, expect, test } from 'bun:test'

const { _renewAcosmiTokenSetForTesting } = await import(
  '../../src/services/acosmi/client.js'
)

describe('the SDK is the sole stored-token rotator', () => {
  const HOUR = 3_600_000
  const now = () => 1_700_000_000_000

  function fakeClient(options: {
    onDisk: { access_token: string; expires_at: string } | null
    afterRefresh?: { access_token: string; expires_at: string }
  }) {
    const calls = { syncFromDisk: 0, forceRefresh: 0 }
    let current = options.onDisk
    return {
      calls,
      client: {
        syncFromDisk: async () => {
          calls.syncFromDisk += 1
        },
        forceRefresh: async () => {
          calls.forceRefresh += 1
          current = options.afterRefresh ?? {
            access_token: 'rotated',
            expires_at: new Date(now() + HOUR).toISOString(),
          }
        },
        getTokenSet: () => current as never,
      },
    }
  }

  test('adopts a different usable token without rotating again', async () => {
    const { calls, client } = fakeClient({
      onDisk: {
        access_token: 'fresh-from-another-process',
        expires_at: new Date(now() + HOUR).toISOString(),
      },
    })
    const result = await _renewAcosmiTokenSetForTesting(
      'stale-rejected-token',
      { getClient: async () => client, now },
    )
    expect(result.access_token).toBe('fresh-from-another-process')
    expect(calls.syncFromDisk).toBe(1)
    expect(calls.forceRefresh).toBe(0)
  })

  test('the token just rejected by the server must be rotated', async () => {
    const { calls, client } = fakeClient({
      onDisk: {
        access_token: 'stale-rejected-token',
        expires_at: new Date(now() + HOUR).toISOString(),
      },
      afterRefresh: {
        access_token: 'rotated-token',
        expires_at: new Date(now() + HOUR).toISOString(),
      },
    })
    const result = await _renewAcosmiTokenSetForTesting(
      'stale-rejected-token',
      { getClient: async () => client, now },
    )
    expect(calls.forceRefresh).toBe(1)
    expect(result.access_token).toBe('rotated-token')
  })

  test('expired or unparseable tokens are never adopted', async () => {
    for (const expiresAt of [
      new Date(now() - HOUR).toISOString(),
      'not-a-date',
    ]) {
      const { calls, client } = fakeClient({
        onDisk: { access_token: 'different-token', expires_at: expiresAt },
      })
      await _renewAcosmiTokenSetForTesting('stale-rejected-token', {
        getClient: async () => client,
        now,
      })
      expect(calls.forceRefresh).toBe(1)
    }
  })

  test('an empty SDK result fails instead of returning a token shell', async () => {
    await expect(
      _renewAcosmiTokenSetForTesting('stale-rejected-token', {
        getClient: async () => ({
          syncFromDisk: async () => {},
          forceRefresh: async () => {},
          getTokenSet: () => null,
        }),
        now,
      }),
    ).rejects.toThrow(/no token set/)
  })
})
