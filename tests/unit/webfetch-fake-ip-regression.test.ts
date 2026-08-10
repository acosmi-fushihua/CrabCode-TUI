/**
 * WebFetch fake-IP DNS regression coverage.
 *
 * A transparent proxy may resolve public hostnames into 198.18.0.0/15 and
 * route those placeholders through its TUN interface. WebFetch must allow
 * that exact hostname-resolution shape after confirming the local fake-IP
 * environment, while keeping literal benchmark IPs and every real SSRF range
 * blocked.
 */
import { afterEach, describe, expect, test } from 'bun:test'
import type { networkInterfaces } from 'os'
import {
  assertWebFetchUrlAllowed,
  deliverGuardedLookupResult,
  isWebFetchBlockedAddress,
  validateURL,
  WEB_FETCH_SSRF_BLOCKED_PREFIX,
  webFetchGuardedLookup,
} from '../../src/tools/WebFetchTool/utils.js'
import {
  __resetFakeIpDnsEnvironmentCacheForTests,
  __setFakeIpDnsEnvironmentOverrideForTests,
  isFakeIpBenchmarkAddress,
  isFakeIpDnsEnvironment,
  type FakeIpEnvProbeDeps,
} from '../../src/utils/ssrf/fakeIpEnv.js'

afterEach(() => {
  __resetFakeIpDnsEnvironmentCacheForTests()
})

function stubInterfaces(
  ...addresses: string[]
): NonNullable<FakeIpEnvProbeDeps['interfaces']> {
  return () =>
    ({
      stub: addresses.map(address => ({ address })),
    }) as unknown as ReturnType<typeof networkInterfaces>
}

type LookupStub = NonNullable<FakeIpEnvProbeDeps['lookup']>

function stubLookup(resultByHost: Record<string, string[] | Error>): LookupStub {
  return (host, _options, callback) => {
    const result = resultByHost[host]
    if (result instanceof Error) {
      callback(result as NodeJS.ErrnoException, [])
      return
    }
    callback(
      null,
      (result ?? []).map(address => ({ address, family: 4 })),
    )
  }
}

function deliver(
  hostname: string,
  addresses: { address: string; family: number }[],
  wantsAll = true,
): Promise<{ err: Error | null; address: unknown; family?: number }> {
  return new Promise(resolve => {
    void deliverGuardedLookupResult(
      hostname,
      addresses,
      wantsAll,
      (err, address, family) => resolve({ err, address, family }),
    )
  })
}

describe('fake-IP environment detection', () => {
  test.each([
    '198.18.0.0',
    '198.18.255.255',
    '198.19.0.1',
    '198.19.255.255',
  ])('recognizes the exact benchmark range: %s', address => {
    expect(isFakeIpBenchmarkAddress(address)).toBe(true)
  })

  test.each([
    '198.17.255.255',
    '198.20.0.0',
    '10.0.0.1',
    '127.0.0.1',
    '169.254.169.254',
    '192.168.0.1',
    '::ffff:198.18.1.58',
  ])('does not broaden the allowance to %s', address => {
    expect(isFakeIpBenchmarkAddress(address)).toBe(false)
  })

  test('detects a fake-IP TUN interface without consulting DNS canaries', async () => {
    let lookupCalled = false
    const result = await isFakeIpDnsEnvironment({
      interfaces: stubInterfaces('192.168.1.8', '198.18.0.1'),
      lookup: (_host, _options, callback) => {
        lookupCalled = true
        callback(null, [{ address: '93.184.216.34', family: 4 }])
      },
      canaryTimeoutMs: 20,
    })

    expect(result).toBe(true)
    expect(lookupCalled).toBe(false)
  })

  test('detects the sing-box shape through a fixed public DNS canary', async () => {
    const result = await isFakeIpDnsEnvironment({
      interfaces: stubInterfaces('172.19.0.1'),
      lookup: stubLookup({
        'example.com': ['93.184.216.34'],
        'example.org': ['198.19.0.7'],
      }),
      canaryTimeoutMs: 20,
    })

    expect(result).toBe(true)
  })

  test('probe errors fail closed', async () => {
    const result = await isFakeIpDnsEnvironment({
      interfaces: () => {
        throw new Error('interface probe failed')
      },
      lookup: () => {
        throw new Error('DNS probe failed')
      },
      canaryTimeoutMs: 20,
    })

    expect(result).toBe(false)
  })
})

describe('fake-IP connect-time allowance', () => {
  test('allows a resolved placeholder after the environment is confirmed', async () => {
    __setFakeIpDnsEnvironmentOverrideForTests(true)
    const { err, address } = await deliver('example.com', [
      { address: '198.18.1.58', family: 4 },
    ])

    expect(err).toBeNull()
    expect(address).toEqual([{ address: '198.18.1.58', family: 4 }])
  })

  test('does not let one placeholder mask another blocked answer', async () => {
    __setFakeIpDnsEnvironmentOverrideForTests(true)
    const { err } = await deliver('example.com', [
      { address: '198.18.1.58', family: 4 },
      { address: '10.0.0.5', family: 4 },
    ])

    expect((err as NodeJS.ErrnoException).code).toBe(
      'ERR_WEB_FETCH_BLOCKED_ADDRESS',
    )
  })

  test.each([
    '127.0.0.1',
    '10.0.0.1',
    '172.16.0.1',
    '192.168.0.1',
    '169.254.169.254',
    '100.64.0.1',
  ])('keeps the real SSRF range blocked: %s', async address => {
    __setFakeIpDnsEnvironmentOverrideForTests(true)
    const { err } = await deliver('example.com', [
      { address, family: 4 },
    ])
    expect(err).not.toBeNull()
  })

  test('always blocks a literal benchmark address', async () => {
    __setFakeIpDnsEnvironmentOverrideForTests(true)
    const err = await new Promise<Error | null>(resolve => {
      webFetchGuardedLookup('198.18.1.58', { all: true }, error => {
        resolve(error)
      })
    })

    expect((err as NodeJS.ErrnoException).code).toBe(
      'ERR_WEB_FETCH_BLOCKED_ADDRESS',
    )
  })

  test('an unconfirmed placeholder fails with an actionable fake-IP hint', async () => {
    __setFakeIpDnsEnvironmentOverrideForTests(false)
    const { err } = await deliver('example.com', [
      { address: '198.18.1.58', family: 4 },
    ])
    const message = (err as Error).message

    expect(message.startsWith(WEB_FETCH_SSRF_BLOCKED_PREFIX)).toBe(true)
    expect(message).toContain('fake-ip')
    expect(message).toContain('redir-host')
  })
})

describe('WebFetch DNS-free SSRF checks', () => {
  test.each([
    'http://127.0.0.1',
    'http://localhost',
    'http://foo.localhost',
    'http://[::1]',
    'http://169.254.169.254/latest/meta-data/',
    'http://metadata.google.internal/computeMetadata/v1',
    'http://10.0.0.1',
    'http://192.168.0.1',
    'ftp://example.com',
  ])('rejects %s before connecting', url => {
    expect(validateURL(url)).toBe(false)
    expect(() => assertWebFetchUrlAllowed(url)).toThrow()
  })

  test('continues to classify ordinary public addresses as safe', () => {
    expect(isWebFetchBlockedAddress('93.184.216.34')).toBe(false)
    expect(validateURL('https://example.com/page')).toBe(true)
  })
})
