import { describe, expect, test } from 'bun:test'

import {
  classifyNetworkAddress,
  decideHost,
  decideResolvedAddress,
  matchesDomainPattern,
} from '../../src/utils/sandbox/networkFilter.js'

describe('sandbox domain filter', () => {
  test('plain and dotted patterns match only at a DNS label boundary', () => {
    for (const pattern of ['example.com', '.example.com']) {
      expect(matchesDomainPattern('example.com', pattern)).toBe(true)
      expect(matchesDomainPattern('api.example.com', pattern)).toBe(true)
      expect(matchesDomainPattern('notexample.com', pattern)).toBe(false)
      expect(matchesDomainPattern('evilexample.com', pattern)).toBe(false)
    }
  })

  test('wildcards include subdomains but not the apex', () => {
    expect(matchesDomainPattern('api.example.com', '*.example.com')).toBe(true)
    expect(matchesDomainPattern('example.com', '*.example.com')).toBe(false)
  })

  test('matching is case-insensitive and a single FQDN trailing dot is normalized', () => {
    expect(matchesDomainPattern('API.Example.COM.', 'example.com')).toBe(true)
    expect(matchesDomainPattern('example.com..', 'example.com')).toBe(false)
  })

  test('deny wins even when the same host is allowed', () => {
    expect(
      decideHost('api.example.com', {
        allowedDomains: ['example.com'],
        deniedDomains: ['api.example.com'],
        allowManagedDomainsOnly: false,
        policy: 'restricted',
        allowLocalBinding: false,
      }),
    ).toEqual({ allowed: false, reason: 'denied:api.example.com' })
  })

  test('an empty managed allowlist rejects every external host', () => {
    expect(
      decideHost('example.com', {
        allowedDomains: [],
        deniedDomains: [],
        allowManagedDomainsOnly: true,
        policy: 'restricted',
        allowLocalBinding: false,
      }),
    ).toEqual({ allowed: false, reason: 'not-in-managed-allowlist' })
  })

  test('classifies loopback, private, link-local, metadata and IPv6 aliases as non-public', () => {
    const cases = [
      ['127.0.0.1', 'loopback'],
      ['10.1.2.3', 'private'],
      ['172.31.255.255', 'private'],
      ['192.168.1.2', 'private'],
      ['169.254.1.2', 'link-local'],
      ['169.254.169.254', 'metadata'],
      ['100.100.100.200', 'metadata'],
      ['::1', 'loopback'],
      ['::ffff:127.0.0.1', 'loopback'],
      ['::ffff:192.168.1.2', 'private'],
      ['fc00::1', 'private'],
      ['fe80::1%lo0', 'link-local'],
      ['2001:db8::1', 'non-public'],
    ] as const
    for (const [address, scope] of cases) {
      expect(classifyNetworkAddress(address)).toBe(scope)
    }
    expect(classifyNetworkAddress('8.8.8.8')).toBe('public')
    expect(classifyNetworkAddress('2606:4700:4700::1111')).toBe('public')
    expect(classifyNetworkAddress('not-an-ip')).toBe('non-public')
  })

  test('explicit domain allow does not override address safety; local exception is loopback-only', () => {
    const base = {
      allowedDomains: ['internal.example'],
      deniedDomains: [],
      allowManagedDomainsOnly: false,
      policy: 'restricted' as const,
      allowLocalBinding: false,
    }
    expect(decideHost('internal.example', base).allowed).toBe(true)
    expect(decideResolvedAddress('127.0.0.1', base)).toEqual({
      allowed: false,
      reason: 'blocked-address:loopback',
    })
    expect(
      decideResolvedAddress('127.0.0.1', {
        ...base,
        allowLocalBinding: true,
      }),
    ).toEqual({ allowed: true, reason: 'allow-local-binding' })
    expect(
      decideResolvedAddress('192.168.1.10', {
        ...base,
        allowLocalBinding: true,
      }),
    ).toEqual({ allowed: false, reason: 'blocked-address:private' })
  })

  test('none policy blocks public addresses while restricted and host never imply LAN relay', () => {
    const base = {
      allowedDomains: [],
      deniedDomains: [],
      allowManagedDomainsOnly: false,
      allowLocalBinding: false,
    }
    expect(
      decideResolvedAddress('8.8.8.8', { ...base, policy: 'none' }),
    ).toEqual({ allowed: false, reason: 'network-policy:none' })
    for (const policy of ['restricted', 'host'] as const) {
      expect(
        decideResolvedAddress('10.0.0.8', { ...base, policy }),
      ).toEqual({ allowed: false, reason: 'blocked-address:private' })
      expect(decideResolvedAddress('8.8.8.8', { ...base, policy })).toEqual({
        allowed: true,
        reason: 'public-address',
      })
    }
  })
})
