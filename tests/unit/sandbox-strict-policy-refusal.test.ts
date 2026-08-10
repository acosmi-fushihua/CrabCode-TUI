import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  expect,
  test,
} from 'bun:test'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { BashTool } from '../../src/tools/BashTool/BashTool.js'
import { PowerShellTool } from '../../src/tools/PowerShellTool/PowerShellTool.js'
import {
  __setEnforcedBackendProbeResultForTest,
  resetEnforcedBackendProbeCache,
} from '../../src/utils/sandbox/enforcedBackendProbe.js'
import {
  STRICT_SANDBOX_REFUSAL_PREFIX,
  buildStrictSandboxRefusal,
  isStrictSandboxBackendViolation,
  strictSandboxRefusalOrNull,
} from '../../src/utils/sandbox/strictSandboxPolicy.js'
import { updateSettingsForSource } from '../../src/utils/settings/settings.js'
import { resetSettingsCache } from '../../src/utils/settings/settingsCache.js'

const BACKEND_REASON = 'backend-unavailable:landlock-unavailable'

let configDir: string
let previousConfigDir: string | undefined

function setSandbox(enabled: boolean, allowUnsandboxedCommands: boolean): void {
  const { error } = updateSettingsForSource('userSettings', {
    sandbox: { enabled, allowUnsandboxedCommands },
  } as never)
  if (error) throw error
  resetSettingsCache()
}

function setBackend(wired: boolean): void {
  __setEnforcedBackendProbeResultForTest(
    wired
      ? { wired: true, reason: null, backend: 'sandbox-exec-linux' }
      : { wired: false, reason: BACKEND_REASON, backend: null },
  )
}

beforeAll(() => {
  configDir = mkdtempSync(join(tmpdir(), 'crabcode-tui-strict-sandbox-'))
  previousConfigDir = process.env.CRABCODE_CONFIG_DIR
  process.env.CRABCODE_CONFIG_DIR = configDir
})

afterAll(() => {
  if (previousConfigDir === undefined) delete process.env.CRABCODE_CONFIG_DIR
  else process.env.CRABCODE_CONFIG_DIR = previousConfigDir
  resetEnforcedBackendProbeCache()
  resetSettingsCache()
  rmSync(configDir, { recursive: true, force: true })
})

beforeEach(() => {
  resetEnforcedBackendProbeCache()
})

afterEach(() => {
  resetEnforcedBackendProbeCache()
})

describe('strict sandbox refusal', () => {
  test('message states cause, policy and the direct TUI recovery path', () => {
    const message = buildStrictSandboxRefusal(BACKEND_REASON)
    expect(message.startsWith(STRICT_SANDBOX_REFUSAL_PREFIX)).toBe(true)
    expect(message).toContain(BACKEND_REASON)
    expect(message).toContain('sandbox.allowUnsandboxedCommands=false')
    expect(message).toContain('dangerouslyDisableSandbox will not lift this')
    expect(message).toContain('run /sandbox in the TUI')
  })

  test('strict mode refuses when the local helper is unavailable', () => {
    setSandbox(true, false)
    setBackend(false)
    expect(isStrictSandboxBackendViolation()).toBe(true)
    expect(strictSandboxRefusalOrNull()).toBe(
      buildStrictSandboxRefusal(BACKEND_REASON),
    )
  })

  test('permissive mode or a wired helper does not trigger the refusal', () => {
    setSandbox(true, true)
    setBackend(false)
    expect(isStrictSandboxBackendViolation()).toBe(false)

    setSandbox(true, false)
    setBackend(true)
    expect(isStrictSandboxBackendViolation()).toBe(false)
  })

  test('both direct shell tools expose the same deterministic validation guard', async () => {
    setSandbox(true, false)
    setBackend(false)
    const expected = buildStrictSandboxRefusal(BACKEND_REASON)

    const bash = (await BashTool.validateInput?.({ command: 'echo ok' } as never)) as {
      result: boolean
      message?: string
    }
    const powershell = (await PowerShellTool.validateInput?.({
      command: 'Write-Output ok',
    } as never)) as { result: boolean; message?: string }

    expect(bash.result).toBe(false)
    expect(bash.message).toBe(expected)
    expect(powershell.result).toBe(false)
    expect(powershell.message).toBe(expected)
  })
})
