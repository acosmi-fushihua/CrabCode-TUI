import { afterEach, describe, expect, test } from 'bun:test'

import {
  SANDBOX_PROBE_ARGV,
  SANDBOX_PROBE_PROTOCOL_VERSION,
  __setEnforcedBackendProbeResultForTest,
  interpretProbePayload,
  markEnforcedBackendDegraded,
  probeEnforcedBackend,
  resetEnforcedBackendProbeCache,
} from '../../src/utils/sandbox/enforcedBackendProbe.js'

afterEach(() => {
  resetEnforcedBackendProbeCache()
})

describe('sandbox helper probe', () => {
  const available = {
    version: SANDBOX_PROBE_PROTOCOL_VERSION,
    platform: 'linux',
    backends: { bash: { available: true, reason: null } },
  }

  test('only the versioned available payload is considered wired', () => {
    expect(interpretProbePayload(available)).toEqual({
      wired: true,
      reason: null,
      backend: 'sandbox-exec-linux',
    })

    for (const payload of [
      null,
      {},
      { ...available, version: SANDBOX_PROBE_PROTOCOL_VERSION + 1 },
      { ...available, backends: {} },
      {
        ...available,
        backends: { bash: { available: false, reason: 'landlock-unavailable' } },
      },
    ]) {
      const result = interpretProbePayload(payload)
      expect(result.wired).toBe(false)
      expect(result.backend).toBeNull()
      expect(result.reason).toBeTruthy()
    }
  })

  test('probe argv carries the version-skew guard', () => {
    expect(SANDBOX_PROBE_ARGV).toEqual(['sandbox-probe', '--json'])
  })

  test('a runtime initialization failure disables the backend for the session', () => {
    __setEnforcedBackendProbeResultForTest({
      wired: true,
      reason: null,
      backend: 'sandbox-exec-linux',
    })

    markEnforcedBackendDegraded('sandbox-apply-failed')
    expect(probeEnforcedBackend()).toEqual({
      wired: false,
      reason: 'runtime-failure:sandbox-apply-failed',
      backend: null,
    })
  })
})
