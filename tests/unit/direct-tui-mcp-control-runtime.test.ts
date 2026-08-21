import { describe, expect, setDefaultTimeout, test } from 'bun:test'
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { readLastJsonEvidence } from '../helpers/readLastJsonEvidence.js'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const FIXTURE = join(
  REPO_ROOT,
  'tests',
  'fixtures',
  'direct-tui-mcp-control-runtime.ts',
)

setDefaultTimeout(30_000)

type Scenario =
  | 'public-sdk-collision'
  | 'fixed-policy-lane'
  | 'bare-fixed-public-policy'
  | 'management-sdk-failclosed'
  | 'late-fixed-logical-collision'
  | 'settings-public-toggle-race'
  | 'settings-public-desired-race'
  | 'startup-persisted-disabled-public'

type ControlResponseFrame = {
  type: 'control_response'
  response: {
    subtype: 'success' | 'error'
    request_id: string
    response?: Record<string, unknown>
    error?: string
  }
}

type Evidence = {
  scenario: Scenario
  frames: Array<{ type?: string; response?: Record<string, unknown> }>
  finalClients: Array<{ name: string; type: string; command?: string }>
  events: string[]
  sideEffects: {
    connect: string[]
    reconnect: string[]
    evict: string[]
    setEnabled: Array<{ name: string; enabled: boolean }>
    authorizeOAuth: string[]
    performOAuth: string[]
    clearAuth: string[]
  }
}

async function runScenario(scenario: Scenario): Promise<Evidence> {
  const auditRoot = mkdtempSync(join(tmpdir(), 'crabcode-direct-mcp-control-'))
  const homeDir = join(auditRoot, 'home')
  const configDir = join(auditRoot, 'config')
  const workspace = join(auditRoot, 'workspace')
  const outputPath = join(auditRoot, 'fixture.stdout')
  const errorPath = join(auditRoot, 'fixture.stderr')
  mkdirSync(homeDir, { recursive: true })
  mkdirSync(configDir, { recursive: true })
  mkdirSync(workspace, { recursive: true })

  try {
    const child = Bun.spawn({
      cmd: [process.execPath, FIXTURE],
      cwd: workspace,
      env: {
        ...process.env,
        NODE_ENV: 'test',
        HOME: homeDir,
        CRABCODE_CONFIG_DIR: configDir,
        CRABCODE_DISABLE_AUTO_MEMORY: '1',
        CRABCODE_DISABLE_TELEMETRY: '1',
        CRABCODE_FEATURE_COORDINATOR_MODE: '0',
        DISABLE_BACKGROUND_TASKS: '1',
        CRABCODE_SIMPLE:
          scenario === 'bare-fixed-public-policy' ? '1' : '0',
        DIRECT_TUI_MCP_CONTROL_SCENARIO: scenario,
      },
      stdout: Bun.file(outputPath),
      stderr: Bun.file(errorPath),
    })
    const exitCode = await child.exited
    const stderr = readFileSync(errorPath, 'utf8')
    expect(exitCode, stderr).toBe(0)
    return await readLastJsonEvidence<Evidence>(outputPath)
  } finally {
    rmSync(auditRoot, { recursive: true, force: true })
  }
}

function response(
  evidence: Evidence,
  requestId: string,
): ControlResponseFrame['response'] {
  const frame = evidence.frames.find(
    candidate =>
      candidate.type === 'control_response' &&
      candidate.response?.request_id === requestId,
  ) as ControlResponseFrame | undefined
  if (!frame) throw new Error(`missing control response: ${requestId}`)
  return frame.response
}

function mcpStatuses(
  evidence: Evidence,
  requestId: string,
): Array<{ name: string; status: string }> {
  const value = response(evidence, requestId).response?.mcpServers
  if (!Array.isArray(value)) {
    throw new Error(`missing MCP statuses: ${requestId}`)
  }
  return value as Array<{ name: string; status: string }>
}

describe('direct TUI MCP controls through real StructuredIO wiring', () => {
  test('rejects a same-name SDK replacement without deleting the existing public process owner', async () => {
    const evidence = await runScenario('public-sdk-collision')

    expect(response(evidence, 'set-process')).toMatchObject({
      subtype: 'success',
      response: { added: ['same'], removed: [], errors: {} },
    })
    expect(response(evidence, 'set-same-name-sdk')).toMatchObject({
      subtype: 'success',
      response: {
        added: [],
        removed: [],
        errors: {
          same: expect.stringContaining('not supported'),
        },
      },
    })
    expect(mcpStatuses(evidence, 'status-after-sdk-rejection')).toEqual([
      expect.objectContaining({ name: 'same', status: 'connected' }),
    ])
    expect(response(evidence, 'set-sdk-while-disabled')).toMatchObject({
      subtype: 'success',
      response: {
        added: [],
        removed: [],
        errors: { same: expect.stringContaining('not supported') },
      },
    })
    expect(
      mcpStatuses(evidence, 'status-after-disabled-sdk-rejection'),
    ).toEqual([])
    expect(response(evidence, 'set-process-v2-while-disabled')).toMatchObject({
      subtype: 'success',
      response: { added: [], removed: [], errors: {} },
    })
    expect(
      mcpStatuses(evidence, 'status-after-disabled-process-update'),
    ).toEqual([])
    expect(response(evidence, 'status-after-enable').response).toMatchObject({
      mcpServers: [
        expect.objectContaining({
          name: 'same',
          status: 'connected',
          config: expect.objectContaining({ command: 'process-v2' }),
        }),
      ],
    })
    expect(response(evidence, 'set-sdk-while-policy-blocked')).toMatchObject({
      subtype: 'success',
      response: {
        added: [],
        removed: [],
        errors: { same: expect.stringContaining('not supported') },
      },
    })
    expect(
      mcpStatuses(evidence, 'status-after-policy-sdk-rejection'),
    ).toEqual([])
    expect(evidence.finalClients).toEqual([])
    expect(evidence.sideEffects.connect).toEqual(['same'])
    expect(evidence.sideEffects.reconnect).toEqual(['same'])
    expect(evidence.sideEffects.setEnabled).toEqual([
      { name: 'same', enabled: false },
      { name: 'same', enabled: true },
    ])
    expect(evidence.sideEffects.evict).toEqual(['same', 'same', 'same'])
  })

  test('serializes a newer fixed-owner policy behind reconnect, restores on allow, and never revives an explicitly disabled owner', async () => {
    const evidence = await runScenario('fixed-policy-lane')

    expect(response(evidence, 'reconnect-raced-by-policy').subtype).toBe(
      'success',
    )
    expect(mcpStatuses(evidence, 'status-after-race')).toEqual([])
    expect(mcpStatuses(evidence, 'status-after-restore')).toEqual([
      expect.objectContaining({ name: 'fixed', status: 'connected' }),
    ])
    expect(response(evidence, 'disable-fixed').subtype).toBe('success')
    expect(mcpStatuses(evidence, 'status-after-disabled-allow')).toEqual([])
    expect(evidence.finalClients).toEqual([])

    const reconnectStart = evidence.events.indexOf('reconnect:start:1')
    const denyNotified = evidence.events.indexOf('policy:deny-notified')
    const reconnectEnd = evidence.events.indexOf('reconnect:end:1')
    const firstEviction = evidence.events.indexOf('evict:fixed')
    expect(reconnectStart).toBeGreaterThanOrEqual(0)
    expect(denyNotified).toBeGreaterThan(reconnectStart)
    expect(reconnectEnd).toBeGreaterThan(denyNotified)
    expect(firstEviction).toBeGreaterThan(reconnectEnd)

    // One manual reconnect plus one policy restoration. The final allow must
    // not schedule a third reconnect after the explicit toggle-off.
    expect(evidence.sideEffects.reconnect).toEqual(['fixed', 'fixed'])
    expect(evidence.sideEffects.setEnabled).toEqual([
      { name: 'fixed', enabled: false },
    ])
  })

  test('revalidates fixed and public policy in bare mode without reviving an explicitly disabled fixed owner', async () => {
    const evidence = await runScenario('bare-fixed-public-policy')

    expect(response(evidence, 'set-bare-public')).toMatchObject({
      subtype: 'success',
      response: { added: ['public'], removed: [], errors: {} },
    })
    expect(mcpStatuses(evidence, 'bare-status-denied')).toEqual([])
    expect(
      mcpStatuses(evidence, 'bare-status-restored').map(status => [
        status.name,
        status.status,
      ]),
    ).toEqual(
      expect.arrayContaining([
        ['fixed', 'connected'],
        ['public', 'connected'],
      ]),
    )
    expect(mcpStatuses(evidence, 'bare-status-restored')).toHaveLength(2)
    expect(response(evidence, 'bare-disable-fixed').subtype).toBe('success')
    expect(
      mcpStatuses(evidence, 'bare-status-after-disabled-allow'),
    ).toEqual([
      expect.objectContaining({ name: 'public', status: 'connected' }),
    ])
    expect(evidence.finalClients).toEqual([
      { name: 'public', type: 'connected', command: 'bare-public' },
    ])

    // Public is connected once initially and once after each allow. Fixed is
    // restored only before its explicit toggle-off.
    expect(evidence.sideEffects.connect).toEqual([
      'public',
      'public',
      'public',
    ])
    expect(evidence.sideEffects.reconnect).toEqual(['fixed'])
    expect(evidence.sideEffects.setEnabled).toEqual([
      { name: 'fixed', enabled: false },
    ])
  })

  test('fails foreign management-only SDK controls closed before every side effect', async () => {
    const evidence = await runScenario('management-sdk-failclosed')

    for (const requestId of [
      'sdk-reconnect',
      'sdk-toggle',
      'sdk-authenticate',
      'sdk-clear-auth',
    ]) {
      expect(response(evidence, requestId)).toMatchObject({
        subtype: 'error',
        error: expect.stringContaining('not supported'),
      })
    }
    expect(evidence.finalClients).toEqual([])
    expect(evidence.sideEffects).toEqual({
      connect: [],
      reconnect: [],
      evict: [],
      setEnabled: [],
      authorizeOAuth: [],
      performOAuth: [],
      clearAuth: [],
    })
  })

  test('admits a delayed fixed candidate inside the mutation lane and preserves a logical public namespace', async () => {
    const evidence = await runScenario('late-fixed-logical-collision')

    expect(response(evidence, 'set-logical-public')).toMatchObject({
      subtype: 'success',
      response: { added: ['team_alpha'], removed: [], errors: {} },
    })
    expect(response(evidence, 'late-fixed-lane-barrier')).toMatchObject({
      subtype: 'success',
      response: {
        added: [],
        removed: [],
        errors: {
          team_alpha: expect.stringContaining('Blocked by enterprise policy'),
        },
      },
    })
    expect(
      mcpStatuses(evidence, 'status-after-late-fixed-rejection'),
    ).toEqual([])
    expect(evidence.finalClients).toEqual([])
    expect(evidence.sideEffects.connect).toEqual(['team_alpha'])
    expect(evidence.sideEffects.reconnect).toEqual([])
    expect(evidence.sideEffects.evict).toEqual(['team_alpha'])
  })

  test('re-reads public disabled authority inside the lane after a slow fixed settings reconcile', async () => {
    const evidence = await runScenario('settings-public-toggle-race')

    expect(response(evidence, 'race-enable-public').subtype).toBe('success')
    expect(mcpStatuses(evidence, 'race-final-status')).toEqual([
      expect.objectContaining({ name: 'public', status: 'connected' }),
    ])
    expect(evidence.finalClients).toEqual([
      { name: 'public', type: 'connected', command: 'public-v1' },
    ])
    expect(evidence.sideEffects.reconnect).toEqual(['public'])
  })

  test('does not let a settings snapshot overwrite a newer queued public desired config', async () => {
    const evidence = await runScenario('settings-public-desired-race')

    expect(response(evidence, 'desired-race-set-v2').subtype).toBe('success')
    expect(
      response(evidence, 'desired-race-final-status').response,
    ).toMatchObject({
      mcpServers: [
        expect.objectContaining({
          name: 'public',
          status: 'connected',
          config: expect.objectContaining({ command: 'public-v2' }),
        }),
      ],
    })
    expect(evidence.finalClients).toEqual([
      { name: 'public', type: 'connected', command: 'public-v2' },
    ])
  })

  test('keeps a startup-persisted disabled public desired owner inert until explicit toggle-on', async () => {
    const evidence = await runScenario('startup-persisted-disabled-public')

    expect(response(evidence, 'persisted-disabled-set-public')).toMatchObject({
      subtype: 'success',
      response: { added: [], removed: [], errors: {} },
    })
    expect(mcpStatuses(evidence, 'persisted-disabled-status')).toEqual([])
    expect(response(evidence, 'persisted-disabled-reconnect')).toMatchObject({
      subtype: 'error',
      error: expect.stringContaining('disabled'),
    })
    expect(
      mcpStatuses(evidence, 'persisted-disabled-status-after-reconnect'),
    ).toEqual([])
    expect(response(evidence, 'persisted-disabled-toggle-on').subtype).toBe(
      'success',
    )
    expect(
      response(evidence, 'persisted-disabled-status-after-enable').response,
    ).toMatchObject({
      mcpServers: [
        expect.objectContaining({
          name: 'public-x',
          status: 'connected',
          config: expect.objectContaining({ command: 'persisted-disabled' }),
        }),
      ],
    })
    expect(evidence.sideEffects.connect).toEqual([])
    expect(evidence.sideEffects.reconnect).toEqual(['public-x'])
    expect(evidence.sideEffects.setEnabled).toEqual([
      { name: 'public-x', enabled: true },
    ])
  })
})
