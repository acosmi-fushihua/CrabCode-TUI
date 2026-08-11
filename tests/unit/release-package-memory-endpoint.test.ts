import { describe, expect, test } from 'bun:test'

import { launcherSelectedMemoryEndpoint } from '../../scripts/release-package-smoke.mjs'

describe('release package Memory endpoint authority', () => {
  test('uses the Windows launcher endpoint instead of recomputing it from an 8.3 path alias', () => {
    const runtimeEndpoint =
      'npipe:\\\\.\\pipe\\crabcode-runneradmin-ec99293d40741729207161968c35e471-memory-orchestrator'
    const staleShortPathEndpoint =
      'npipe:\\\\.\\pipe\\crabcode-runneradmin-2f7d9664f0fcfdad9275109a513f7135-memory-orchestrator'

    expect(
      launcherSelectedMemoryEndpoint({ memory_ipc_endpoint: runtimeEndpoint }, 'win32'),
    ).toBe(runtimeEndpoint)
    expect(runtimeEndpoint).not.toBe(staleShortPathEndpoint)
  })

  test('rejects a missing, cross-platform, or multiline endpoint', () => {
    expect(() => launcherSelectedMemoryEndpoint({}, 'win32')).toThrow()
    expect(() =>
      launcherSelectedMemoryEndpoint({ memory_ipc_endpoint: 'unix:/tmp/memory.sock' }, 'win32'),
    ).toThrow()
    expect(() =>
      launcherSelectedMemoryEndpoint(
        { memory_ipc_endpoint: 'npipe:\\\\.\\pipe\\crabcode-user-ok\nforged' },
        'win32',
      ),
    ).toThrow()
  })
})
