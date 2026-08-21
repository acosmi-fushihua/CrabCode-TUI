// ANT-ONLY import markers must not be reordered
import { feature } from '../../utils/featurePolyfill.js'
import type { Command } from 'src/types/command.js'
import {
  clearHeadlessCommandMemoizationCaches,
  formatHeadlessCommandDescription,
} from 'src/cli/headlessCommands.js'
import {
  projectCommandCatalogEntries,
  projectDirectTuiCommandCatalogEntries,
} from 'src/cli/commandCatalogProjection.js'
import type { ToolPermissionContext } from 'src/Tool.js'
import {
  type AgentDefinition,
  isBuiltInAgent,
  parseAgentsFromJson,
} from 'src/tools/AgentTool/loadAgentsDir.js'
import type { StructuredIO } from 'src/cli/structuredIO.js'
import type {
  SDKControlInitializeRequest,
  SDKControlInitializeResponse,
  StdoutMessage,
} from 'src/entrypoints/sdk/controlTypes.js'
import type {
  ModelInfo,
  RewindFilesResult,
} from 'src/entrypoints/agentSdkTypes.js'
import type { PermissionMode as InternalPermissionMode } from 'src/types/permissions.js'
import type { AppState } from 'src/state/AppStateStore.js'
import type { MCPServerConnection } from 'src/services/mcp/types.js'
import type {
  AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
} from 'src/services/analytics/index.js'
import { logEvent } from 'src/services/analytics/index.js'
import { logMCPDebug } from 'src/utils/log.js'
import { logForDebugging } from 'src/utils/debug.js'
import {
  getSettings_DEPRECATED,
} from 'src/utils/settings/settings.js'
import {
  DEFAULT_OUTPUT_STYLE_NAME,
  getAllOutputStyles,
} from 'src/constants/outputStyles.js'
import { getCwd } from 'src/utils/cwd.js'
import { getAccountInformation } from 'src/utils/auth.js'
import { getAPIProvider } from 'src/utils/model/providers.js'
import { parseUserSpecifiedModel } from 'src/utils/model/model.js'
import {
  isFastModeAvailable,
  isFastModeEnabled,
  getFastModeState,
} from 'src/utils/fastMode.js'
import {
  isAutoModeGateEnabled,
  getAutoModeUnavailableNotification,
  getAutoModeUnavailableReason,
  isBypassPermissionsModeDisabled,
  transitionPermissionMode,
} from 'src/utils/permissions/permissionSetup.js'
import {
  fileHistoryRewind,
  fileHistoryCanRestore,
  fileHistoryEnabled,
  fileHistoryGetDiffStats,
} from 'src/utils/fileHistory.js'
import {
  getSessionId,
  setMainLoopModelOverride,
  setMainThreadAgentType,
  getAllowedChannels,
  setAllowedChannels,
  setQuestionPreviewFormat,
  type ChannelEntry,
} from 'src/bootstrap/state.js'
import {
  registerHookCallbacks,
  setInitJsonSchema,
} from 'src/bootstrap/state.js'
import { getMainThreadAgentType } from 'src/bootstrap/state.js'
import { randomUUID } from 'crypto'
import type { UUID } from 'crypto'
import type { HookCallbackMatcher } from 'src/types/hooks.js'
import type { HookEvent } from 'src/entrypoints/agentSdkTypes.js'
import { AwsAuthStatusManager } from 'src/utils/awsAuthStatusManager.js'
import { errorMessage } from 'src/utils/errors.js'
import { performLogout } from 'src/services/auth/logout.js'
import { AcosmiAccountRemovalCommittedCleanupError } from 'src/services/auth/localAuthState.js'
import {
  ChannelMessageNotificationSchema,
  gateChannelServer,
  wrapChannelMessage,
  findChannelEntry,
} from 'src/services/mcp/channelNotification.js'
import {
  isChannelAllowlisted,
  isChannelsEnabled,
} from 'src/services/mcp/channelAllowlist.js'
import { parsePluginIdentifier } from 'src/utils/plugins/pluginIdentifier.js'
import {
  enqueue,
} from 'src/utils/messageQueueManager.js'

type StdoutMessageSink = {
  enqueue(message: StdoutMessage): void
}

export type InitializeCommandCatalog =
  | readonly Command[]
  | (() => Promise<readonly Command[]>)

export type SDKControlAuthCatalogResponse = Pick<
  SDKControlInitializeResponse,
  'account' | 'commands'
>

/**
 * Replace only the command half of an authentication snapshot.
 *
 * Direct TUI authentication commits the base registry first, then reads the
 * live MCP inventory from AppState. Keeping this projection in the existing
 * control shape lets the correlated response carry the exact executable
 * catalog without changing the public authentication protocol.
 */
export function withCurrentControlAuthCommandCatalog(
  response: SDKControlAuthCatalogResponse,
  commands: readonly Command[],
  strictDirectTuiCatalog = false,
): SDKControlAuthCatalogResponse {
  return {
    ...response,
    commands: strictDirectTuiCatalog
      ? projectDirectTuiCommandCatalogEntries(
          commands,
          formatHeadlessCommandDescription,
        )
      : projectCommandCatalogEntries(
          commands,
          formatHeadlessCommandDescription,
        ),
  }
}

type AuthCatalogRefreshDependencies = {
  clearCommandCaches: () => void
  getAccount: typeof getAccountInformation
  getProvider: typeof getAPIProvider
}

type DirectTuiLogoutDependencies = AuthCatalogRefreshDependencies & {
  logout: typeof performLogout
}

type AuthCatalogSnapshot = {
  commands: Command[]
  response: SDKControlAuthCatalogResponse
}

const AUTH_CATALOG_REFRESH_DEPENDENCIES: AuthCatalogRefreshDependencies = {
  clearCommandCaches: clearHeadlessCommandMemoizationCaches,
  getAccount: getAccountInformation,
  getProvider: getAPIProvider,
}

const DIRECT_TUI_LOGOUT_DEPENDENCIES: DirectTuiLogoutDependencies = {
  ...AUTH_CATALOG_REFRESH_DEPENDENCIES,
  logout: performLogout,
}

/**
 * Rebuild the command projection after an authentication transition.
 *
 * The command loader is memoized, while availability checks such as
 * `crabcode-ai` depend on the active account. Clearing that memoization before
 * loading is therefore part of the auth-control response contract rather than
 * an optional presentation refresh.
 */
export async function refreshControlAuthCatalog(
  commandLoader: (cwd: string) => Promise<Command[]>,
  currentCwd: string,
  dependencies: AuthCatalogRefreshDependencies =
    AUTH_CATALOG_REFRESH_DEPENDENCIES,
  strictDirectTuiCatalog = false,
): Promise<AuthCatalogSnapshot> {
  dependencies.clearCommandCaches()
  const commands = await commandLoader(currentCwd)
  const accountInfo = dependencies.getAccount()
  return {
    commands,
    response: {
      commands: strictDirectTuiCatalog
        ? projectDirectTuiCommandCatalogEntries(
            commands,
            formatHeadlessCommandDescription,
          )
        : projectCommandCatalogEntries(
            commands,
            formatHeadlessCommandDescription,
          ),
      account: {
        email: accountInfo?.email,
        organization: accountInfo?.organization,
        subscriptionType: accountInfo?.subscription,
        tokenSource: accountInfo?.tokenSource,
        apiKeySource: accountInfo?.apiKeySource,
        apiProvider: dependencies.getProvider(),
      },
    },
  }
}

/**
 * Empty is the only catalog that is safe after credentials were cleared but
 * discovery could not be rebuilt. In particular, retaining the previous
 * snapshot would leave account-gated commands executable in TypeScript even
 * if the renderer had already presented a signed-out state.
 */
export function conservativeControlAuthCatalog(): AuthCatalogSnapshot {
  return {
    commands: [],
    response: {
      commands: [],
      account: {},
    },
  }
}

/**
 * Rebuild the post-logout snapshot without ever copying identity fields back
 * into the response. `getAccount` is still evaluated so a broken auth cache is
 * treated as a refresh failure, but a successful read is used only to detect
 * stale identity remnants. The renderer must observe a signed-out account
 * even if a lower layer briefly retains old profile data.
 */
async function refreshSignedOutControlAuthCatalog(
  commandLoader: (cwd: string) => Promise<Command[]>,
  currentCwd: string,
  dependencies: AuthCatalogRefreshDependencies,
): Promise<AuthCatalogSnapshot> {
  dependencies.clearCommandCaches()
  const commands = await commandLoader(currentCwd)
  const accountInfo = dependencies.getAccount()
  if (
    accountInfo?.email !== undefined ||
    accountInfo?.organization !== undefined ||
    accountInfo?.subscription !== undefined ||
    (accountInfo?.tokenSource !== undefined &&
      accountInfo.tokenSource !== 'none') ||
    (accountInfo?.apiKeySource !== undefined &&
      accountInfo.apiKeySource !== 'none')
  ) {
    throw new Error('logout refresh retained account identity')
  }
  const apiProvider = dependencies.getProvider()
  return {
    commands,
    response: {
      commands: projectDirectTuiCommandCatalogEntries(
        commands,
        formatHeadlessCommandDescription,
      ),
      account: { apiProvider },
    },
  }
}

/**
 * Own the process-private native-TUI logout handshake without terminating the
 * StructuredIO child. Ordinary print/SDK sessions are rejected before any
 * credential mutation, preserving their existing command lifecycle.
 */
export async function handleDirectTuiLogoutRequest(
  requestId: string,
  output: StdoutMessageSink,
  commandLoader: (cwd: string) => Promise<Command[]>,
  currentCwd: string,
  interactiveProductSession: boolean,
  dependencies: DirectTuiLogoutDependencies =
    DIRECT_TUI_LOGOUT_DEPENDENCIES,
  commitCommands?: (commands: readonly Command[]) => unknown,
  getCommittedCatalogCommands?: () => readonly Command[],
): Promise<Command[] | null> {
  if (!interactiveProductSession) {
    output.enqueue({
      type: 'control_response',
      response: {
        subtype: 'error',
        request_id: requestId,
        error: 'crabcode_tui_logout is available only to the direct TUI runtime',
      },
    })
    return null
  }

  // Phase one is the only fallible phase allowed to report logout failure.
  // Once this resolves (or the typed secure-storage commit signal is caught),
  // Acosmi account removal is authoritative and must never be misrepresented
  // as a failed logout merely because later projection/output work fails.
  let committedCleanupWarning: AcosmiAccountRemovalCommittedCleanupError | null =
    null
  try {
    await dependencies.logout({ clearOnboarding: true })
  } catch (error) {
    if (error instanceof AcosmiAccountRemovalCommittedCleanupError) {
      committedCleanupWarning = error
    } else {
      output.enqueue({
        type: 'control_response',
        response: {
          subtype: 'error',
          request_id: requestId,
          error: errorMessage(error),
        },
      })
      return null
    }
  }

  let refreshed: AuthCatalogSnapshot
  if (committedCleanupWarning) {
    // Secure storage proved the credential mutation committed, but one of the
    // later local cleanup steps rejected. Do not consult any potentially stale
    // account/cache projection in this state.
    refreshed = conservativeControlAuthCatalog()
    logForDebugging(
      `[direct-tui] Acosmi account removal committed but later logout cleanup degraded safely: ${committedCleanupWarning.message}`,
      { level: 'warn' },
    )
  } else {
    try {
      refreshed = await refreshSignedOutControlAuthCatalog(
        commandLoader,
        currentCwd,
        dependencies,
      )
    } catch (error) {
      refreshed = conservativeControlAuthCatalog()
      logForDebugging(
        `[direct-tui] credentials were cleared but the signed-out auth/catalog refresh degraded safely: ${errorMessage(error)}`,
        { level: 'warn' },
      )
    }
  }

  // Commit before publishing the correlated response. The production
  // lifecycle replacement is synchronous and non-throwing; violating that
  // internal invariant must fail closed instead of acknowledging signed-out
  // state while TypeScript can still dispatch the old catalog.
  commitCommands?.(refreshed.commands)

  try {
    const response = getCommittedCatalogCommands
      ? withCurrentControlAuthCommandCatalog(
          refreshed.response,
          getCommittedCatalogCommands(),
          true,
        )
      : refreshed.response
    output.enqueue({
      type: 'control_response',
      response: {
        subtype: 'success',
        request_id: requestId,
        response,
      },
    })
  } catch (error) {
    // Do not enqueue an error response here: credentials are already gone and
    // claiming otherwise could prompt the user to continue relying on a stale
    // authenticated UI. A broken response lane is diagnosed, while the query
    // registry remains committed to the signed-out snapshot above.
    logForDebugging(
      `[direct-tui] Acosmi account removal committed but the logout response could not be queued: ${errorMessage(error)}`,
      { level: 'warn' },
    )
  }
  return refreshed.commands
}

/**
 * Select the one preview dialect this process will advertise to the model.
 * Version 1 has no preview contract; version 2 and future versions negotiate
 * only the small capability subset understood here. Markdown wins when a host
 * lists both so the choice is stable across list ordering.
 */
export function negotiateQuestionPreviewFormat(
  capability: SDKControlInitializeRequest['askUserQuestion'],
): 'markdown' | 'html' | undefined {
  if (!capability || capability.version < 2) return undefined
  const formats = capability.previewFormats ?? []
  if (formats.includes('markdown')) return 'markdown'
  if (formats.includes('html')) return 'html'
  return undefined
}

export async function handleInitializeRequest(
  request: SDKControlInitializeRequest,
  requestId: string,
  initialized: boolean,
  output: StdoutMessageSink,
  commands: InitializeCommandCatalog,
  modelInfos: ModelInfo[],
  structuredIO: StructuredIO,
  enableAuthStatus: boolean,
  options: {
    systemPrompt: string | undefined
    appendSystemPrompt: string | undefined
    agent?: string | undefined
    userSpecifiedModel?: string | undefined
    [key: string]: unknown
  },
  agents: AgentDefinition[],
  getAppState: () => AppState,
  strictDirectTuiCatalog = false,
): Promise<void> {
  if (initialized) {
    output.enqueue({
      type: 'control_response',
      response: {
        subtype: 'error',
        error: 'Already initialized',
        request_id: requestId,
        pending_permission_requests:
          structuredIO.getPendingPermissionRequests(),
      },
    })
    return
  }

  // Apply systemPrompt/appendSystemPrompt from stdin to avoid ARG_MAX limits
  if (request.systemPrompt !== undefined) {
    options.systemPrompt = request.systemPrompt
  }
  if (request.appendSystemPrompt !== undefined) {
    options.appendSystemPrompt = request.appendSystemPrompt
  }
  if (request.promptSuggestions !== undefined) {
    options.promptSuggestions = request.promptSuggestions
  }
  setQuestionPreviewFormat(
    negotiateQuestionPreviewFormat(request.askUserQuestion),
  )

  // Merge agents from stdin to avoid ARG_MAX limits
  if (request.agents) {
    const stdinAgents = parseAgentsFromJson(request.agents, 'flagSettings')
    agents.push(...stdinAgents)
  }

  // Re-evaluate main thread agent after SDK agents are merged
  // This allows --agent to reference agents defined via SDK
  if (options.agent) {
    // If main.tsx already found this agent (filesystem-defined), it already
    // applied systemPrompt/model/initialPrompt. Skip to avoid double-apply.
    const alreadyResolved = getMainThreadAgentType() === options.agent
    const mainThreadAgent = agents.find(a => a.agentType === options.agent)
    if (mainThreadAgent && !alreadyResolved) {
      // Update the main thread agent type in bootstrap state
      setMainThreadAgentType(mainThreadAgent.agentType)

      // Surface the agent's persona to QueryEngine via mainThreadAgentDefinition
      // so buildEffectiveSystemPrompt APPENDs the agent prompt under
      // `# Custom Agent Instructions` instead of REPLACING the user's
      // --system-prompt slot (which would produce a misleading
      // `# Custom System Prompt` header). Mirrors the W-PROMPT-SYSTEM-
      // ROOTCAUSE PR-2 wiring done at main.tsx for the non-interactive
      // --agent path. Existing guards preserved: skip when the user passed
      // --system-prompt explicitly (their override wins) and skip built-in
      // agents (their prompt is delivered via the AgentTool dispatch path).
      if (!options.systemPrompt && !isBuiltInAgent(mainThreadAgent)) {
        options.mainThreadAgentDefinition = mainThreadAgent
      }

      // Apply the agent's model if user didn't specify one and agent has a model
      if (
        !options.userSpecifiedModel &&
        mainThreadAgent.model &&
        mainThreadAgent.model !== 'inherit'
      ) {
        const agentModel = parseUserSpecifiedModel(mainThreadAgent.model)
        setMainLoopModelOverride(agentModel)
      }

      // SDK-defined agents arrive via init, so main.tsx's lookup missed them.
      if (mainThreadAgent.initialPrompt) {
        structuredIO.prependUserMessage(mainThreadAgent.initialPrompt)
      }
    } else if (mainThreadAgent?.initialPrompt) {
      // Filesystem-defined agent (alreadyResolved by main.tsx). main.tsx
      // handles initialPrompt for the string inputPrompt case, but when
      // inputPrompt is an AsyncIterable (SDK stream-json), it can't
      // concatenate — fall back to prependUserMessage here.
      structuredIO.prependUserMessage(mainThreadAgent.initialPrompt)
    }
  }

  const settings = getSettings_DEPRECATED()
  const outputStyle = settings?.outputStyle || DEFAULT_OUTPUT_STYLE_NAME
  const availableOutputStyles = await getAllOutputStyles(getCwd())

  // Get account information
  const accountInfo = getAccountInformation()
  if (request.hooks) {
    const hooks: Partial<Record<HookEvent, HookCallbackMatcher[]>> = {}
    for (const [event, matchers] of Object.entries(request.hooks)) {
      hooks[event as HookEvent] = matchers.map(matcher => {
        const callbacks = matcher.hookCallbackIds.map(callbackId => {
          return structuredIO.createHookCallback(callbackId, matcher.timeout)
        })
        return {
          matcher: matcher.matcher,
          hooks: callbacks,
        }
      })
    }
    registerHookCallbacks(hooks)
  }
  if (request.jsonSchema) {
    setInitJsonSchema(request.jsonSchema)
  }
  // Resolve a direct runtime's catalog as the final asynchronous operation
  // before constructing its correlated initialize response. The resolver can
  // wait for every catalog refresh revision that appeared while earlier
  // initialize work was in flight. Standard SDK callers retain the eager
  // array path and its established timing.
  const resolvedCommands =
    typeof commands === 'function' ? await commands() : commands
  const initResponse: SDKControlInitializeResponse = {
    commands: strictDirectTuiCatalog
      ? projectDirectTuiCommandCatalogEntries(
          resolvedCommands,
          formatHeadlessCommandDescription,
        )
      : projectCommandCatalogEntries(
          resolvedCommands,
          formatHeadlessCommandDescription,
        ),
    agents: agents.map(agent => ({
      name: agent.agentType,
      description: agent.whenToUse,
      // 'inherit' is an internal sentinel; normalize to undefined for the public API
      model: agent.model === 'inherit' ? undefined : agent.model,
    })),
    output_style: outputStyle,
    available_output_styles: Object.keys(availableOutputStyles),
    models: modelInfos,
    account: {
      email: accountInfo?.email,
      organization: accountInfo?.organization,
      subscriptionType: accountInfo?.subscription,
      tokenSource: accountInfo?.tokenSource,
      apiKeySource: accountInfo?.apiKeySource,
      // getAccountInformation() returns undefined under 3P providers, so the
      // other fields are all absent. apiProvider disambiguates "not logged
      // in" (firstParty + tokenSource:none) from "3P, login not applicable".
      apiProvider: getAPIProvider(),
    },
    pid: process.pid,
  }

  if (isFastModeEnabled() && isFastModeAvailable()) {
    const appState = getAppState()
    initResponse.fast_mode_state = getFastModeState(
      options.userSpecifiedModel ?? null,
      appState.fastMode,
    )
  }

  output.enqueue({
    type: 'control_response',
    response: {
      subtype: 'success',
      request_id: requestId,
      response: initResponse,
    },
  })

  // After the initialize message, check the auth status-
  // This will get notified of changes, but we also want to send the
  // initial state.
  if (enableAuthStatus) {
    const authStatusManager = AwsAuthStatusManager.getInstance()
    const status = authStatusManager.getStatus()
    if (status) {
      output.enqueue({
        type: 'auth_status',
        isAuthenticating: status.isAuthenticating,
        output: status.output,
        error: status.error,
        uuid: randomUUID(),
        session_id: getSessionId(),
      })
    }
  }
}

export async function handleRewindFiles(
  userMessageId: UUID,
  appState: AppState,
  setAppState: (updater: (prev: AppState) => AppState) => void,
  dryRun: boolean,
): Promise<RewindFilesResult> {
  if (!fileHistoryEnabled()) {
    return { canRewind: false, error: 'File rewinding is not enabled.' }
  }
  if (!fileHistoryCanRestore(appState.fileHistory, userMessageId)) {
    return {
      canRewind: false,
      error: 'No file checkpoint found for this message.',
    }
  }

  if (dryRun) {
    const diffStats = await fileHistoryGetDiffStats(
      appState.fileHistory,
      userMessageId,
    )
    return {
      canRewind: true,
      filesChanged: diffStats?.filesChanged,
      insertions: diffStats?.insertions,
      deletions: diffStats?.deletions,
    }
  }

  try {
    await fileHistoryRewind(
      updater =>
        setAppState(prev => ({
          ...prev,
          fileHistory: updater(prev.fileHistory),
        })),
      userMessageId,
    )
  } catch (error) {
    return {
      canRewind: false,
      error: `Failed to rewind: ${errorMessage(error)}`,
    }
  }

  return { canRewind: true }
}

export function handleSetPermissionMode(
  request: { mode: InternalPermissionMode },
  requestId: string,
  toolPermissionContext: ToolPermissionContext,
  output: StdoutMessageSink,
): ToolPermissionContext {
  // Check if trying to switch to bypassPermissions mode
  if (request.mode === 'bypassPermissions') {
    if (isBypassPermissionsModeDisabled()) {
      output.enqueue({
        type: 'control_response',
        response: {
          subtype: 'error',
          request_id: requestId,
          error:
            'Cannot set permission mode to bypassPermissions because it is disabled by settings or configuration',
        },
      })
      return toolPermissionContext
    }
    if (!toolPermissionContext.isBypassPermissionsModeAvailable) {
      output.enqueue({
        type: 'control_response',
        response: {
          subtype: 'error',
          request_id: requestId,
          error:
            'Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions',
        },
      })
      return toolPermissionContext
    }
  }

  // Check if trying to switch to auto mode without the classifier gate
  if (
    feature('TRANSCRIPT_CLASSIFIER') &&
    request.mode === 'auto' &&
    !isAutoModeGateEnabled()
  ) {
    const reason = getAutoModeUnavailableReason()
    output.enqueue({
      type: 'control_response',
      response: {
        subtype: 'error',
        request_id: requestId,
        error: reason
          ? `Cannot set permission mode to auto: ${getAutoModeUnavailableNotification(reason)}`
          : 'Cannot set permission mode to auto',
      },
    })
    return toolPermissionContext
  }

  // Allow the mode switch
  output.enqueue({
    type: 'control_response',
    response: {
      subtype: 'success',
      request_id: requestId,
      response: {
        mode: request.mode,
      },
    },
  })

  return {
    ...transitionPermissionMode(
      toolPermissionContext.mode,
      request.mode,
      toolPermissionContext,
    ),
    mode: request.mode,
  }
}

/**
 * IDE-triggered channel enable. Derives the ChannelEntry from the connection's
 * pluginSource (IDE can't spoof kind/marketplace — we only take the server
 * name), appends it to session allowedChannels, and runs the full gate. On
 * gate failure, rolls back the append. On success, registers a notification
 * handler that enqueues channel messages at priority:'next' — drainCommandQueue
 * picks them up between turns.
 *
 * Intentionally does NOT register the crabcode/channel/permission handler that
 * useManageMCPConnections sets up for interactive mode. That handler resolves
 * a pending dialog inside handleInteractivePermission — but print.ts never
 * calls handleInteractivePermission. When SDK permission lands on 'ask', it
 * goes to the consumer's canUseTool callback over stdio; there is no CLI-side
 * dialog for a remote "yes tbxkq" to resolve. If an IDE wants channel-relayed
 * tool approval, that's IDE-side plumbing against its own pending-map. (Also
 * gated separately by tengu_harbor_permissions — not yet shipping on
 * interactive either.)
 */
export function handleChannelEnable(
  requestId: string,
  serverName: string,
  connectionPool: readonly MCPServerConnection[],
  output: StdoutMessageSink,
): void {
  const respondError = (error: string) =>
    output.enqueue({
      type: 'control_response',
      response: { subtype: 'error', request_id: requestId, error },
    })

  if (!(feature('KAIROS') || feature('KAIROS_CHANNELS'))) {
    return respondError('channels feature not available in this build')
  }

  // Only a 'connected' client has .capabilities and .client to register the
  // handler on. The pool spread at the call site matches mcp_status.
  const connection = connectionPool.find(
    c => c.name === serverName && c.type === 'connected',
  )
  if (!connection || connection.type !== 'connected') {
    return respondError(`server ${serverName} is not connected`)
  }
  // W-MCP-RUNTIME-OWNERSHIP: worker owns invocation; main-proc has no handle
  // for a worker-projected connected server, so it cannot register channel
  // notification handlers here (the worker owns that side).
  if (!connection.client) {
    return respondError(
      `server ${serverName} is owned by the worker; channel handlers register worker-side`,
    )
  }

  const pluginSource = connection.config.pluginSource
  const parsed = pluginSource ? parsePluginIdentifier(pluginSource) : undefined
  if (!parsed?.marketplace) {
    // No pluginSource or @-less source — can never pass the {plugin,
    // marketplace}-keyed allowlist. Short-circuit with the same reason the
    // gate would produce.
    return respondError(
      `server ${serverName} is not plugin-sourced; channel_enable requires a marketplace plugin`,
    )
  }

  const entry: ChannelEntry = {
    kind: 'plugin',
    name: parsed.name,
    marketplace: parsed.marketplace,
  }
  // Idempotency: don't double-append on repeat enable.
  const prior = getAllowedChannels()
  const already = prior.some(
    e =>
      e.kind === 'plugin' &&
      e.name === entry.name &&
      e.marketplace === entry.marketplace,
  )
  if (!already) setAllowedChannels([...prior, entry])

  const gate = gateChannelServer(
    serverName,
    connection.capabilities,
    pluginSource,
  )
  if (gate.action === 'skip') {
    // Rollback — only remove the entry we appended.
    if (!already) setAllowedChannels(prior)
    return respondError(gate.reason)
  }

  const pluginId =
    `${entry.name}@${entry.marketplace}` as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS
  logMCPDebug(serverName, 'Channel notifications registered')
  logEvent('tengu_mcp_channel_enable', { plugin: pluginId })

  // Identical enqueue shape to the interactive register block in
  // useManageMCPConnections. drainCommandQueue processes it between turns —
  // channel messages queue at priority 'next' and are seen by the model on
  // the turn after they arrive.
  connection.client.setNotificationHandler(
    ChannelMessageNotificationSchema(),
    async notification => {
      const { content, meta } = notification.params
      logMCPDebug(
        serverName,
        `notifications/crabcode/channel: ${content.slice(0, 80)}`,
      )
      logEvent('tengu_mcp_channel_message', {
        content_length: content.length,
        meta_key_count: Object.keys(meta ?? {}).length,
        entry_kind:
          'plugin' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        is_dev: false,
        plugin: pluginId,
      })
      enqueue({
        mode: 'prompt',
        value: wrapChannelMessage(serverName, content, meta),
        priority: 'next',
        isMeta: true,
        origin: { kind: 'channel', server: serverName },
        skipSlashCommands: true,
      })
    },
  )

  output.enqueue({
    type: 'control_response',
    response: {
      subtype: 'success',
      request_id: requestId,
      response: undefined,
    },
  })
}

/**
 * Re-register the channel notification handler after mcp_reconnect /
 * mcp_toggle creates a new client. handleChannelEnable bound the handler to
 * the OLD client object; allowedChannels survives the reconnect but the
 * handler binding does not. Without this, channel messages silently drop
 * after a reconnect while the IDE still believes the channel is live.
 *
 * Mirrors the interactive CLI's onConnectionAttempt in
 * useManageMCPConnections, which re-gates on every new connection. Paired
 * with registerElicitationHandlers at the same call sites.
 *
 * No-op if the server was never channel-enabled: gateChannelServer calls
 * findChannelEntry internally and returns skip/session for an unlisted
 * server, so reconnecting a non-channel MCP server costs one feature-flag
 * check.
 */
export function reregisterChannelHandlerAfterReconnect(
  connection: MCPServerConnection,
): void {
  if (!(feature('KAIROS') || feature('KAIROS_CHANNELS'))) return
  if (connection.type !== 'connected') return
  // W-MCP-RUNTIME-OWNERSHIP: worker owns invocation; main-proc has no handle
  // for a worker-projected connected server — nothing to (re)register here.
  if (!connection.client) return

  const gate = gateChannelServer(
    connection.name,
    connection.capabilities,
    connection.config.pluginSource,
  )
  if (gate.action !== 'register') return

  const entry = findChannelEntry(
    connection.name,
    getAllowedChannels(),
    connection.config.pluginSource,
  )
  const pluginId =
    entry?.kind === 'plugin'
      ? (`${entry.name}@${entry.marketplace}` as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
      : undefined

  logMCPDebug(
    connection.name,
    'Channel notifications re-registered after reconnect',
  )
  connection.client.setNotificationHandler(
    ChannelMessageNotificationSchema(),
    async notification => {
      const { content, meta } = notification.params
      logMCPDebug(
        connection.name,
        `notifications/crabcode/channel: ${content.slice(0, 80)}`,
      )
      logEvent('tengu_mcp_channel_message', {
        content_length: content.length,
        meta_key_count: Object.keys(meta ?? {}).length,
        entry_kind:
          entry?.kind as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        is_dev: entry?.dev ?? false,
        plugin: pluginId,
      })
      enqueue({
        mode: 'prompt',
        value: wrapChannelMessage(connection.name, content, meta),
        priority: 'next',
        isMeta: true,
        origin: { kind: 'channel', server: connection.name },
        skipSlashCommands: true,
      })
    },
  )
}
