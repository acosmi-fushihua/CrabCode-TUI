import { afterEach, describe, expect, test } from 'bun:test'
import { connect as netConnect } from 'node:net'

import type { DomainFilterRules } from '../../src/utils/sandbox/networkFilter.js'
import {
  SANDBOX_PROXY_NO_PROXY,
  currentSandboxFilteringProxyPort,
  ensureSandboxFilteringProxy,
  resetSandboxNetworkProxyForTests,
  sandboxFilteringActive,
  sandboxProxyChildEnv,
  stopSandboxFilteringProxy,
} from '../../src/utils/sandbox/sandboxNetworkProxy.js'

function rules(overrides: Partial<DomainFilterRules> = {}): DomainFilterRules {
  return {
    allowedDomains: [],
    deniedDomains: [],
    allowManagedDomainsOnly: false,
    policy: 'restricted',
    allowLocalBinding: false,
    ...overrides,
  }
}

async function accepts(port: number): Promise<boolean> {
  return await new Promise<boolean>(resolve => {
    const socket = netConnect(port, '127.0.0.1')
    const settle = (value: boolean): void => {
      socket.destroy()
      resolve(value)
    }
    socket.setTimeout(2_000, () => settle(false))
    socket.once('connect', () => settle(true))
    socket.once('error', () => settle(false))
  })
}

afterEach(async () => {
  await stopSandboxFilteringProxy()
  resetSandboxNetworkProxyForTests()
})

describe('sandbox proxy lifecycle', () => {
  test('only active filtering rules start a listener', async () => {
    expect(sandboxFilteringActive(rules())).toBe(false)
    expect(await ensureSandboxFilteringProxy(rules())).toBeNull()
    expect(sandboxFilteringActive(rules({ allowManagedDomainsOnly: true }))).toBe(
      true,
    )
  })

  test('equal rules share one live port and current-port reporting is rule-bound', async () => {
    const activeRules = rules({ allowedDomains: ['example.com'] })
    const [first, second] = await Promise.all([
      ensureSandboxFilteringProxy(activeRules),
      ensureSandboxFilteringProxy({ ...activeRules }),
    ])

    expect(first).not.toBeNull()
    expect(second).toBe(first)
    expect(await accepts(first!)).toBe(true)
    expect(currentSandboxFilteringProxyPort({ ...activeRules })).toBe(first!)
    expect(
      currentSandboxFilteringProxyPort(
        rules({ allowedDomains: ['other.example'] }),
      ),
    ).toBe(0)
  })

  test('changing rules replaces the listener and stop clears its authority', async () => {
    const firstRules = rules({ allowedDomains: ['example.com'] })
    const first = await ensureSandboxFilteringProxy(firstRules)
    const secondRules = rules({ deniedDomains: ['example.com'] })
    const second = await ensureSandboxFilteringProxy(secondRules)

    expect(first).not.toBeNull()
    expect(second).not.toBeNull()
    expect(second).not.toBe(first)
    expect(await accepts(first!)).toBe(false)
    expect(await accepts(second!)).toBe(true)

    await stopSandboxFilteringProxy()
    expect(currentSandboxFilteringProxyPort(secondRules)).toBe(0)
    expect(await accepts(second!)).toBe(false)
  })

  test('policy and local-binding semantics are part of the live proxy identity', async () => {
    const strict = rules({ allowedDomains: ['example.com'] })
    const first = await ensureSandboxFilteringProxy(strict)
    const local = { ...strict, allowLocalBinding: true }

    expect(first).not.toBeNull()
    expect(currentSandboxFilteringProxyPort(local)).toBe(0)
    const second = await ensureSandboxFilteringProxy(local)
    expect(second).not.toBeNull()
    expect(second).not.toBe(first)
    expect(await accepts(first!)).toBe(false)
    expect(currentSandboxFilteringProxyPort(local)).toBe(second!)

    const none = { ...local, policy: 'none' as const }
    expect(currentSandboxFilteringProxyPort(none)).toBe(0)
  })

  test('child environment points at loopback and grants no external bypass', () => {
    expect(sandboxProxyChildEnv(0)).toEqual({})
    const env = sandboxProxyChildEnv(12_345)
    expect(env.HTTP_PROXY).toBe('http://127.0.0.1:12345')
    expect(env.http_proxy).toBe(env.HTTP_PROXY)
    expect(env.HTTPS_PROXY).toBe(env.HTTP_PROXY)
    expect(env.https_proxy).toBe(env.HTTP_PROXY)
    expect(env.NO_PROXY).toBe(SANDBOX_PROXY_NO_PROXY)
    expect(env.no_proxy).toBe(SANDBOX_PROXY_NO_PROXY)
    expect(SANDBOX_PROXY_NO_PROXY.split(',').sort()).toEqual(
      ['localhost', '127.0.0.1', '::1'].sort(),
    )
  })
})
