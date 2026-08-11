import { describe, expect, test } from 'bun:test'
import {
  mkdtempSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { getDirectTuiBuiltInCommandDefinitions } from '../../src/cli/headlessCommands.js'
import type { ToolPresentationNode } from '../../src/Tool.js'
import type { Command } from '../../src/types/command.js'
import { AbortError } from '../../src/utils/errors.js'
import { loadTranscriptFile } from '../../src/utils/sessionStorage-transcript.js'
import { processSlashCommandCore } from '../../src/utils/processUserInput/processSlashCommandCore.js'
import type { ProcessUserInputContext } from '../../src/utils/processUserInput/processUserInputCore.js'

const LOCAL_TOKENS = [
  'advisor',
  'cost',
  'extra-usage',
  'files',
  'heapdump',
  'install-slack-app',
  'local-models',
  'proxy',
  'release-notes',
  'smallmodel',
  'terminal-setup',
  'vision',
] as const

const PROMPT_TOKENS = [
  'init',
  'insights',
  'pr-comments',
  'review',
  'security-review',
  'statusline',
] as const

const LOCAL_JSX_TOKENS = ['output-style'] as const

const RUNTIME_GENERAL_TOKENS = [
  ...LOCAL_TOKENS,
  ...PROMPT_TOKENS,
  ...LOCAL_JSX_TOKENS,
].sort()

type LifecycleOutcome = 'success' | 'failure' | 'cancellation'

function cloneCommand(
  command: Command,
  overrides: Partial<Command>,
): Command {
  const clone = Object.create(Object.getPrototypeOf(command)) as Command
  Object.defineProperties(clone, Object.getOwnPropertyDescriptors(command))
  for (const [key, value] of Object.entries(overrides)) {
    Object.defineProperty(clone, key, {
      configurable: true,
      enumerable: true,
      value,
      writable: true,
    })
  }
  return clone
}

function commandForOutcome(
  definition: Command,
  token: string,
  outcome: LifecycleOutcome,
): Command {
  const marker = `${outcome}:${token}`
  if (definition.type === 'local') {
    return cloneCommand(definition, {
      load: async () => ({
        call: async () => {
          if (outcome !== 'success') throw new Error(marker)
          return { type: 'text' as const, value: marker }
        },
      }),
    })
  }
  if (definition.type === 'local-jsx') {
    return cloneCommand(definition, {
      load: async () => ({
        call: async onDone => {
          if (outcome === 'cancellation') throw new AbortError(marker)
          if (outcome === 'failure') throw new Error(marker)
          onDone(marker, { display: 'system' })
          return null
        },
      }),
    })
  }
  return cloneCommand(definition, {
    getPromptForCommand: async () => {
      if (outcome === 'cancellation') throw new AbortError(marker)
      if (outcome === 'failure') throw new Error(marker)
      return [{ type: 'text' as const, text: marker }]
    },
  })
}

function fixtureContext(
  command: Command,
  abortController: AbortController,
): ProcessUserInputContext {
  const state = {
    mcp: { clients: [] },
    toolPermissionContext: { mode: 'default' },
  }
  return {
    abortController,
    getAppState: () => state,
    messages: [],
    options: {
      agentDefinitions: { activeAgents: [], allAgents: [] },
      commands: [command],
      ideInstallationStatus: null,
      isNonInteractiveSession: false,
      mainLoopModel: 'lifecycle-fixture-model',
      theme: 'dark',
      tools: [],
    },
    onChangeAPIKey: () => {},
    setAppState: () => {},
    setMessages: () => {},
    setResponseLength: () => {},
  } as unknown as ProcessUserInputContext
}

async function executeOutcome(
  definition: Command,
  token: string,
  outcome: LifecycleOutcome,
) {
  const abortController = new AbortController()
  if (outcome === 'cancellation' && definition.type === 'local') {
    abortController.abort()
  }
  const command = commandForOutcome(definition, token, outcome)
  return processSlashCommandCore(
    `/${token}`,
    [],
    [],
    [],
    fixtureContext(command, abortController),
    (_presentation: ToolPresentationNode | null) => {},
    undefined,
    false,
    undefined,
    {
      isBuiltInCommandName: name => name === token,
      isSubscriberGatedCommandName: () => false,
    },
  )
}

function serializedText(value: unknown): string {
  return JSON.stringify(value)
}

describe('direct TUI runtime command token-complete lifecycle matrix', () => {
  const definitions = getDirectTuiBuiltInCommandDefinitions()
  const byToken = new Map<string, Command>()
  for (const definition of definitions) {
    for (const token of [definition.name, ...(definition.aliases ?? [])]) {
      if (RUNTIME_GENERAL_TOKENS.includes(token)) {
        expect(byToken.has(token), `duplicate production owner for /${token}`).toBe(
          false,
        )
        byToken.set(token, definition)
      }
    }
  }

  test('binds every audited token to one actual production execution kind', () => {
    expect([...byToken.keys()].sort()).toEqual(RUNTIME_GENERAL_TOKENS)
    expect(
      LOCAL_TOKENS.every(token => byToken.get(token)?.type === 'local'),
    ).toBe(true)
    expect(
      PROMPT_TOKENS.every(token => byToken.get(token)?.type === 'prompt'),
    ).toBe(true)
    expect(
      LOCAL_JSX_TOKENS.every(
        token => byToken.get(token)?.type === 'local-jsx',
      ),
    ).toBe(true)
  })

  test('exercises success, failure, cancellation, terminal truth, and render input for all 19 tokens', async () => {
    for (const token of RUNTIME_GENERAL_TOKENS) {
      const definition = byToken.get(token)
      if (!definition) throw new Error(`missing production owner for /${token}`)

      const success = await executeOutcome(definition, token, 'success')
      expect(success.localCommandOutcome, `/${token} success`).toBeUndefined()
      expect(serializedText(success.messages), `/${token} success rendering`).toContain(
        `success:${token}`,
      )
      expect(success.shouldQuery, `/${token} success query boundary`).toBe(
        definition.type === 'prompt',
      )

      const failure = await executeOutcome(definition, token, 'failure')
      expect(failure.shouldQuery, `/${token} failure terminal`).toBe(false)
      expect(failure.localCommandOutcome, `/${token} failure truth`).toEqual({
        status: 'error',
        message: `Error: failure:${token}`,
      })
      expect(serializedText(failure.messages), `/${token} failure rendering`).toContain(
        'local-command-stderr',
      )
      expect(serializedText(failure.messages), `/${token} failure identity`).toContain(
        `failure:${token}`,
      )

      const cancellation = await executeOutcome(
        definition,
        token,
        'cancellation',
      )
      expect(cancellation.shouldQuery, `/${token} cancellation terminal`).toBe(
        false,
      )
      expect(
        cancellation.localCommandOutcome?.status,
        `/${token} cancellation truth`,
      ).toBe('cancelled')
      expect(
        serializedText(cancellation.messages),
        `/${token} cancellation rendering`,
      ).toContain(
        definition.type === 'prompt'
          ? 'Request interrupted by user'
          : `cancellation:${token}`,
      )
    }
  })

  test('round-trips every token-specific success projection through the physical transcript loader', async () => {
    const auditRoot = mkdtempSync(join(tmpdir(), 'direct-tui-lifecycle-'))
    const transcript = join(auditRoot, 'lifecycle.jsonl')
    const sessionId = '11111111-1111-4111-8111-111111111111'
    let parentUuid: string | null = null
    const persisted: Array<Record<string, unknown>> = []

    try {
      for (const token of RUNTIME_GENERAL_TOKENS) {
        const definition = byToken.get(token)
        if (!definition) throw new Error(`missing production owner for /${token}`)
        const success = await executeOutcome(definition, token, 'success')
        for (const message of success.messages) {
          const entry = {
            ...message,
            parentUuid,
            sessionId,
          }
          persisted.push(entry)
          parentUuid = message.uuid
        }
      }
      writeFileSync(
        transcript,
        `${persisted.map(entry => JSON.stringify(entry)).join('\n')}\n`,
      )

      const recovered = await loadTranscriptFile(transcript, {
        keepAllLeaves: true,
      })
      const recoveredText = serializedText([...recovered.messages.values()])
      for (const token of RUNTIME_GENERAL_TOKENS) {
        expect(recoveredText, `/${token} recovery`).toContain(`success:${token}`)
      }
      expect(recovered.messages.size).toBe(persisted.length)
    } finally {
      rmSync(auditRoot, { recursive: true, force: true })
    }
  })
})
