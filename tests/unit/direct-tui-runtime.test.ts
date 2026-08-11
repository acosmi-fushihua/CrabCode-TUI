import { afterEach, describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { runTuiRuntimeEntrypoint } from '../../src/entrypoints/tuiRuntime.js'
import { parseTuiRuntimeOptions } from '../../src/cli/tuiRuntimeOptions.js'
import {
  _resetStreamJsonStdoutGuardForTesting,
  STDOUT_GUARD_MARKER,
} from '../../src/utils/streamJsonStdoutGuard.js'
import { resolveIdeAutoConnectMcpConfig } from '../../src/utils/ide.js'
import {
  buildDownloadPath,
  parseFileSpecs,
} from '../../src/services/api/filesApi.js'

const ROOT = resolve(import.meta.dir, '../..')

function source(path: string): string {
  return readFileSync(resolve(ROOT, path), 'utf8')
}

async function* emptyRuntimeInput(): AsyncGenerator<string> {}

afterEach(() => {
  _resetStreamJsonStdoutGuardForTesting()
})

describe('dedicated native TUI runtime boundary', () => {
  test('the entry is the capability and contains no mode marker or foreign-surface vocabulary', () => {
    const entry = source('src/entrypoints/tuiRuntime.ts')

    expect(entry).toContain(
      "import { installStreamJsonStdoutGuard } from '../utils/streamJsonStdoutGuard.js'",
    )
    expect(entry).toContain("await import('../cli/tuiRuntimeBootstrap.js')")
    expect(entry).toContain('await runRuntime(options, rendererSession)')
    expect(entry).toContain('void runTuiRuntimeEntrypoint().catch')
    expect(entry).not.toContain('await runTuiRuntimeEntrypoint()')
    expect(entry.indexOf('await startRendererSession()')).toBeLessThan(
      entry.lastIndexOf("'../cli/tuiRuntimeOptions.js'"),
    )
    expect(entry).not.toContain('CRABCODE_TUI_DIRECT')
    expect(entry).not.toContain('APP_SERVER')
    expect(entry).not.toContain('AppServer')
    expect(entry).not.toContain('GUI')
    expect(entry).not.toContain('parsePrivateTuiBackendMode')
    expect(entry).not.toContain('chromeAutomation')
  })

  test('guards bootstrap stdout before the first control response and overwrites inherited entry identity', async () => {
    const originalStdoutWrite = process.stdout.write
    const originalStderrWrite = process.stderr.write
    const originalEntrypoint = process.env.CRABCODE_ENTRYPOINT
    let stdout = ''
    let stderr = ''
    process.stdout.write = ((chunk: string | Uint8Array) => {
      stdout +=
        typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8')
      return true
    }) as typeof process.stdout.write
    process.stderr.write = ((chunk: string | Uint8Array) => {
      stderr +=
        typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8')
      return true
    }) as typeof process.stderr.write
    process.env.CRABCODE_ENTRYPOINT = 'hostile-inherited-surface'

    const response = {
      type: 'control_response',
      response: {
        request_id: 'initialize',
        subtype: 'success',
      },
    }

    try {
      await runTuiRuntimeEntrypoint(
        ['bun', '/release/dist/tui-runtime/index.js'],
        {
          startRendererSession: async () => ({
            bindRendererContext: async () => {},
            projectRendererScrollSpeed: async () => {},
            ensureWorkspaceTrust: async () => {},
            requestSetup: async () => {
              throw new Error('not used by entrypoint fixture')
            },
            finishSetup: async () => emptyRuntimeInput(),
          }),
          parseOptions: async () => ({}),
          runRuntime: async () => {
            // Fixture for an eager init/setup dependency accidentally writing
            // to stdout before StructuredIO emits its initialize response.
            console.log('fixture bootstrap banner')
            process.stdout.write(`${JSON.stringify(response)}\n`)
          },
        },
      )

      expect(process.env.CRABCODE_ENTRYPOINT).toBe('cli')
      expect(stdout).toBe(`${JSON.stringify(response)}\n`)
      expect(stderr).toContain(
        `${STDOUT_GUARD_MARKER} console.log: fixture bootstrap banner`,
      )
      expect(stdout.trimStart().startsWith('{"type":"control_response"')).toBe(
        true,
      )
    } finally {
      _resetStreamJsonStdoutGuardForTesting()
      process.stdout.write = originalStdoutWrite
      process.stderr.write = originalStderrWrite
      if (originalEntrypoint === undefined) {
        delete process.env.CRABCODE_ENTRYPOINT
      } else {
        process.env.CRABCODE_ENTRYPOINT = originalEntrypoint
      }
    }
  })

  test('does not expose Chrome extension subprocess modes or runtime options', () => {
    const entry = source('src/entrypoints/tuiRuntime.ts')
    const options = source('src/cli/tuiRuntimeOptions.ts')
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    const lifecycle = source('src/cli/directTuiSetupLifecycle.ts')
    const protocol = source('src/cli/crabcodeTuiBridgeProtocol.ts')

    for (const marker of [
      '--chrome-automation-mcp',
      '--chrome-native-host',
      'runChromeAutomationMcpServer',
      'runChromeNativeHost',
    ]) {
      expect(entry).not.toContain(marker)
    }
    for (const marker of [
      "chrome?: boolean",
      ".option('--chrome')",
      ".option('--no-chrome')",
      ".option('--caps <slices>')",
      ".option('--profile <id>')",
      ".option('--session <name>')",
    ]) {
      expect(options).not.toContain(marker)
    }
    for (const marker of [
      'setupDirectChromeAutomation',
      'shouldAutoEnableChromeAutomation',
      'shouldEnableChromeAutomation',
      'markChromeAutomationMounted',
    ]) {
      expect(bootstrap).not.toContain(marker)
    }
    expect(lifecycle).not.toContain('chrome_automation_onboarding')
    expect(protocol).not.toContain('chrome_automation_onboarding')
  })

  test('parses SDK betas and the complete pane-teammate identity contract', () => {
    const options = parseTuiRuntimeOptions([
      'bun',
      '/release/dist/tui-runtime/index.js',
      '--betas',
      'context-1m-2025-08-07',
      'structured-outputs-2025-11-13',
      '--agent-id',
      'agent-7',
      '--agent-name',
      'reviewer',
      '--team-name',
      'migration',
      '--agent-color',
      'blue',
      '--parent-session-id',
      'parent-session',
      '--teammate-mode',
      'in-process',
      '--agent-type',
      'reviewer',
      '--plan-mode-required',
    ])

    expect(options.betas).toEqual([
      'context-1m-2025-08-07',
      'structured-outputs-2025-11-13',
    ])
    expect(options).toMatchObject({
      agentId: 'agent-7',
      agentName: 'reviewer',
      teamName: 'migration',
      agentColor: 'blue',
      parentSessionId: 'parent-session',
      teammateMode: 'in-process',
      agentType: 'reviewer',
      planModeRequired: true,
    })
  })

  test('permission classifier aliases preserve the established auto-mode implication', () => {
    for (const flag of [
      '--delegate-permissions',
      '--dangerously-skip-permissions-with-classifiers',
      '--afk',
    ]) {
      const options = parseTuiRuntimeOptions([
        'bun',
        '/release/dist/tui-runtime/index.js',
        flag,
      ])
      expect(options.permissionMode).toBe('auto')
    }

    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    expect(bootstrap).toContain(
      'if (options.enableAutoMode && !hasTranscriptClassifier)',
    )
    expect(bootstrap).toContain("options.permissionMode === 'auto'")
    expect(bootstrap).not.toContain(
      "if (!feature('TRANSCRIPT_CLASSIFIER')) {\n      fail('--enable-auto-mode is not available in this build')",
    )
  })

  test('--ide is parsed and produces the established dynamic IDE MCP config', async () => {
    const previous = process.env.CRABCODE_AUTO_CONNECT_IDE
    delete process.env.CRABCODE_AUTO_CONNECT_IDE
    try {
      const options = parseTuiRuntimeOptions([
        'bun',
        '/release/dist/tui-runtime/index.js',
        '--ide',
      ])
      expect(options.ide).toBe(true)
      const config = await resolveIdeAutoConnectMcpConfig(
        options.ide,
        async () => ({
          name: 'VS Code',
          port: 3117,
          workspaceFolders: ['/workspace'],
          url: 'ws://127.0.0.1:3117',
          isValid: true,
          authToken: 'fixture-token',
          ideRunningInWindows: false,
        }),
      )
      expect(config).toEqual({
        type: 'ws-ide',
        url: 'ws://127.0.0.1:3117',
        ideName: 'VS Code',
        authToken: 'fixture-token',
        ideRunningInWindows: false,
        scope: 'dynamic',
      })

      process.env.CRABCODE_AUTO_CONNECT_IDE = '0'
      expect(
        await resolveIdeAutoConnectMcpConfig(options.ide, async () => {
          throw new Error('explicit false must prevent discovery')
        }),
      ).toBeUndefined()
    } finally {
      if (previous === undefined) {
        delete process.env.CRABCODE_AUTO_CONNECT_IDE
      } else {
        process.env.CRABCODE_AUTO_CONNECT_IDE = previous
      }
    }
  })

  test('--tasks preserves optional Commander grammar and enters the direct task controller', () => {
    const defaultTasks = parseTuiRuntimeOptions([
      'bun',
      '/release/dist/tui-runtime/index.js',
      '--tasks',
    ])
    const namedTasks = parseTuiRuntimeOptions([
      'bun',
      '/release/dist/tui-runtime/index.js',
      '--tasks',
      'release-audit',
    ])
    expect(defaultTasks.tasks).toBe(true)
    expect(namedTasks.tasks).toBe('release-audit')

    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    const core = source('src/cli/print/queryExecutionCore.ts')
    expect(bootstrap).toContain(
      'process.env.CRABCODE_TASK_LIST_ID = taskListId',
    )
    expect(bootstrap).toContain('taskListId,')
    expect(core).toContain('new TaskListWatcherCore({')
    expect(core).toContain('satisfies SDKUserMessage')
    expect(core).toContain('tasksMode: true')
    expect(core).toContain('tasksMode: false')
  })

  test('--file preserves the established direct startup-ingress download contract', () => {
    const options = parseTuiRuntimeOptions([
      'bun',
      '/release/dist/tui-runtime/index.js',
      '--file',
      'file_doc:docs/spec.md',
      'file_image:images/screenshot.png',
    ])
    expect(options.file).toEqual([
      'file_doc:docs/spec.md',
      'file_image:images/screenshot.png',
    ])
    expect(parseFileSpecs(options.file ?? [])).toEqual([
      { fileId: 'file_doc', relativePath: 'docs/spec.md' },
      { fileId: 'file_image', relativePath: 'images/screenshot.png' },
    ])
    expect(buildDownloadPath('/workspace', 'session-7', 'docs/spec.md')).toBe(
      '/workspace/session-7/uploads/docs/spec.md',
    )

    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    expect(bootstrap).not.toContain(
      '--file downloads remote-ingress resources and is not available',
    )
    expect(bootstrap).toContain(
      'const fileDownloadPromise = startSessionFileDownloads(options)',
    )
    expect(bootstrap).toContain(
      'process.env.CRABCODE_REMOTE_SESSION_ID || getSessionId()',
    )
    expect(bootstrap).toContain(
      'baseUrl: process.env.ACOSMI_BASE_URL || getOauthConfig().BASE_API_URL',
    )
    expect(
      bootstrap.indexOf('await awaitSessionFileDownloads(fileDownloadPromise)'),
    ).toBeLessThan(bootstrap.indexOf('await runHeadlessDirectTui('))
  })

  test('remote session resume implementation is absent from the TUI runtime', () => {
    const options = source('src/cli/tuiRuntimeOptions.ts')
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    const loader = source('src/cli/print/sessionLoader.ts')
    expect(options).not.toContain('teleport')
    expect(bootstrap).not.toContain('teleport')
    expect(loader).not.toContain('teleport')
  })

  test('deprecated --mcp-debug is a real --debug alias rather than an accepted no-op', () => {
    const options = parseTuiRuntimeOptions([
      'bun',
      '/release/dist/tui-runtime/index.js',
      '--mcp-debug',
    ])
    expect(options.mcpDebug).toBe(true)
    expect(source('src/cli/tuiRuntimeBootstrap.ts')).toContain(
      'if (options.mcpDebug) enableDebugLogging()',
    )
  })

  test('the runtime fixes StructuredIO framing and reaches the direct QueryEngine adapter unconditionally', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    const core = source('src/cli/print/queryExecutionCore.ts')
    const queryEngine = source('src/QueryEngine.ts')

    expect(bootstrap).toContain(
      'const runtimeInput = await rendererSession.finishSetup()',
    )
    expect(bootstrap).toContain(
      'await runHeadlessDirectTui(\n    runtimeInput,',
    )
    expect(bootstrap).toContain("outputFormat: 'stream-json'")
    expect(bootstrap).toContain('verbose: true')
    expect(bootstrap).toContain(
      'permissionPromptToolName: DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME',
    )
    expect(bootstrap).not.toContain(
      'permissionPromptToolName: options.permissionPromptTool',
    )
    expect(bootstrap).toContain('setIsInteractive(true)')
    expect(bootstrap).toContain("setClientType('cli')")
    expect(bootstrap).toContain('setUsesStructuredIoTransport(true)')
    expect(bootstrap).not.toContain('setIsInteractive(false)')
    expect(bootstrap.indexOf('setIsInteractive(true)')).toBeLessThan(
      bootstrap.indexOf('await init()'),
    )
    expect(bootstrap).toContain(
      "previewFormat === 'html' || previewFormat === 'markdown'",
    )
    expect(bootstrap).toContain('await runHeadlessDirectTui(')
    expect(bootstrap).toContain(
      'setSdkBetas(filterAllowedSdkBetas(options.betas ?? []))',
    )
    expect(bootstrap).toContain('isPlanModeRequired()')
    expect(bootstrap).toContain("mode: 'plan' as const")
    expect(bootstrap).toContain('resolveIdeAutoConnectMcpConfig(options.ide)')
    expect(bootstrap).toContain(
      "connectMcpConfigs(store, { ide: config }, 'IDE')",
    )
    expect(core).toContain('processOwnedAccountBridge: true')
    expect(core).toContain('withDirectTuiPermissionBridge(args[7])')
    expect(core).not.toContain('sessionControl')
    expect(core).not.toContain('session_list')
    expect(core).toContain('interactiveProductSession: true')
    expect(core).toContain('querySource: getQuerySourceForREPL()')
    expect(core).toContain('directQueryEventDelivery: true')
    expect(core).toContain('onQueryEvent: directQueryEventSink')
    expect(core).toContain('shouldRegisterSdkHookEventHandler({')
    expect(core).toContain(
      'directQueryEventDelivery: routePolicy.directQueryEventDelivery',
    )
    expect(core).toContain('!isDirectTuiControlPlaneSdkMessage(message)')
    expect(core).toContain('interactive: routePolicy.interactiveProductSession')
    expect(core).toContain('querySource: routePolicy.querySource')
    expect(queryEngine).toContain(
      'const isNonInteractiveSession = !interactive',
    )
    expect(queryEngine).toContain('isInteractive: interactive')
  })

  test('post-trust direct services preserve backend capability without importing foreign surface housekeeping', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    const lifecycle = source(
      'src/services/memoryTierProxy/directTuiLifecycle.ts',
    )

    const finalTrust = bootstrap.indexOf('await setup(')
    const memoryImport = bootstrap.indexOf(
      "await import(\n    '../services/memoryTierProxy/directTuiLifecycle.js'",
    )
    const firstPrompt = bootstrap.indexOf('await runHeadlessDirectTui(')
    expect(finalTrust).toBeGreaterThanOrEqual(0)
    expect(memoryImport).toBeGreaterThan(finalTrust)
    expect(memoryImport).toBeLessThan(firstPrompt)
    expect(memoryImport).toBeGreaterThan(
      bootstrap.indexOf('await cleanupOrphanedPluginVersionsInBackground()'),
    )
    expect(memoryImport).toBeGreaterThan(
      bootstrap.indexOf('if (options.initOnly)'),
    )
    expect(bootstrap).not.toContain('startBackgroundHousekeeping')
    expect(lifecycle).toContain('startDirectTuiDeferredPrefetches()')
    expect(lifecycle).toContain('initMagicDocs()')
    expect(lifecycle).toContain('initSkillImprovement()')
    expect(lifecycle).toContain('prefetchMemoryUiState')
    expect(lifecycle).toContain('bootstrapMemoryBridgeAndMaybeRunners')
    expect(lifecycle).toContain('registerCleanup(releaseLeaderIfHeld)')
    expect(lifecycle).toContain(
      'autoUpdateMarketplacesAndPluginsInBackground()',
    )
    expect(lifecycle).toContain('registerSession()')
    expect(lifecycle).toContain('countConcurrentSessions()')
    expect(lifecycle).toContain("'tengu_concurrent_sessions'")
    expect(lifecycle).toContain('cleanupDirectTuiUserDataInBackground()')
    expect(lifecycle).not.toContain('cleanupOldVersions')
    expect(lifecycle).not.toContain('ensureDeepLinkProtocolRegistered')
    expect(lifecycle).not.toContain('registerProtocol')
  })

  test('permission eligibility gates remain attached to the direct backend state', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')

    expect(bootstrap).toContain(
      'checkAndDisableBypassPermissions(effectiveToolPermissionContext)',
    )
    expect(bootstrap).toContain(
      'verifyAutoModeGateAccess(\n      effectiveToolPermissionContext,',
    )
    expect(bootstrap).toContain(
      'const nextContext = updateContext(previous.toolPermissionContext)',
    )
  })

  test('remote managed executable settings are rejected without changing startup semantics', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    const remoteSettings = source('src/services/remoteManagedSettings/index.ts')
    const review = source(
      'src/services/remoteManagedSettings/securityReviewCore.ts',
    )

    const installReview = bootstrap.indexOf(
      'installDirectTuiManagedSettingsSecurityReview()',
    )
    const loadSettings = bootstrap.indexOf('await loadRemoteManagedSettings()')
    expect(installReview).toBeGreaterThanOrEqual(0)
    expect(loadSettings).toBeGreaterThan(installReview)
    expect(bootstrap).not.toContain('void loadRemoteManagedSettings()')
    expect(review).not.toContain(
      "return 'no_check_needed'\n      }\n      return 'no_check_needed'",
    )
    expect(review).toContain("return 'rejected'")
    expect(remoteSettings).toContain(
      "'Remote settings: User rejected new settings, using cached settings'",
    )
    expect(remoteSettings).toContain('return cachedSettings')
    expect(remoteSettings).not.toContain(
      'ManagedSettingsSecurityReviewRejectedError',
    )
  })

})
