import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import uniqBy from 'lodash-es/uniqBy.js'

import type { TuiRuntimeOptions } from './tuiRuntimeOptions.js'
import { DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME } from './directTuiPermissionBridge.js'
import { runHeadlessDirectTui } from './print/queryExecutionCore.js'
import {
  getDirectTuiCommands,
  installDirectTuiCommandSurface,
} from './headlessCommands.js'
import { init, initializeTelemetryAfterTrust } from '../entrypoints/init.js'
import { setup } from '../setup.js'
import { getSystemContext } from '../context.js'
import { getDefaultAppState, type AppState } from '../state/AppStateStore.js'
import { onChangeAppState } from '../state/onChangeAppState.js'
import { createStore } from '../state/store.js'
import { getTools } from '../tools.js'
import {
  createSyntheticOutputTool,
  isSyntheticOutputToolEnabled,
} from '../tools/SyntheticOutputTool/SyntheticOutputTool.js'
import {
  getActiveAgentsFromList,
  getAgentDefinitionsWithOverrides,
  isBuiltInAgent,
  parseAgentsFromJson,
} from '../tools/AgentTool/loadAgentsDir.js'
import { initBuiltinPlugins } from '../plugins/bundled/index.js'
import { initBundledSkills } from '../skills/bundled/index.js'
import { getCwd } from '../utils/cwd.js'
import {
  checkAndDisableBypassPermissions,
  initialPermissionModeFromCLI,
  initializeToolPermissionContext,
  isDefaultPermissionModeAuto,
  verifyAutoModeGateAccess,
} from '../utils/permissions/permissionSetup.js'
import {
  getDefaultFallbackModel,
  getDefaultMainLoopModel,
  normalizeModelStringForAPI,
  parseUserSpecifiedModel,
} from '../utils/model/model.js'
import {
  type ChannelEntry,
  getInitialMainLoopModel,
  getSessionId,
  setAdditionalDirectoriesForCrabcodeMd,
  setAllowedChannels,
  setClientType,
  setInitialMainLoopModel,
  setInlinePlugins,
  setIsInteractive,
  setMainLoopModelOverride,
  setMainThreadAgentType,
  setQuestionPreviewFormat,
  setSdkBetas,
  setSessionBypassPermissionsMode,
  setSessionPersistenceDisabled,
  setUsesStructuredIoTransport,
} from '../bootstrap/state.js'
import { eagerLoadSettings } from '../main/settingsLoader.js'
import { applyConfigEnvironmentVariables } from '../utils/managedEnv.js'
import { getInitialSettings } from '../utils/settings/settings.js'
import { getInitialEffortSetting, parseEffortValue } from '../utils/effort.js'
import {
  shouldEnableThinkingByDefault,
  type ThinkingConfig,
} from '../utils/thinking.js'
import { getUserSpecifiedModelSetting } from '../utils/model/model.js'
import {
  getInitialFastModeSetting,
  isFastModeEnabled,
} from '../utils/fastMode.js'
import { safeParseJSON } from '../utils/json.js'
import { logError } from '../utils/log.js'
import {
  enableDebugLogging,
  logForDebugging,
  setHasFormattedOutput,
} from '../utils/debug.js'
import { errorMessage } from '../utils/errors.js'
import { validateUuid } from '../utils/uuid.js'
import { cacheSessionTitle } from '../utils/sessionStorage.js'
import { initializeVersionedPlugins } from '../utils/plugins/installedPluginsManager.js'
import { cleanupOrphanedPluginVersionsInBackground } from '../utils/plugins/cacheUtils.js'
import { getGlobExclusionsForPluginCache } from '../utils/plugins/orphanedPluginFilter.js'
import { clearPluginCache } from '../utils/plugins/pluginLoader.js'
import { isBareMode, isEnvTruthy } from '../utils/envUtils.js'
import {
  parseMcpConfig,
  parseMcpConfigFromFilePath,
  getCrabCodeMcpConfigs,
  filterMcpServersByPolicy,
  doesEnterpriseMcpConfigExist,
  areMcpConfigsAllowedWithEnterpriseMcpConfig,
  isMcpServerDisabled,
  isMcpServerRuntimeActive,
  dedupAcosmiMcpServers,
  getMcpServerSignature,
} from '../services/mcp/config.js'
import type {
  McpSdkServerConfig,
  McpServerConfig,
  ScopedMcpServerConfig,
} from '../services/mcp/types.js'
import type { McpServerConfigForProcessTransport } from '../entrypoints/agentSdkTypes.js'
import {
  clearServerCache,
  getMcpToolsCommandsAndResources,
} from '../services/mcp/client.js'
import {
  excludeCommandsByServer,
  excludeResourcesByServer,
} from '../services/mcp/utils.js'
import { fetchAcosmiMcpConfigsIfEligible } from '../services/mcp/acosmi.js'
import {
  admitStartupMcpNamespaceReservations,
  filterDirectTuiProcessMcpServers,
  filterMcpServersForOwner,
  orderMcpServersByPrecedence,
} from './print/mcpServerOwnership.js'
import {
  processSessionStartHooks,
  processSetupHooks,
} from '../utils/sessionStart.js'
import {
  setAllHookEventsEnabled,
  suppressHookEventDelivery,
} from '../utils/hooks/hookEvents.js'
import { validateForceLoginOrg } from '../utils/auth.js'
import { loadRemoteManagedSettings } from '../services/remoteManagedSettings/index.js'
import { installDirectTuiManagedSettingsSecurityReview } from '../services/remoteManagedSettings/securityReviewCore.js'
import { loadPolicyLimits } from '../services/policyLimits/index.js'
import { filterAllowedSdkBetas } from '../utils/betas.js'
import { feature } from '../utils/featurePolyfill.js'
import {
  extractTeammateOptions,
  maybeActivateBrief,
  maybeActivateProactive,
  type TeammateOptions,
} from '../main/modeHelpers.js'
import { isAgentSwarmsEnabled } from '../utils/agentSwarmsEnabled.js'
import { isPlanModeRequired, setDynamicTeamContext } from '../utils/teammate.js'
import { setCliTeammateModeOverride } from '../utils/swarm/backends/teammateModeSnapshot.js'
import { TEAMMATE_SYSTEM_PROMPT_ADDENDUM } from '../utils/swarm/teammatePromptAddendum.js'
import {
  canUserConfigureAdvisor,
  getInitialAdvisorSetting,
  isAdvisorEnabled,
  isValidAdvisorModel,
  modelSupportsAdvisor,
} from '../utils/advisor.js'
import { setAutoModeFlagCli } from '../utils/permissions/autoModeState.js'
import { resolveIdeAutoConnectMcpConfig } from '../utils/ide.js'
import { installDirectTuiCronEnsureProvider } from '../utils/installDirectTuiCronEnsureProvider.js'
import { createDefaultLocalModelDirectClient } from '../services/localModel/directClient.js'
import { installLocalModelServerStatusProvider } from '../services/localModel/localModelChatStream.js'
import { DEFAULT_TASKS_MODE_TASK_LIST_ID } from '../utils/tasks.js'
import {
  type DownloadResult,
  downloadSessionFiles,
  type FilesApiConfig,
  parseFileSpecs,
} from '../services/api/filesApi.js'
import { getSessionIngressAuthToken } from '../utils/sessionIngressAuth.js'
import { getOauthConfig } from '../constants/oauth.js'
import type { NativeTuiRendererSession } from '../entrypoints/nativeTuiRendererSession.js'
import {
  runDirectTuiPostTrustSetup,
  runDirectTuiPreTrustOnboarding,
} from './directTuiSetupLifecycle.js'
import { runDirectTuiStartupSessionPicker } from './directTuiSessionPicker.js'
import { updateGithubRepoPathMapping } from '../utils/githubRepoPathMapping.js'
import { updateDeepLinkTerminalPreference } from '../utils/deepLink/terminalPreference.js'
import {
  initializeGrowthBook,
  resetGrowthBook,
} from '../services/analytics/growthbook.js'

type MutableMcpState = AppState['mcp']

function fail(message: string): never {
  throw new Error(message)
}

function validateDirectRuntimeSurface(options: TuiRuntimeOptions): void {
  if (
    options.outputFormat !== undefined &&
    options.outputFormat !== 'stream-json'
  ) {
    fail(
      'The native TUI owns presentation; --output-format must be stream-json.',
    )
  }
  if (
    options.inputFormat !== undefined &&
    options.inputFormat !== 'stream-json'
  ) {
    fail('The native TUI owns input; --input-format must be stream-json.')
  }
}

/**
 * Preserve the established interactive `--file` ingress path. The native TUI
 * replaces presentation only: the backend still downloads each startup
 * resource through the existing authenticated Files API into
 * `{cwd}/{sessionId}/uploads` (the path policy is owned by filesApi.ts).
 *
 * Start the work before the rest of setup, then await it immediately before
 * StructuredIO begins. This retains the legacy overlap without allowing the
 * first user turn to race files that were supplied as startup context.
 */
function startSessionFileDownloads(
  options: TuiRuntimeOptions,
): Promise<DownloadResult[]> | undefined {
  if (!options.file || options.file.length === 0) return undefined

  const sessionToken = getSessionIngressAuthToken()
  if (!sessionToken) {
    fail(
      'Session token required for file downloads. CRABCODE_SESSION_ACCESS_TOKEN must be set.',
    )
  }

  const files = parseFileSpecs(options.file)
  if (files.length === 0) return undefined

  const config: FilesApiConfig = {
    baseUrl: process.env.ACOSMI_BASE_URL || getOauthConfig().BASE_API_URL,
    oauthToken: sessionToken,
    sessionId: process.env.CRABCODE_REMOTE_SESSION_ID || getSessionId(),
  }
  return downloadSessionFiles(files, config)
}

async function awaitSessionFileDownloads(
  downloads: Promise<DownloadResult[]> | undefined,
): Promise<void> {
  if (!downloads) return
  try {
    const results = await downloads
    const failedCount = results.filter(result => !result.success).length
    if (failedCount > 0) {
      process.stderr.write(
        `Warning: ${failedCount}/${results.length} file(s) failed to download.\n`,
      )
    }
  } catch (error) {
    fail(`Error downloading files: ${errorMessage(error)}`)
  }
}

function appendPrompt(
  existing: string | undefined,
  addition: string | undefined,
): string | undefined {
  if (!addition) return existing
  return existing ? `${existing}\n\n${addition}` : addition
}

function configureSessionModes(options: TuiRuntimeOptions): void {
  if (options.goal || options.coordinator) {
    if (!feature('COORDINATOR_MODE')) {
      fail('--goal is not available in this build')
    }
    process.env.CRABCODE_COORDINATOR_MODE = '1'
  }
  const hasTranscriptClassifier = feature('TRANSCRIPT_CLASSIFIER')
  if (options.enableAutoMode && !hasTranscriptClassifier) {
    fail('--enable-auto-mode is not available in this build')
  }
  if (
    hasTranscriptClassifier &&
    (options.enableAutoMode ||
      options.permissionMode === 'auto' ||
      isDefaultPermissionModeAuto())
  ) {
    setAutoModeFlagCli(true)
  }
  if (options.proactive && !(feature('PROACTIVE') || feature('KAIROS'))) {
    fail('--proactive is not available in this build')
  }
  if (options.brief && !(feature('KAIROS') || feature('KAIROS_BRIEF'))) {
    fail('--brief is not available in this build')
  }
}

function parseChannelEntries(
  raw: readonly string[],
  flag: string,
): ChannelEntry[] {
  const entries: ChannelEntry[] = []
  const invalid: string[] = []
  for (const value of raw) {
    if (value.startsWith('plugin:')) {
      const identifier = value.slice('plugin:'.length)
      const separator = identifier.indexOf('@')
      if (separator <= 0 || separator === identifier.length - 1) {
        invalid.push(value)
      } else {
        entries.push({
          kind: 'plugin',
          name: identifier.slice(0, separator),
          marketplace: identifier.slice(separator + 1),
        })
      }
    } else if (value.startsWith('server:') && value.length > 'server:'.length) {
      entries.push({ kind: 'server', name: value.slice('server:'.length) })
    } else {
      invalid.push(value)
    }
  }
  if (invalid.length > 0) {
    fail(
      `${flag} entries must use plugin:<name>@<marketplace> or server:<name>: ${invalid.join(', ')}`,
    )
  }
  return entries
}

function configureChannels(options: TuiRuntimeOptions): void {
  if (!options.channels || options.channels.length === 0) return
  if (!(feature('KAIROS') || feature('KAIROS_CHANNELS'))) {
    fail('--channels is not available in this build')
  }
  setAllowedChannels(parseChannelEntries(options.channels, '--channels'))
}

function configureDevelopmentChannels(
  options: TuiRuntimeOptions,
): ChannelEntry[] {
  const raw = options.dangerouslyLoadDevelopmentChannels ?? []
  if (raw.length === 0) return []
  if (!(feature('KAIROS') || feature('KAIROS_CHANNELS'))) {
    fail(
      '--dangerously-load-development-channels is not available in this build',
    )
  }
  return parseChannelEntries(raw, '--dangerously-load-development-channels')
}

function configureTeammate(
  options: TuiRuntimeOptions,
): TeammateOptions | undefined {
  const teammate = extractTeammateOptions(options)
  const hasAnyIdentity = Boolean(
    teammate.agentId || teammate.agentName || teammate.teamName,
  )
  const hasAllIdentity = Boolean(
    teammate.agentId && teammate.agentName && teammate.teamName,
  )
  if (hasAnyIdentity && !hasAllIdentity) {
    fail('--agent-id, --agent-name, and --team-name must be provided together')
  }
  const hasTeammateOption = Boolean(
    hasAnyIdentity ||
      teammate.agentColor ||
      teammate.planModeRequired ||
      teammate.parentSessionId ||
      teammate.teammateMode ||
      teammate.agentType,
  )
  if (!hasTeammateOption) return undefined
  if (!isAgentSwarmsEnabled()) {
    fail(
      'Teammate identity options require the agent-swarms capability for this session.',
    )
  }
  if (teammate.agentId && teammate.agentName && teammate.teamName) {
    setDynamicTeamContext({
      agentId: teammate.agentId,
      agentName: teammate.agentName,
      teamName: teammate.teamName,
      color: teammate.agentColor,
      planModeRequired: teammate.planModeRequired ?? false,
      parentSessionId: teammate.parentSessionId,
    })
  }
  if (teammate.teammateMode) {
    setCliTeammateModeOverride(teammate.teammateMode)
  }
  return teammate
}

function resolveAdvisorModel(
  options: TuiRuntimeOptions,
  effectiveModel: string | undefined,
): string | undefined {
  if (options.advisor && !canUserConfigureAdvisor()) {
    fail('--advisor is not available for this account or build.')
  }
  if (!isAdvisorEnabled()) return undefined
  const advisorOption = canUserConfigureAdvisor()
    ? (options.advisor ?? getInitialAdvisorSetting())
    : undefined
  if (!advisorOption) return undefined
  const resolvedBaseModel = parseUserSpecifiedModel(
    effectiveModel ?? getDefaultMainLoopModel(),
  )
  if (!modelSupportsAdvisor(resolvedBaseModel)) {
    fail(`The model "${resolvedBaseModel}" does not support the advisor tool.`)
  }
  const normalizedAdvisorModel = normalizeModelStringForAPI(
    parseUserSpecifiedModel(advisorOption),
  )
  if (!isValidAdvisorModel(normalizedAdvisorModel)) {
    fail(`The model "${advisorOption}" cannot be used as an advisor.`)
  }
  return advisorOption
}

async function readExclusivePromptOption(
  inline: string | undefined,
  file: string | undefined,
  label: string,
): Promise<string | undefined> {
  if (inline !== undefined && file !== undefined) {
    fail(`Cannot use both ${label} and ${label}-file`)
  }
  if (file === undefined) return inline
  try {
    return await readFile(resolve(file), 'utf8')
  } catch (error) {
    fail(
      `Unable to read ${label}-file ${resolve(file)}: ${errorMessage(error)}`,
    )
  }
}

function parseDynamicMcpConfigs(
  rawConfigs: string[],
): Record<string, ScopedMcpServerConfig> {
  let merged: Record<string, McpServerConfig> = {}
  const errors: string[] = []
  for (const raw of rawConfigs.map(value => value.trim()).filter(Boolean)) {
    const parsed = safeParseJSON(raw)
    const result = parsed
      ? parseMcpConfig({
          configObject: parsed,
          filePath: 'command line',
          expandVars: true,
          scope: 'dynamic',
        })
      : parseMcpConfigFromFilePath({
          filePath: resolve(raw),
          expandVars: true,
          scope: 'dynamic',
        })
    if (!result.config) {
      errors.push(
        ...result.errors.map(
          error => `${error.path ? `${error.path}: ` : ''}${error.message}`,
        ),
      )
      continue
    }
    merged = { ...merged, ...result.config.mcpServers }
  }
  if (errors.length > 0) {
    fail(`Invalid MCP configuration:\n${errors.join('\n')}`)
  }

  return Object.fromEntries(
    Object.entries(merged).map(([name, config]) => [
      name,
      { ...config, scope: 'dynamic' as const },
    ]),
  ) as Record<string, ScopedMcpServerConfig>
}

function resolveThinkingConfig(options: TuiRuntimeOptions): ThinkingConfig {
  if (options.thinking === 'disabled') return { type: 'disabled' }
  if (options.thinking === 'adaptive' || options.thinking === 'enabled') {
    return { type: 'adaptive' }
  }
  const envValue = process.env.MAX_THINKING_TOKENS
  const parsedEnv = envValue === undefined ? undefined : Number(envValue)
  const maxTokens =
    parsedEnv !== undefined && Number.isFinite(parsedEnv)
      ? parsedEnv
      : options.maxThinkingTokens
  if (maxTokens !== undefined) {
    return maxTokens > 0
      ? { type: 'enabled', budgetTokens: maxTokens }
      : { type: 'disabled' }
  }
  return shouldEnableThinkingByDefault()
    ? { type: 'adaptive' }
    : { type: 'disabled' }
}

async function connectMcpConfigs(
  store: ReturnType<typeof createStore<AppState>>,
  configs: Record<string, ScopedMcpServerConfig>,
  label: string,
): Promise<void> {
  if (Object.keys(configs).length === 0) return
  store.setState(previous => ({
    ...previous,
    mcp: {
      ...previous.mcp,
      clients: [
        ...previous.mcp.clients,
        ...Object.entries(configs).map(([name, config]) => ({
          name,
          type: 'pending' as const,
          config,
        })),
      ],
    } as MutableMcpState,
  }))
  await getMcpToolsCommandsAndResources(({ client, tools, commands }) => {
    store.setState(previous => ({
      ...previous,
      mcp: {
        ...previous.mcp,
        clients: previous.mcp.clients.some(item => item.name === client.name)
          ? previous.mcp.clients.map(item =>
              item.name === client.name ? client : item,
            )
          : [...previous.mcp.clients, client],
        tools: uniqBy([...previous.mcp.tools, ...tools], 'name'),
        commands: uniqBy([...previous.mcp.commands, ...commands], 'name'),
      } as MutableMcpState,
    }))
  }, configs).catch(error => {
    logForDebugging(`[MCP] ${label} connect error: ${errorMessage(error)}`, {
      level: 'warn',
    })
  })
}

function suppressDuplicatePluginServers(
  store: ReturnType<typeof createStore<AppState>>,
  regular: Record<string, ScopedMcpServerConfig>,
  connectorConfigs: Record<string, ScopedMcpServerConfig>,
): void {
  const connectorSignatures = new Set(
    Object.values(connectorConfigs)
      .map(getMcpServerSignature)
      .filter((value): value is string => value !== null),
  )
  const suppressed = new Set(
    Object.entries(regular)
      .filter(([name, config]) => {
        const signature = getMcpServerSignature(config)
        return (
          name.startsWith('plugin:') &&
          signature !== null &&
          connectorSignatures.has(signature)
        )
      })
      .map(([name]) => name),
  )
  if (suppressed.size === 0) return

  for (const client of store.getState().mcp.clients) {
    if (
      suppressed.has(client.name) &&
      client.type === 'connected' &&
      client.client
    ) {
      client.client.onclose = undefined
      void clearServerCache(client.name, client.config).catch(() => {})
    }
  }
  store.setState(previous => {
    let commands = previous.mcp.commands
    let resources = previous.mcp.resources
    for (const name of suppressed) {
      commands = excludeCommandsByServer(commands, name)
      resources = excludeResourcesByServer(resources, name)
    }
    return {
      ...previous,
      mcp: {
        ...previous.mcp,
        clients: previous.mcp.clients.filter(
          client => !suppressed.has(client.name),
        ),
        tools: previous.mcp.tools.filter(
          tool => !tool.mcpInfo || !suppressed.has(tool.mcpInfo.serverName),
        ),
        commands,
        resources,
      } as MutableMcpState,
    }
  })
}

export async function runTuiRuntime(
  options: TuiRuntimeOptions,
  rendererSession: NativeTuiRendererSession,
): Promise<void> {
  installDirectTuiCommandSurface()
  installDirectTuiCronEnsureProvider()
  installLocalModelServerStatusProvider(() =>
    createDefaultLocalModelDirectClient().serverStatus(),
  )
  validateDirectRuntimeSurface(options)
  // The legacy spelling is deprecated but must not be an accepted no-op.
  // Preserve its documented diagnostic effect by treating it as --debug.
  if (options.mcpDebug) enableDebugLogging()
  // The Rust renderer talks StructuredIO over pipes, but the product session
  // is still the interactive CLI. Keep product semantics independent from
  // transport framing: auth selection, hooks, tools, prompts, error recovery,
  // memory paths and activity-aware housekeeping all read this global bit
  // before QueryEngine exists.
  setIsInteractive(true)
  setClientType('cli')
  setUsesStructuredIoTransport(true)
  // SessionStart may begin before queryExecutionCore joins its promise. Put
  // the process-global Hook event bus into discard mode before that work can
  // start, so neither a stale handler nor the pending queue can expose Hook
  // lifecycle/output to the native renderer. Hook result messages still flow
  // through the unchanged SessionStart backend path.
  suppressHookEventDelivery()
  const previewFormat = process.env.CRABCODE_QUESTION_PREVIEW_FORMAT
  setQuestionPreviewFormat(
    previewFormat === 'html' || previewFormat === 'markdown'
      ? previewFormat
      : undefined,
  )
  installDirectTuiManagedSettingsSecurityReview()
  eagerLoadSettings()
  await init()
  const fileDownloadPromise = startSessionFileDownloads(options)
  await loadRemoteManagedSettings()
  void loadPolicyLimits()
  configureSessionModes(options)
  configureChannels(options)
  const devChannels = configureDevelopmentChannels(options)
  const teammateOptions = configureTeammate(options)
  const taskListId = options.tasks
    ? typeof options.tasks === 'string'
      ? options.tasks
      : DEFAULT_TASKS_MODE_TASK_LIST_ID
    : undefined
  if (taskListId) process.env.CRABCODE_TASK_LIST_ID = taskListId
  // Match the established renderer behavior: discovery is asynchronous and
  // must not hold the native TUI's first frame for the 30s IDE polling window.
  const ideMcpConfigPromise = resolveIdeAutoConnectMcpConfig(options.ide)

  if (options.pluginDir && options.pluginDir.length > 0) {
    setInlinePlugins(options.pluginDir)
    clearPluginCache('native TUI --plugin-dir')
  }
  if (options.includeHookEvents || isEnvTruthy(process.env.CRABCODE_REMOTE)) {
    setAllHookEventsEnabled(true)
  }

  const allowedTools = [...(options.allowedTools ?? [])]
  const addDirs = options.addDir ?? []
  setAdditionalDirectoriesForCrabcodeMd(addDirs)
  const { mode: permissionMode } = initialPermissionModeFromCLI({
    permissionModeCli: options.permissionMode,
    dangerouslySkipPermissions: options.dangerouslySkipPermissions,
  })
  setSessionBypassPermissionsMode(permissionMode === 'bypassPermissions')
  const permission = await initializeToolPermissionContext({
    allowedToolsCli: allowedTools,
    disallowedToolsCli: options.disallowedTools ?? [],
    baseToolsCli: options.tools,
    permissionMode,
    allowDangerouslySkipPermissions:
      options.allowDangerouslySkipPermissions ?? false,
    addDirs,
  })
  for (const warning of permission.warnings) {
    process.stderr.write(`${warning}\n`)
  }

  let systemPrompt = await readExclusivePromptOption(
    options.systemPrompt,
    options.systemPromptFile,
    '--system-prompt',
  )
  let appendSystemPrompt = await readExclusivePromptOption(
    options.appendSystemPrompt,
    options.appendSystemPromptFile,
    '--append-system-prompt',
  )
  if (
    teammateOptions?.agentId &&
    teammateOptions.agentName &&
    teammateOptions.teamName
  ) {
    appendSystemPrompt = appendPrompt(
      appendSystemPrompt,
      TEAMMATE_SYSTEM_PROMPT_ADDENDUM,
    )
  }

  const dynamicMcpConfig = parseDynamicMcpConfigs(options.mcpConfig ?? [])
  const rawDynamicProcessTransport = filterDirectTuiProcessMcpServers(
    dynamicMcpConfig,
  )
  for (const [name, reason] of Object.entries(
    rawDynamicProcessTransport.errors,
  )) {
    process.stderr.write(`Warning: Ignoring MCP server "${name}": ${reason}\n`)
  }
  const rawDynamicProcessMcpConfigs = rawDynamicProcessTransport.accepted
  const dynamicMcpPolicyView = filterMcpServersByPolicy(dynamicMcpConfig)
  const dynamicProcessPolicyView = filterMcpServersByPolicy(
    rawDynamicProcessMcpConfigs,
  )
  // This mutable record is the direct session's fixed-owner desired source.
  // IDE discovery may append after the query loop starts; the consumer reads
  // the same record at each settings reconciliation rather than a stale copy.
  const fixedSessionMcpConfigs: Record<string, ScopedMcpServerConfig> = {
    ...rawDynamicProcessMcpConfigs,
  }
  const initiallyInactiveFixedMcpNames = new Set(
    Object.keys(rawDynamicProcessMcpConfigs).filter(isMcpServerDisabled),
  )
  if (doesEnterpriseMcpConfigExist()) {
    if (options.strictMcpConfig) {
      fail(
        '--strict-mcp-config cannot be used when enterprise MCP configuration is present',
      )
    }
    if (
      !areMcpConfigsAllowedWithEnterpriseMcpConfig(
        dynamicMcpPolicyView.allowed,
      )
    ) {
      fail(
        'dynamic MCP servers are not allowed with enterprise MCP configuration',
      )
    }
  }

  if (
    options.sessionId &&
    (options.continue || options.resume) &&
    !options.forkSession
  ) {
    fail('--session-id requires --fork-session with --continue or --resume')
  }
  const customSessionId = options.sessionId
    ? validateUuid(options.sessionId)
    : undefined
  if (options.sessionId && !customSessionId) {
    fail('--session-id must be a valid UUID')
  }
  if (options.tmux && !options.worktree) {
    fail('--tmux requires --worktree')
  }

  initBuiltinPlugins()
  initBundledSkills()
  const preSetupCwd = getCwd()
  await setup(
    preSetupCwd,
    permissionMode,
    options.allowDangerouslySkipPermissions ?? false,
    options.worktree !== undefined,
    typeof options.worktree === 'string' ? options.worktree : undefined,
    options.tmux ?? false,
    customSessionId,
    undefined,
    options.messagingSocketPath,
  )
  // setup() can select a different authoritative cwd for --worktree. Bind
  // after that transition so renderer_context and workspace_trust describe
  // the same final workspace without inventing a cwd-rebind control message.
  await rendererSession.bindRendererContext()
  const { onboardingShown } = await runDirectTuiPreTrustOnboarding(
    rendererSession.requestSetup,
  )
  const setupScreensSkipped =
    process.env.NODE_ENV === 'test' || isEnvTruthy(process.env.IS_DEMO)
  if (!setupScreensSkipped && !isEnvTruthy(process.env.CLAUBBIT)) {
    await rendererSession.ensureWorkspaceTrust(getCwd())
    resetGrowthBook()
    void initializeGrowthBook()
    void getSystemContext()
  }
  await runDirectTuiPostTrustSetup(
    rendererSession.requestSetup,
    {
      permissionMode,
      allowDangerouslySkipPermissions:
        options.allowDangerouslySkipPermissions ?? false,
      devChannels,
    },
    onboardingShown,
    async () => {
      void updateGithubRepoPathMapping()
      if (feature('LODESTONE')) updateDeepLinkTerminalPreference()
      applyConfigEnvironmentVariables()
      await rendererSession.projectRendererScrollSpeed(
        process.env.CRABCODE_SCROLL_SPEED,
      )
      setImmediate(() => initializeTelemetryAfterTrust())
    },
  )
  await runDirectTuiStartupSessionPicker(
    options,
    rendererSession.requestSetup,
  )
  const runtimeInput = await rendererSession.finishSetup()
  const currentCwd = getCwd()
  const [commands, loadedAgents] = await Promise.all([
    options.disableSlashCommands
      ? Promise.resolve([])
      : getDirectTuiCommands(currentCwd),
    getAgentDefinitionsWithOverrides(currentCwd),
  ])
  let cliAgents: typeof loadedAgents.activeAgents = []
  if (options.agents) {
    const parsed = safeParseJSON(options.agents)
    if (!parsed) fail('--agents must be a JSON object')
    cliAgents = parseAgentsFromJson(parsed, 'flagSettings')
  }
  const allAgents = [...loadedAgents.allAgents, ...cliAgents]
  const agents = {
    allAgents,
    activeAgents: getActiveAgentsFromList(allAgents),
  }
  const agentName = options.agent ?? getInitialSettings().agent
  const mainThreadAgentDefinition = agentName
    ? agents.activeAgents.find(agent => agent.agentType === agentName)
    : undefined
  if (agentName && !mainThreadAgentDefinition) {
    process.stderr.write(
      `Warning: agent "${agentName}" was not found; using the default agent\n`,
    )
  }
  setMainThreadAgentType(mainThreadAgentDefinition?.agentType)

  if (teammateOptions?.agentType) {
    const teammateAgentDefinition = agents.activeAgents.find(
      agent => agent.agentType === teammateOptions.agentType,
    )
    if (!teammateAgentDefinition) {
      process.stderr.write(
        `Warning: teammate agent "${teammateOptions.agentType}" was not found\n`,
      )
    } else if (!isBuiltInAgent(teammateAgentDefinition)) {
      appendSystemPrompt = appendPrompt(
        appendSystemPrompt,
        `# Custom Agent Instructions\n${teammateAgentDefinition.getSystemPrompt()}`,
      )
    }
  }

  let effectiveModel =
    options.model === 'default' ? getDefaultMainLoopModel() : options.model
  if (
    !effectiveModel &&
    mainThreadAgentDefinition?.model &&
    mainThreadAgentDefinition.model !== 'inherit'
  ) {
    effectiveModel = parseUserSpecifiedModel(mainThreadAgentDefinition.model)
  }
  setMainLoopModelOverride(effectiveModel)
  setInitialMainLoopModel(getUserSpecifiedModelSetting() || null)
  void getInitialMainLoopModel()
  const advisorModel = resolveAdvisorModel(options, effectiveModel)

  if (options.name?.trim()) cacheSessionTitle(options.name.trim())
  if (options.sessionPersistence === false) {
    setSessionPersistenceDisabled(true)
  }
  setSdkBetas(filterAllowedSdkBetas(options.betas ?? []))

  maybeActivateBrief(options)
  maybeActivateProactive(options)
  const effectiveToolPermissionContext =
    isAgentSwarmsEnabled() && isPlanModeRequired()
      ? { ...permission.toolPermissionContext, mode: 'plan' as const }
      : permission.toolPermissionContext
  let tools = getTools(effectiveToolPermissionContext)
  let jsonSchema: Record<string, unknown> | undefined
  if (options.jsonSchema) {
    const parsed = safeParseJSON(options.jsonSchema)
    if (!parsed || typeof parsed !== 'object') {
      fail('--json-schema must be a JSON object')
    }
    jsonSchema = parsed as Record<string, unknown>
    if (isSyntheticOutputToolEnabled({ isNonInteractiveSession: true })) {
      const synthetic = createSyntheticOutputTool(jsonSchema)
      if ('tool' in synthetic) tools = [...tools, synthetic.tool]
    }
  }

  const existing =
    options.strictMcpConfig || isBareMode()
      ? {}
      : (await getCrabCodeMcpConfigs(dynamicMcpPolicyView.allowed)).servers
  const runtimeActiveMcpConfigs = Object.fromEntries(
    [
      ...Object.entries(dynamicMcpPolicyView.allowed),
      ...Object.entries(orderMcpServersByPrecedence(existing)).filter(
        ([name]) => !(name in rawDynamicProcessMcpConfigs),
      ),
    ].filter(
      ([name, config]) => isMcpServerRuntimeActive(name, config),
    ),
  )
  // Native direct TUI has no SDK MCP control host. Reject SDK transports before
  // assigning namespaces so an unsupported config cannot shadow a healthy
  // process server that normalizes to the same wire namespace.
  const processTransportConfigs = filterDirectTuiProcessMcpServers(
    runtimeActiveMcpConfigs,
  )
  for (const [name, reason] of Object.entries(processTransportConfigs.errors)) {
    if (name in rawDynamicProcessTransport.errors) continue
    process.stderr.write(`Warning: Ignoring MCP server "${name}": ${reason}\n`)
  }
  // --mcp-config is not part of getCrabCodeMcpConfigs()' returned server map,
  // so the direct bootstrap must apply the same managed policy itself before
  // a blocked name can claim a namespace or create a transport.
  const startupPolicy = filterMcpServersByPolicy(
    processTransportConfigs.accepted,
  )
  const startupBlockedNames = new Set([
    ...startupPolicy.blocked,
    ...dynamicProcessPolicyView.blocked,
  ])
  for (const name of startupBlockedNames) {
    process.stderr.write(
      `Warning: Ignoring MCP server "${name}": Blocked by enterprise policy (allowedMcpServers/deniedMcpServers)\n`,
    )
    if (dynamicProcessPolicyView.blocked.includes(name)) {
      initiallyInactiveFixedMcpNames.add(name)
    }
  }
  // Raw names share one MCP wire namespace across tools and prompts. Reject
  // later collisions before any process transport connects so one server can
  // never erase another server's projected capabilities.
  const startupNamespaceOwnership = admitStartupMcpNamespaceReservations(
    rawDynamicProcessMcpConfigs,
    startupPolicy.allowed,
  )
  for (const [name, reason] of Object.entries(
    startupNamespaceOwnership.errors,
  )) {
    process.stderr.write(`Warning: Ignoring MCP server "${name}": ${reason}\n`)
  }
  const allMcpConfigs = startupNamespaceOwnership.acceptedActive
  for (const name of Object.keys(rawDynamicProcessMcpConfigs)) {
    if (
      !startupNamespaceOwnership.acceptedReservationNames.has(name)
    ) {
      // A namespace-rejected CLI row never owned this session and must not
      // become a ghost protected owner during later reconciliation.
      delete fixedSessionMcpConfigs[name]
      initiallyInactiveFixedMcpNames.delete(name)
    }
  }
  // Kept for the shared standard-query signature. Direct startup rejects every
  // SDK transport above, so this record is intentionally always empty.
  const sdkMcpConfigs: Record<string, McpSdkServerConfig> = {}
  const regularMcpConfigs: Record<string, ScopedMcpServerConfig> = {}
  for (const [name, config] of Object.entries(allMcpConfigs)) {
    if (config.type !== 'sdk') regularMcpConfigs[name] = config
  }

  const defaultState = getDefaultAppState()
  const initialState: AppState = {
    ...defaultState,
    mcp: {
      ...defaultState.mcp,
      clients: [],
      tools: [],
      commands: [],
    },
    toolPermissionContext: effectiveToolPermissionContext,
    effortValue: parseEffortValue(options.effort) ?? getInitialEffortSetting(),
    ...(isFastModeEnabled()
      ? { fastMode: getInitialFastModeSetting(effectiveModel ?? null) }
      : {}),
    ...(advisorModel ? { advisorModel } : {}),
  }
  const store = createStore(initialState, onChangeAppState)
  if (
    effectiveToolPermissionContext.mode === 'bypassPermissions' ||
    options.allowDangerouslySkipPermissions
  ) {
    void checkAndDisableBypassPermissions(effectiveToolPermissionContext)
  }
  if (feature('TRANSCRIPT_CLASSIFIER')) {
    void verifyAutoModeGateAccess(
      effectiveToolPermissionContext,
      store.getState().fastMode,
    ).then(({ updateContext }) => {
      store.setState(previous => {
        const nextContext = updateContext(previous.toolPermissionContext)
        return nextContext === previous.toolPermissionContext
          ? previous
          : { ...previous, toolPermissionContext: nextContext }
      })
    })
  }
  await connectMcpConfigs(store, regularMcpConfigs, 'regular')

  if (
    !options.strictMcpConfig &&
    !isBareMode() &&
    !doesEnterpriseMcpConfigExist()
  ) {
    const fetched = (await fetchAcosmiMcpConfigsIfEligible()) as Record<
      string,
      ScopedMcpServerConfig
    >
    const nonPlugin = Object.fromEntries(
      Object.entries(regularMcpConfigs).filter(
        ([name]) => !name.startsWith('plugin:'),
      ),
    )
    const { servers: dedupedFetched } = dedupAcosmiMcpServers(
      fetched,
      nonPlugin,
    )
    const { allowed, blocked } = filterMcpServersByPolicy(dedupedFetched)
    if (blocked.length > 0) {
      process.stderr.write(
        `Warning: Acosmi MCP servers blocked by enterprise policy: ${blocked.join(', ')}\n`,
      )
    }
    const inactive = Object.fromEntries(
      blocked.flatMap(name => {
        const config = dedupedFetched[name]
        return config ? [[name, config] as const] : []
      }),
    )
    const runtimeAllowed = Object.fromEntries(
      Object.entries(allowed).filter(([name, config]) => {
        if (isMcpServerRuntimeActive(name, config)) return true
        inactive[name] = config
        return false
      }),
    )
    const fixedOwnerNames = new Set([
      ...Object.keys(fixedSessionMcpConfigs),
      ...Object.keys(allMcpConfigs),
      ...store.getState().mcp.clients.map(client => client.name),
    ])
    const activeOwnership = filterMcpServersForOwner(
      'plugin',
      runtimeAllowed,
      fixedOwnerNames,
    )
    const inactiveOwnership = filterMcpServersForOwner(
      'plugin',
      inactive,
      new Set([
        ...fixedOwnerNames,
        ...Object.keys(activeOwnership.accepted),
      ]),
    )
    for (const [name, reason] of Object.entries({
      ...activeOwnership.errors,
      ...inactiveOwnership.errors,
    })) {
      process.stderr.write(
        `Warning: Ignoring Acosmi MCP server "${name}": ${reason}\n`,
      )
    }
    Object.assign(fixedSessionMcpConfigs, inactiveOwnership.accepted)
    for (const name of Object.keys(inactiveOwnership.accepted)) {
      initiallyInactiveFixedMcpNames.add(name)
    }
    suppressDuplicatePluginServers(
      store,
      regularMcpConfigs,
      activeOwnership.accepted,
    )
    Object.assign(fixedSessionMcpConfigs, activeOwnership.accepted)
    await connectMcpConfigs(store, activeOwnership.accepted, 'acosmi')
  }

  if (!isBareMode()) {
    // Preserve the established plugin-generation ordering. The exclusion
    // snapshot must observe GC's settled disk state, and the updater in the
    // direct lifecycle must not race version migration or orphan marking.
    await initializeVersionedPlugins()
    await cleanupOrphanedPluginVersionsInBackground()
    void getGlobExclusionsForPluginCache()
  }
  if (options.initOnly) {
    await processSetupHooks('init', { forceSyncExecution: true })
    await processSessionStartHooks('startup', { forceSyncExecution: true })
    return
  }
  const orgValidation = await validateForceLoginOrg()
  if (!orgValidation.valid) fail(orgValidation.message)

  const { startDirectTuiBackendLifecycle } = await import(
    '../services/memoryTierProxy/directTuiLifecycle.js'
  )
  startDirectTuiBackendLifecycle(options.name?.trim())

  const setupTrigger =
    options.initOnly || options.init
      ? 'init'
      : options.maintenance
        ? 'maintenance'
        : undefined
  const sessionStartHooksPromise =
    options.continue || options.resume || setupTrigger
      ? undefined
      : processSessionStartHooks('startup')
  sessionStartHooksPromise?.catch(logError)

  await awaitSessionFileDownloads(fileDownloadPromise)
  setHasFormattedOutput(true)
  await runHeadlessDirectTui(
    runtimeInput,
    store.getState,
    store.setState,
    commands,
    tools,
    sdkMcpConfigs,
    agents.activeAgents,
    {
      continue: options.continue,
      resume: options.resume,
      resumeSessionAt: options.resumeSessionAt,
      verbose: true,
      outputFormat: 'stream-json',
      jsonSchema,
      permissionPromptToolName: DIRECT_TUI_PERMISSION_PROMPT_TOOL_NAME,
      allowedTools,
      thinkingConfig: resolveThinkingConfig(options),
      maxTurns: options.maxTurns,
      maxBudgetUsd: options.maxBudgetUsd,
      taskBudget:
        options.taskBudget === undefined
          ? undefined
          : { total: options.taskBudget },
      systemPrompt,
      appendSystemPrompt,
      mainThreadAgentDefinition,
      userSpecifiedModel: effectiveModel,
      fallbackModel:
        options.fallbackModel === 'default'
          ? getDefaultMainLoopModel()
          : (options.fallbackModel ?? getDefaultFallbackModel()),
      replayUserMessages: options.replayUserMessages,
      includePartialMessages: options.includePartialMessages,
      includeHookEvents:
        options.includeHookEvents || isEnvTruthy(process.env.CRABCODE_REMOTE),
      forkSession: options.forkSession,
      rewindFiles: options.rewindFiles,
      enableAuthStatus: options.enableAuthStatus,
      agent: options.agent,
      workload: options.workload,
      disableSlashCommands: options.disableSlashCommands,
      subscribeAppState: store.subscribe,
      startupSessionMcpServerNames: Object.keys(fixedSessionMcpConfigs),
      startupSessionMcpServers: fixedSessionMcpConfigs,
      startupPolicyBlockedMcpServerNames: initiallyInactiveFixedMcpNames,
      lateFixedMcpConfig: ideMcpConfigPromise.then(config =>
        config ? { name: 'ide', config } : null,
      ),
      taskListId,
      setupTrigger,
      sessionStartHooksPromise,
    },
  )
}
