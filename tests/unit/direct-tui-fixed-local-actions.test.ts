import {
  describe,
  expect,
  mock,
  setDefaultTimeout,
  spyOn,
  test,
} from 'bun:test'
import { createHash } from 'node:crypto'
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { readLastJsonEvidence } from '../helpers/readLastJsonEvidence.js'
import stripAnsi from 'strip-ansi'

import * as analytics from '../../src/services/analytics/index.js'
import { env } from '../../src/utils/env.js'
import * as model from '../../src/utils/model/model.js'
import * as allowlist from '../../src/utils/model/modelAllowlist.js'
import * as settings from '../../src/utils/settings/settings.js'
import * as sideQuery from '../../src/utils/sideQuery.js'

const REPO_ROOT = join(import.meta.dir, '..', '..')

// QueryEngine lifecycle assertions run in isolated children. Keep any
// load-induced pause distinct from the command's own terminal contract.
setDefaultTimeout(20_000)

function source(path: string): string {
  return readFileSync(join(REPO_ROOT, path), 'utf8')
}

function sha256(path: string): string {
  return createHash('sha256').update(source(path)).digest('hex')
}

type ClearFixtureEvidence = {
  oldSessionId: string
  newSessionId: string
  envelopes: Array<{
    type: string
    subtype?: string
    session_id?: string
    is_error?: boolean
    stop_reason?: string | null
    errors?: string[]
  }>
}

async function runClearQueryFixture(
  overrides: Record<string, string> = {},
): Promise<ClearFixtureEvidence> {
  const auditRoot = mkdtempSync(join(tmpdir(), 'crabcode-clear-query-engine-'))
  const configDir = join(auditRoot, 'config')
  const homeDir = join(auditRoot, 'home')
  const outputPath = join(auditRoot, 'evidence.json')
  const errorPath = join(auditRoot, 'fixture.stderr')
  mkdirSync(configDir, { recursive: true })
  mkdirSync(homeDir, { recursive: true })
  writeFileSync(
    join(configDir, '.crabcode.json'),
    JSON.stringify({ theme: 'dark', hasCompletedOnboarding: true }),
  )
  writeFileSync(
    join(configDir, 'settings.json'),
    JSON.stringify({ autoMemoryEnabled: false, disableAllHooks: true }),
  )

  try {
    const child = Bun.spawn({
      cmd: [
        process.execPath,
        join(REPO_ROOT, 'tests/fixtures/direct-tui-clear-query-engine.ts'),
      ],
      cwd: REPO_ROOT,
      env: {
        ...process.env,
        HOME: homeDir,
        CRABCODE_CONFIG_DIR: configDir,
        CRABCODE_DISABLE_AUTO_MEMORY: '1',
        CRABCODE_DISABLE_TELEMETRY: '1',
        // This fixture exercises the fixed local-command envelope only.
        // Disable the unrelated coordinator branch so Bun's raw-module
        // circular require cannot affect the assertion.
        CRABCODE_FEATURE_COORDINATOR_MODE: '0',
        DISABLE_BACKGROUND_TASKS: '1',
        ...overrides,
      },
      stdout: Bun.file(outputPath),
      stderr: Bun.file(errorPath),
    })
    const exitCode = await child.exited
    const stderr = readFileSync(errorPath, 'utf8')
    expect(exitCode, stderr).toBe(0)
    return await readLastJsonEvidence<ClearFixtureEvidence>(outputPath)
  } finally {
    rmSync(auditRoot, { recursive: true, force: true })
  }
}

describe('direct TUI fixed local actions', () => {
  test('routes clear/reset/new through the byte-exact fixed transaction owner', async () => {
    const { default: command } = await import(
      '../../src/commands/clear/index.js'
    )
    expect(command?.type).toBe('local')
    expect(command?.aliases).toEqual(['reset', 'new'])
    if (command.type !== 'local') {
      throw new Error('direct clear command is absent')
    }

    const transaction = await import(
      '../../src/commands/clear/conversation.js'
    )
    const clearSpy = spyOn(
      transaction,
      'clearConversation',
    ).mockResolvedValue()
    const context = { marker: 'direct-clear-context' }
    try {
      const action = await command.load()
      await expect(action.call('', context as never)).resolves.toEqual({
        type: 'text',
        value: '',
      })
      expect(clearSpy).toHaveBeenCalledTimes(1)
      expect(clearSpy).toHaveBeenCalledWith(context)
    } finally {
      clearSpy.mockRestore()
    }

    expect(sha256('src/commands/clear/clear.ts')).toBe(
      'eab6a857043fbf2e42605bd70d4423d6ac19e9b98ce4463cfaebc01d8be01b07',
    )
    expect(sha256('src/commands/clear/conversation.ts')).toBe(
      'e0652811485cfc0686676d1fb4faac2d10598d18ca55b24224f41e6f0e653683',
    )
    const conversation = source('src/commands/clear/conversation.ts')
    expect(conversation).toContain(
      'regenerateSessionId({ setCurrentAsParent: true })',
    )
    expect(conversation).toContain("processSessionStartHooks('clear')")
    expect(conversation).not.toContain('resetAppServerSession')
    expect(conversation).not.toMatch(/appServer/i)

    const caches = source('src/commands/clear/caches.ts')
    expect(caches).toContain(
      "from '../../utils/swarm/permissionCallbackRegistry.js'",
    )
    expect(caches).not.toContain("from '../../hooks/")
  })

  test('QueryEngine emits the regenerated clear session id on the real local-command envelope path', async () => {
    const evidence = await runClearQueryFixture()
    expect(evidence.newSessionId).not.toBe(evidence.oldSessionId)
    expect(evidence.envelopes[0]).toMatchObject({
      type: 'system',
      subtype: 'init',
      session_id: evidence.newSessionId,
    })
    expect(evidence.envelopes.at(-1)).toMatchObject({
      type: 'result',
      subtype: 'success',
      session_id: evidence.newSessionId,
    })
    expect(
      evidence.envelopes
        .filter(envelope => envelope.session_id !== undefined)
        .every(envelope => envelope.session_id === evidence.newSessionId),
    ).toBe(true)
  })

  test('projects clear transaction rejection as a terminal error through the real catalog and QueryEngine', async () => {
    const evidence = await runClearQueryFixture({
      DIRECT_TUI_CLEAR_FAILURE: '1',
    })
    expect(evidence.newSessionId).toBe(evidence.oldSessionId)
    expect(evidence.envelopes.at(-1)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: null,
      session_id: evidence.oldSessionId,
      errors: ['Error: fixture clear transaction failed'],
    })
  })

  test('projects pre-commit clear cancellation as interrupted through the real catalog and QueryEngine', async () => {
    const evidence = await runClearQueryFixture({
      DIRECT_TUI_CLEAR_PREABORT: '1',
    })
    expect(evidence.newSessionId).toBe(evidence.oldSessionId)
    expect(evidence.envelopes.at(-1)).toMatchObject({
      type: 'result',
      subtype: 'error_during_execution',
      is_error: true,
      stop_reason: 'interrupted',
      session_id: evidence.oldSessionId,
    })
  })

  test('pre-abort leaves clear session, messages, and persistence pointers untouched', async () => {
    const transaction = await import('../../src/commands/clear/conversation.js')
    const state = await import('../../src/bootstrap/state.js')
    const storage = await import('../../src/utils/sessionStorage.js')
    const abortController = new AbortController()
    abortController.abort(new DOMException('fixture pre-abort', 'AbortError'))
    const setMessages = mock(() => {})
    const setAppState = mock(() => {})
    const readFileState = new Map()
    const clearReadFileState = spyOn(readFileState, 'clear')
    const sessionBefore = state.getSessionId()
    const transcriptBefore = storage.getTranscriptPath()

    try {
      await expect(
        transaction.clearConversation({
          setMessages,
          readFileState,
          setAppState,
          abortController,
        } as never),
      ).rejects.toThrow('fixture pre-abort')
      expect(state.getSessionId()).toBe(sessionBefore)
      expect(storage.getTranscriptPath()).toBe(transcriptBefore)
      expect(setMessages).not.toHaveBeenCalled()
      expect(setAppState).not.toHaveBeenCalled()
      expect(clearReadFileState).not.toHaveBeenCalled()
    } finally {
      clearReadFileState.mockRestore()
    }
  })

  test('preserves smallmodel display, set, reset, and telemetry behavior without JSX', async () => {
    const currentModel = spyOn(
      model,
      'getSmallFastModel',
    ).mockReturnValue('small-current')
    const update = spyOn(
      settings,
      'updateSettingsForSource',
    ).mockReturnValue({ error: null })
    const log = spyOn(analytics, 'logEvent').mockImplementation(() => {})
    const allowed = spyOn(allowlist, 'isModelAllowed').mockImplementation(
      candidate => candidate !== 'illegal-model',
    )
    const query = spyOn(sideQuery, 'sideQuery').mockResolvedValue(
      {} as never,
    )
    try {
      const action = await import(
        '../../src/commands/smallmodel/smallmodel.js'
      )

      expect(stripAnsi((await action.executeSmallModelCommand('')).value)).toBe(
        'Small model: small-current',
      )
      expect(
        stripAnsi((await action.executeSmallModelCommand(' status ')).value),
      ).toBe('Small model: small-current')
      expect(update).not.toHaveBeenCalled()

      expect(
        stripAnsi(
          (await action.executeSmallModelCommand(' custom-small-model ')).value,
        ),
      ).toBe('Small model set to custom-small-model')
      expect(update).toHaveBeenLastCalledWith('userSettings', {
        smallModel: 'custom-small-model',
      })
      expect(log).toHaveBeenCalledWith('tengu_smallmodel_set', {})

      currentModel.mockReturnValue('fallback-after-reset')
      expect(
        stripAnsi((await action.executeSmallModelCommand('reset')).value),
      ).toBe('Small model reset to SDK default (fallback-after-reset)')
      expect(update).toHaveBeenLastCalledWith('userSettings', {
        smallModel: undefined,
      })

      query.mockClear()
      await expect(
        action.executeSmallModelCommand('illegal-model'),
      ).rejects.toThrow("Model 'illegal-model' is not in the list of available models")
      expect(update).toHaveBeenLastCalledWith('userSettings', {
        smallModel: undefined,
      })
      expect(query).not.toHaveBeenCalled()

      update.mockReturnValueOnce({
        error: new Error('fixture settings write failed'),
      })
      await expect(
        action.executeSmallModelCommand('failed-model'),
      ).rejects.toThrow('Failed to set small model: fixture settings write failed')
      expect(log).toHaveBeenCalledTimes(1)

      update.mockReturnValueOnce({
        error: new Error('fixture settings reset failed'),
      })
      await expect(action.executeSmallModelCommand('reset')).rejects.toThrow(
        'Failed to reset small model: fixture settings reset failed',
      )
    } finally {
      currentModel.mockRestore()
      update.mockRestore()
      log.mockRestore()
      allowed.mockRestore()
      query.mockRestore()
    }

    const actionSource = source(
      'src/commands/smallmodel/smallmodel.ts',
    )
    expect(actionSource).not.toMatch(/from ['"]react|LocalJSX/)
  })

  test('preserves terminal-setup completion on the exact private worker adapter', async () => {
    const previousTerminal = env.terminal
    env.terminal = 'ghostty'
    try {
      const catalog = await import('../../src/cli/headlessCommands.js')
      catalog.clearHeadlessCommandMemoizationCaches()
      const commands = await catalog.getDirectTuiCommands(process.cwd())
      const command = commands.find(
        candidate => candidate.name === 'terminal-setup',
      )
      expect(command?.type).toBe('local')
      if (!command || command.type !== 'local') {
        throw new Error('direct terminal-setup command is absent')
      }

      const action = await command.load()
      const result = await action.call('', {
        options: { theme: 'dark' },
      } as never)
      expect(result).toEqual({
        type: 'text',
        value:
          'Shift+Enter is natively supported in Ghostty.\n\nNo configuration needed. Just use Shift+Enter to add newlines.',
      })
    } finally {
      env.terminal = previousTerminal
    }

    const adapter = source('src/commands/terminalSetup/direct.ts')
    expect(adapter).toContain(
      "import { call as runTerminalSetup } from './terminalSetup.js'",
    )
    expect(adapter).toContain('await runTerminalSetup(onDone, context, args)')
    expect(adapter).not.toMatch(/appServer/i)

    const action = source('src/commands/terminalSetup/terminalSetup.ts')
    expect(action).not.toMatch(/from ['\"]react|from ['\"]ink/)
    expect(action).not.toMatch(/appServer/i)
  })
})
