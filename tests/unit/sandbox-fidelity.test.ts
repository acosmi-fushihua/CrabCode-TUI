import { describe, expect, test } from 'bun:test'

import {
  FIDELITY_LINUX_DENY_WITHIN_ALLOW,
  computeSandboxFidelity,
  type SandboxFidelityInput,
} from '../../src/utils/sandbox/fidelity.js'
import type { Platform } from '../../src/utils/platform.js'

function input(
  platform: Platform,
  overrides: {
    denyRead?: string[]
    denyWrite?: string[]
    allowedDomains?: string[]
    deniedDomains?: string[]
    allowManagedDomainsOnly?: boolean
    httpProxyPort?: number
  } = {},
): SandboxFidelityInput {
  return {
    platform,
    filesystem: {
      allowRead: ['/project'],
      allowWrite: ['/project'],
      denyRead: overrides.denyRead ?? [],
      denyWrite: overrides.denyWrite ?? [],
    },
    network: {
      allowedDomains: overrides.allowedDomains ?? [],
      deniedDomains: overrides.deniedDomains ?? [],
      allowManagedDomainsOnly: overrides.allowManagedDomainsOnly ?? false,
      httpProxyPort: overrides.httpProxyPort ?? 0,
    },
  }
}

describe('sandbox fidelity is conservative enough for authorization', () => {
  test('macOS domain filtering remains partial even with a live proxy', () => {
    const report = computeSandboxFidelity(
      input('macos', {
        allowedDomains: ['example.com'],
        httpProxyPort: 41_111,
      }),
    )
    expect(report.level).toBe('partial')
    expect(report.unenforced).toContain('network.allowedDomains')
  })

  test.each(['linux', 'wsl'] as const)(
    '%s domain filtering remains partial despite a live proxy',
    platform => {
      const report = computeSandboxFidelity(
        input(platform, {
          deniedDomains: ['blocked.example'],
          httpProxyPort: 41_112,
        }),
      )
      expect(report.level).toBe('partial')
      expect(report.unenforced).toContain('network.deniedDomains')
    },
  )

  test('Linux and WSL deny-within-allow cannot be promoted to full fidelity', () => {
    for (const platform of ['linux', 'wsl'] as const) {
      const report = computeSandboxFidelity(
        input(platform, { denyWrite: ['/project/.crabcode'] }),
      )
      expect(report.level).toBe('partial')
      expect(report.unenforced).toContain(FIDELITY_LINUX_DENY_WITHIN_ALLOW)
    }
  })

  test('Windows is structurally partial even with no domain or deny rules', () => {
    const report = computeSandboxFidelity(input('windows'))
    expect(report.level).toBe('partial')
    expect(report.unenforced).toContain('filesystem.allowRead')
    expect(report.unenforced).toContain('filesystem.allowWrite')
    expect(report.unenforced).toContain('network.policy')
  })

  test('macOS may be full only when no unproven domain promise is present', () => {
    expect(computeSandboxFidelity(input('macos'))).toEqual({
      level: 'full',
      unenforced: [],
    })
  })
})
