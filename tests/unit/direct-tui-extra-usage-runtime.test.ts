import {
  afterAll,
  beforeEach,
  describe,
  expect,
  setDefaultTimeout,
  spyOn,
  test,
} from 'bun:test'
import { getComposerExecutionKind } from '../../src/types/command.js'

const previousApiKey = process.env.ACOSMI_API_KEY
process.env.ACOSMI_API_KEY = 'direct-tui-catalog-fixture'
const auth = await import('../../src/utils/auth.js')

// Command discovery loads the complete built-in/skill surface. The normal
// path is sub-second, but a concurrent Rust link can pause this isolated Bun
// worker past Bun's 5s default without indicating a product timeout.
setDefaultTimeout(20_000)
const overageSpy = spyOn(
  auth,
  'isOverageProvisioningAllowed',
).mockReturnValue(true)
const subscriberSpy = spyOn(auth, 'isAcosmiSubscriber').mockReturnValue(true)
const state = await import('../../src/bootstrap/state.js')
const catalog = await import('../../src/cli/headlessCommands.js')
const models = await import('../../src/utils/model/model.js')

const previousDisable = process.env.DISABLE_EXTRA_USAGE_COMMAND

beforeEach(() => {
  state.resetStateForTests()
  state.setIsInteractive(true)
  process.env.ACOSMI_API_KEY = 'direct-tui-catalog-fixture'
  delete process.env.DISABLE_EXTRA_USAGE_COMMAND
  overageSpy.mockReturnValue(true)
  subscriberSpy.mockReturnValue(true)
  catalog.clearHeadlessCommandMemoizationCaches()
})

afterAll(() => {
  overageSpy.mockRestore()
  subscriberSpy.mockRestore()
  state.resetStateForTests()
  if (previousDisable === undefined) {
    delete process.env.DISABLE_EXTRA_USAGE_COMMAND
  } else {
    process.env.DISABLE_EXTRA_USAGE_COMMAND = previousDisable
  }
  if (previousApiKey === undefined) {
    delete process.env.ACOSMI_API_KEY
  } else {
    process.env.ACOSMI_API_KEY = previousApiKey
  }
})

describe('direct TUI extra-usage catalog', () => {
  test('retains the renderer-neutral command in an interactive direct session', async () => {
    const commands = await catalog.getDirectTuiCommands(process.cwd())
    expect(commands.some(command => command.name === 'extra-usage')).toBe(
      true,
    )
    expect(commands.some(command => command.name === 'context')).toBe(false)
  })

  test('preserves the established product gate', async () => {
    overageSpy.mockReturnValue(false)
    catalog.clearHeadlessCommandMemoizationCaches()
    const commands = await catalog.getDirectTuiCommands(process.cwd())
    expect(commands.some(command => command.name === 'extra-usage')).toBe(
      false,
    )
  })

  test('executes reviewed commands only on the direct catalog', async () => {
    const direct = await catalog.getDirectTuiCommands(process.cwd())
    const headless = await catalog.getHeadlessCommands(process.cwd())

    for (const name of [
      'clear',
      'install-slack-app',
      'output-style',
      'smallmodel',
      'terminal-setup',
    ]) {
      expect(direct.some(command => command.name === name), name).toBe(
        true,
      )
      expect(headless.some(command => command.name === name), name).toBe(
        false,
      )
    }
    for (const name of ['clear', 'smallmodel', 'terminal-setup']) {
      const command = direct.find(candidate => candidate.name === name)
      expect(command?.type, name).toBe('local')
      if (!command) throw new Error(`missing direct command ${name}`)
      expect(getComposerExecutionKind(command), name).toBe('worker-slash')
    }
    expect(
      direct.some(command => command.name === 'keybindings'),
    ).toBe(false)
    expect(
      catalog.getDirectTuiBuiltInCommandNames().has('proactive'),
    ).toBe(false)
    expect(
      catalog.getHeadlessBuiltInCommandNames().has('proactive'),
    ).toBe(false)
  })

  test('preserves smallmodel description as a lazy accessor in the direct projection', async () => {
    const lookup = spyOn(
      models,
      'getSmallFastModel',
    ).mockReturnValue('lazy-small-model')
    try {
      catalog.clearHeadlessCommandMemoizationCaches()
      lookup.mockClear()
      const direct = await catalog.getDirectTuiCommands(process.cwd())
      const command = direct.find(candidate => candidate.name === 'smallmodel')
      expect(command).toBeDefined()
      expect(lookup).not.toHaveBeenCalled()

      const descriptor = Object.getOwnPropertyDescriptor(
        command as object,
        'description',
      )
      expect(descriptor?.get).toBeFunction()
      expect(descriptor).not.toHaveProperty('value')
      expect(command?.description).toContain('lazy-small-model')
      expect(lookup).toHaveBeenCalledTimes(1)
    } finally {
      lookup.mockRestore()
    }
  })

  test('preserves the fixed output-style onDone result lifecycle', async () => {
    const direct = await catalog.getDirectTuiCommands(process.cwd())
    const command = direct.find(
      candidate => candidate.name === 'output-style',
    )
    expect(command?.type).toBe('local-jsx')
    if (!command || command.type !== 'local-jsx') {
      throw new Error('direct output-style local-jsx command is absent')
    }

    let completion:
      | {
          result: string | undefined
          options:
            | {
                display?: 'skip' | 'system' | 'user'
              }
            | undefined
        }
      | undefined
    const module = await command.load()
    const jsx = await module.call(
      (result, options) => {
        completion = { result, options }
      },
      {} as never,
      '',
    )

    expect(jsx).toBeUndefined()
    expect(completion).toEqual({
      result:
        '/output-style has been deprecated. Change outputStyle in your settings file; changes take effect on the next session.',
      options: { display: 'system' },
    })
  })
})
