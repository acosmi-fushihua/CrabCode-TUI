import { mock, spyOn } from 'bun:test'

import type { Command } from '../../src/types/command.js'

await import('../setup.js')

const mode = process.env.DIRECT_TUI_PRODUCTION_SMOKE_MODE

function writeEvidence(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value)}\n`)
}

function context() {
  let state = {
    advisorModel: undefined as string | undefined,
    mainLoopModel: 'sonnet',
    mcp: { clients: [] },
    toolPermissionContext: {
      mode: 'default',
      alwaysAllowRules: { command: [] as string[] },
    },
  }
  return {
    abortController: new AbortController(),
    getAppState: () => state,
    setAppState: (update: (previous: typeof state) => typeof state) => {
      state = update(state)
    },
    messages: [],
    options: {
      agentDefinitions: { activeAgents: [], allAgents: [] },
      commands: [],
      ideInstallationStatus: null,
      isNonInteractiveSession: false,
      mainLoopModel: 'sonnet',
      theme: 'dark',
      tools: [],
    },
    onChangeAPIKey: () => {},
    setMessages: () => {},
    setResponseLength: () => {},
  }
}

async function loadCall(command: Command) {
  if (command.type === 'prompt') return command.getPromptForCommand
  return (await command.load()).call
}

if (mode === 'inventory') {
  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  const tokens = [
    'advisor',
    'cost',
    'extra-usage',
    'files',
    'heapdump',
    'init',
    'insights',
    'install-slack-app',
    'local-models',
    'output-style',
    'pr-comments',
    'proxy',
    'release-notes',
    'review',
    'security-review',
    'smallmodel',
    'statusline',
    'terminal-setup',
    'vision',
  ]
  const definitions = getDirectTuiBuiltInCommandDefinitions()
  const rows = []
  for (const token of tokens) {
    const definition = definitions.find(
      command =>
        command.name === token || command.aliases?.includes(token) === true,
    )
    if (!definition) throw new Error(`missing production definition: ${token}`)
    rows.push({
      token,
      type: definition.type,
      handler: typeof (await loadCall(definition)),
    })
  }
  writeEvidence(rows)
} else if (mode === 'safe') {
  const ctx = context()
  const results: Record<string, unknown> = {}
  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  const definitions = getDirectTuiBuiltInCommandDefinitions()
  const get = (name: string): Command => {
    const command = definitions.find(candidate => candidate.name === name)
    if (!command) throw new Error(`missing safe production command: ${name}`)
    return command
  }

  const advisor = get('advisor')
  if (advisor.type !== 'local') throw new Error('advisor is not local')
  results.advisor = await (await advisor.load()).call('', ctx as never)

  const cost = get('cost')
  if (cost.type !== 'local') throw new Error('cost is not local')
  results.cost = await (await cost.load()).call('', ctx as never)

  const files = get('files')
  if (files.type !== 'local') throw new Error('files is not local')
  results.files = await (await files.load()).call('', ctx as never)

  const localModels = get('local-models')
  if (localModels.type !== 'local') throw new Error('local-models is not local')
  results['local-models'] = await (await localModels.load()).call(
    'help',
    ctx as never,
  )

  const proxy = get('proxy')
  if (proxy.type !== 'local') throw new Error('proxy is not local')
  results.proxy = await (await proxy.load()).call(
    'invalid-smoke-argument',
    ctx as never,
  )

  const vision = get('vision')
  if (vision.type !== 'local') throw new Error('vision is not local')
  results.vision = await (await vision.load()).call(
    'invalid-smoke-argument',
    ctx as never,
  )

  const outputStyle = get('output-style')
  if (outputStyle.type !== 'local-jsx') {
    throw new Error('output-style is not local-jsx')
  }
  let outputStyleCompletion: unknown
  await (await outputStyle.load()).call(
    (result, options) => {
      outputStyleCompletion = { result, options }
    },
    ctx as never,
    '',
  )
  results['output-style'] = outputStyleCompletion

  const prComments = get('pr-comments')
  if (prComments.type !== 'prompt') throw new Error('pr-comments is not prompt')
  results['pr-comments'] = await prComments.getPromptForCommand(
    '42',
    ctx as never,
  )

  const review = get('review')
  if (review.type !== 'prompt') throw new Error('review is not prompt')
  results.review = await review.getPromptForCommand('42', ctx as never)

  const statusline = get('statusline')
  if (statusline.type !== 'prompt') throw new Error('statusline is not prompt')
  results.statusline = await statusline.getPromptForCommand(
    'smoke prompt',
    ctx as never,
  )

  writeEvidence(results)
} else if (mode === 'init-onboarding') {
  const config = await import('../../src/utils/config.js')
  let saveCalls = 0
  let rejectSave = false
  let writtenConfig: unknown
  spyOn(config, 'saveCurrentProjectConfig').mockImplementation(updater => {
    saveCalls += 1
    if (rejectSave) {
      throw new Error('fixture init onboarding persistence failed')
    }
    writtenConfig = updater(config.getCurrentProjectConfig())
  })

  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  const init = getDirectTuiBuiltInCommandDefinitions().find(
    command => command.name === 'init',
  )
  if (!init || init.type !== 'prompt') {
    throw new Error('init production prompt is absent')
  }
  const ctx = context()
  ctx.options.commands = [init]
  const { processSlashCommandCore } = await import(
    '../../src/utils/processUserInput/processSlashCommandCore.js'
  )
  const execute = () =>
    processSlashCommandCore(
      '/init',
      [],
      [],
      [],
      ctx as never,
      () => {},
      undefined,
      false,
      undefined,
      {
        isBuiltInCommandName: name => name === 'init',
        isSubscriberGatedCommandName: () => false,
      },
    )

  const succeeded = await execute()
  rejectSave = true
  const failed = await execute()
  writeEvidence({
    succeeded: {
      shouldQuery: succeeded.shouldQuery,
      outcome: succeeded.localCommandOutcome,
      containsInitPrompt: JSON.stringify(succeeded.messages).includes(
        'CRABCODE.md',
      ),
    },
    failed: {
      shouldQuery: failed.shouldQuery,
      outcome: failed.localCommandOutcome,
      resultText: failed.resultText,
    },
    saveCalls,
    writtenConfig,
  })
} else if (mode === 'external') {
  let heapDumpCalls = 0
  mock.module('../../src/utils/heapDumpService.js', () => ({
    performHeapDump: async () => {
      heapDumpCalls += 1
      return heapDumpCalls === 1
        ? {
            success: true,
            heapPath: '/fixture/heap.heapsnapshot',
            diagPath: '/fixture/heap.json',
          }
        : { success: false, error: 'fixture heap failure' }
    },
  }))
  mock.module('../../src/utils/releaseNotes.js', () => ({
    CHANGELOG_URL: 'https://fixture.invalid/changelog',
    fetchAndStoreChangelog: async () => {
      throw new Error('fixture changelog unavailable')
    },
    getAllReleaseNotes: () => [],
    getStoredChangelog: async () => '',
  }))
  let localModelStatusCalls = 0
  mock.module('../../src/services/localModel/directClient.js', () => ({
    createDefaultLocalModelTuiClient: () => ({
      catalogRead: async () => ({
        data: [],
        manifestStatus: 'valid',
        manifestVersion: 1,
        source: 'fixture',
      }),
      systemProfileRead: async () => ({
        arch: 'arm64',
        memoryBytes: 1024,
        platform: 'darwin',
        recommendedRuntime: null,
      }),
      serverStatus: async () => {
        localModelStatusCalls += 1
        if (localModelStatusCalls > 1) {
          throw new Error('fixture local-model authority failed')
        }
        return { status: { state: 'stopped' } }
      },
    }),
  }))

  const heapDump = await import('../../src/commands/heapdump/heapdump.js')
  const releaseNotes = await import(
    '../../src/commands/release-notes/release-notes.js'
  )
  const localModels = await import(
    '../../src/commands/local-models/local-models.js'
  )
  const { proxyCommand } = await import('../../src/commands/proxy/proxy.js')
  const { visionCommand } = await import('../../src/commands/vision/vision.js')

  const proxyBase = {
    getProxy: () => undefined,
    getNoProxyList: () => '',
    probe: async () => ({
      kind: 'static' as const,
      url: 'http://127.0.0.1:8080',
      bypass: 'localhost',
    }),
    readUserEnv: () => undefined,
    reapplyEnv: () => {},
    processEnv: {} as Record<string, string | undefined>,
  }
  const proxySuccess = await proxyCommand('use-system', {
    ...proxyBase,
    writeSettings: () => ({ error: null }),
  } as never)
  const proxyFailure = await proxyCommand('use-system', {
    ...proxyBase,
    writeSettings: () => ({ error: new Error('fixture proxy write failed') }),
  } as never)
  const visionBase = {
    getMainModel: () => 'fixture-text-model',
    preview: () => ({
      destination: {
        mainProvider: 'fixture',
        provider: 'fixture',
        modelId: 'fixture-vision-model',
      },
    }),
    isConsentEnabled: () => false,
    getConsentBinding: () => null,
    classifyModality: () => 'text_only' as const,
    clearCache: () => {},
  }
  const visionSuccess = visionCommand('on', {
    ...visionBase,
    writeSettings: () => ({ error: null }),
  } as never)
  const visionFailure = visionCommand('on', {
    ...visionBase,
    writeSettings: () => ({ error: new Error('fixture vision write failed') }),
  } as never)

  writeEvidence({
    heapSuccess: await heapDump.call(),
    heapFailure: await heapDump.call(),
    releaseNotes: await releaseNotes.call(),
    localModelsSuccess: await localModels.call('status', context() as never),
    localModelsFailure: await localModels.call('status', context() as never),
    proxySuccess,
    proxyFailure,
    visionSuccess,
    visionFailure,
  })
} else if (mode === 'install-slack') {
  const openedUrls: string[] = []
  mock.module('../../src/utils/browser.js', () => ({
    openBrowser: async (url: string) => {
      openedUrls.push(url)
      return false
    },
  }))
  mock.module('../../src/utils/config.js', () => ({
    saveGlobalConfig: (update: (value: object) => object) => update({}),
  }))
  mock.module('../../src/services/analytics/index.js', () => ({
    logEvent: () => {},
  }))
  const installSlack = await import(
    '../../src/commands/install-slack-app/install-slack-app.js'
  )
  writeEvidence({ result: await installSlack.call(), openedUrls })
} else if (mode === 'install-slack-count') {
  const openedUrls: string[] = []
  const openResult =
    process.env.DIRECT_TUI_PRODUCTION_SMOKE_OPEN_BROWSER === '1'
  let config = { slackAppInstallCount: 3 }
  mock.module('../../src/utils/browser.js', () => ({
    openBrowser: async (url: string) => {
      openedUrls.push(url)
      return openResult
    },
  }))
  mock.module('../../src/utils/config.js', () => ({
    saveGlobalConfig: (update: (value: typeof config) => typeof config) => {
      config = update(config)
    },
  }))
  mock.module('../../src/services/analytics/index.js', () => ({
    logEvent: () => {},
  }))
  const installSlack = await import(
    '../../src/commands/install-slack-app/install-slack-app.js'
  )
  writeEvidence({
    result: await installSlack.call(),
    openedUrls,
    slackAppInstallCount: config.slackAppInstallCount,
  })
} else if (mode === 'extra-usage') {
  const openedUrls: string[] = []
  mock.module('../../src/utils/browser.js', () => ({
    openBrowser: async (url: string) => {
      openedUrls.push(url)
      return false
    },
  }))
  mock.module('../../src/utils/config.js', () => ({
    getGlobalConfig: () => ({ hasVisitedExtraUsage: true }),
    saveGlobalConfig: (update: (value: object) => object) => update({}),
  }))
  mock.module('../../src/utils/auth.js', () => ({
    getSubscriptionType: () => 'pro',
  }))
  mock.module('../../src/utils/billing.js', () => ({
    hasAcosmiBillingAccess: () => true,
  }))
  mock.module('../../src/services/api/overageCreditGrant.js', () => ({
    invalidateOverageCreditGrantCache: () => {},
  }))
  mock.module('../../src/services/api/adminRequests.js', () => ({
    checkAdminRequestEligibility: async () => {
      throw new Error('admin API must not run in the billing-access branch')
    },
    createAdminRequest: async () => {
      throw new Error('admin API must not run in the billing-access branch')
    },
    getMyAdminRequests: async () => {
      throw new Error('admin API must not run in the billing-access branch')
    },
  }))
  mock.module('../../src/services/api/usage.js', () => ({
    fetchUtilization: async () => {
      throw new Error('usage API must not run in the billing-access branch')
    },
  }))
  mock.module('../../src/utils/log.js', () => ({ logError: () => {} }))
  const extraUsage = await import(
    '../../src/commands/extra-usage/extra-usage-noninteractive.js'
  )
  writeEvidence({ result: await extraUsage.call(), openedUrls })
} else if (mode === 'terminal-failure') {
  mock.module('../../src/commands/terminalSetup/terminalSetup.js', () => ({
    call: async () => {
      throw new Error('fixture terminal setup failed')
    },
  }))
  const terminalSetup = await import('../../src/commands/terminalSetup/direct.js')
  try {
    await terminalSetup.call('', context() as never)
    writeEvidence({ status: 'unexpected-success' })
  } catch (error) {
    writeEvidence({ status: 'error', message: String(error) })
  }
} else if (mode === 'security') {
  let shellCalls = 0
  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  mock.module('../../src/utils/promptShellExecution.js', () => ({
    executeShellCommandsInPrompt: async (content: string) => {
      shellCalls += 1
      return `security-shell-boundary:${content.length}`
    },
  }))
  const securityReview = getDirectTuiBuiltInCommandDefinitions().find(
    command => command.name === 'security-review',
  )
  if (!securityReview || securityReview.type !== 'prompt') {
    throw new Error('security-review production prompt is absent')
  }
  const result = await securityReview.getPromptForCommand(
    '',
    context() as never,
  )
  writeEvidence({ result, shellCalls })
} else if (mode === 'security-failure' || mode === 'security-cancel') {
  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  const { AbortError } = await import('../../src/utils/errors.js')
  mock.module('../../src/utils/promptShellExecution.js', () => ({
    executeShellCommandsInPrompt: async () => {
      if (mode === 'security-cancel') {
        throw new AbortError('fixture security review cancelled')
      }
      throw new Error('fixture security review failed')
    },
  }))
  const securityReview = getDirectTuiBuiltInCommandDefinitions().find(
    command => command.name === 'security-review',
  )
  if (!securityReview || securityReview.type !== 'prompt') {
    throw new Error('security-review production prompt is absent')
  }
  const ctx = context()
  ctx.options.commands = [securityReview]
  const { processSlashCommandCore } = await import(
    '../../src/utils/processUserInput/processSlashCommandCore.js'
  )
  const result = await processSlashCommandCore(
    '/security-review',
    [],
    [],
    [],
    ctx as never,
    () => {},
    undefined,
    false,
    undefined,
    {
      isBuiltInCommandName: name => name === 'security-review',
      isSubscriberGatedCommandName: () => false,
    },
  )
  writeEvidence({
    outcome: result.localCommandOutcome,
    resultText: result.resultText,
  })
} else if (mode === 'advisor-persistence') {
  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  let settingsCalls = 0
  mock.module('../../src/utils/settings/settings.js', () => ({
    updateSettingsForSource: () => {
      settingsCalls += 1
      return settingsCalls === 1
        ? { error: new Error('fixture advisor persistence failed') }
        : { error: null }
    },
  }))
  const advisor = getDirectTuiBuiltInCommandDefinitions().find(
    command => command.name === 'advisor',
  )
  if (!advisor || advisor.type !== 'local') {
    throw new Error('advisor production command is absent')
  }
  const ctx = context()
  let advisorState = {
    ...ctx.getAppState(),
    advisorModel: 'persisted-advisor',
  }
  ctx.getAppState = () => advisorState
  ctx.setAppState = update => {
    advisorState = update(advisorState)
  }
  ctx.options.commands = [advisor]
  const { processSlashCommandCore } = await import(
    '../../src/utils/processUserInput/processSlashCommandCore.js'
  )
  const execute = () =>
    processSlashCommandCore(
      '/advisor off',
      [],
      [],
      [],
      ctx as never,
      () => {},
      undefined,
      false,
      undefined,
      {
        isBuiltInCommandName: name => name === 'advisor',
        isSubscriberGatedCommandName: () => false,
      },
    )
  const failed = await execute()
  const stateAfterFailure = advisorState.advisorModel
  const succeeded = await execute()
  writeEvidence({
    failed: {
      outcome: failed.localCommandOutcome,
      resultText: failed.resultText,
    },
    stateAfterFailure,
    succeeded: {
      outcome: succeeded.localCommandOutcome,
      resultText: succeeded.resultText,
    },
    stateAfterSuccess: advisorState.advisorModel ?? null,
    settingsCalls,
  })
} else if (mode?.startsWith('insights-')) {
  const controller = new AbortController()
  let generationCalls = 0
  if (mode === 'insights-pre-abort') controller.abort()
  const { getDirectTuiBuiltInCommandDefinitions } = await import(
    '../../src/cli/headlessCommands.js'
  )
  mock.module(
    '../../src/commands/insights/insightGeneration.js',
    () => ({
      generateParallelInsights: async () => {
        generationCalls += 1
        if (mode === 'insights-error') {
          throw new Error('fixture insights generation failed')
        }
        if (mode === 'insights-phase-abort') controller.abort()
        return {
          at_a_glance: { whats_working: 'production smoke' },
        }
      },
    }),
  )
  const insights = getDirectTuiBuiltInCommandDefinitions().find(
    command => command.name === 'insights',
  )
  if (!insights || insights.type !== 'prompt') {
    throw new Error('insights production prompt is absent')
  }
  const ctx = context()
  ctx.abortController = controller
  try {
    const result = await insights.getPromptForCommand('', ctx as never)
    writeEvidence({ status: 'success', generationCalls, result })
  } catch (error) {
    writeEvidence({
      status: error instanceof Error ? error.name : typeof error,
      message: String(error),
      generationCalls,
    })
  }
} else {
  throw new Error(`unknown production smoke mode: ${String(mode)}`)
}
