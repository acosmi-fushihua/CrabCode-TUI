import type { ContentBlockParam } from './types/api-types.js'
import { feature } from './utils/featurePolyfill.js'

import { randomUUID, type UUID } from 'crypto'
import last from 'lodash-es/last.js'
import {
  getSessionId,
  isSessionPersistenceDisabled,
} from 'src/bootstrap/state.js'
import type {
  PermissionMode,
  SDKCompactBoundaryMessage,
  SDKMessage,
  SDKPermissionDenial,
  SDKStatus,
  SDKUserMessageReplay,
} from 'src/entrypoints/agentSdkTypes.js'
import { accumulateUsage, updateUsage } from 'src/services/api/crabcode.js'
import type { NonNullableUsage } from 'src/services/api/logging.js'
import { EMPTY_USAGE } from 'src/services/api/logging.js'
import stripAnsi from 'strip-ansi'
import type { Command } from './types/command.js'
import { getHeadlessSlashCommandToolSkills } from './cli/headlessCommands.js'
import {
  LOCAL_COMMAND_STDERR_TAG,
  LOCAL_COMMAND_STDOUT_TAG,
} from './constants/xml.js'
import {
  getModelUsage,
  getTotalAPIDuration,
  getTotalCost,
} from './cost-tracker.js'
import type { CanUseToolFn } from './types/canUseTool.js'
import { loadMemoryPrompt } from './memdir/memdir.js'
import { hasAutoMemPathOverride } from './memdir/paths.js'
import { query } from './query.js'
import { categorizeRetryableAPIError } from './services/api/errors.js'
import {
  selectTurnAlwaysAllowRules,
  selectTurnCommandAllowRules,
} from './query/privateCommandRules.js'
import type { MCPServerConnection } from './services/mcp/types.js'
import type { AppState } from './state/AppState.js'
import type {
  AccountBridgeRuntimeAccess,
  CrabCodeThinkingMode,
} from './services/accountBridge/types.js'
import { type Tools, type ToolUseContext, toolMatchesName } from './Tool.js'
import { resolveAgentTools } from './tools/AgentTool/agentToolUtils.js'
import type { AgentDefinition } from './tools/AgentTool/loadAgentsDir.js'
import { SYNTHETIC_OUTPUT_TOOL_NAME } from './tools/SyntheticOutputTool/SyntheticOutputTool.js'
import type { Message } from './types/message.js'
import type { OrphanedPermission } from './types/textInputTypes.js'
import { createAbortController } from './utils/abortController.js'
import type { AttributionState } from './utils/commitAttribution.js'
import { getGlobalConfig } from './utils/config.js'
import { getCwd } from './utils/cwd.js'
import { isBareMode, isEnvTruthy } from './utils/envUtils.js'
import { getFastModeState } from './utils/fastMode.js'
import type { QuerySource } from './constants/querySource.js'
import {
  type FileHistoryState,
  fileHistoryEnabled,
  fileHistoryMakeSnapshot,
} from './utils/fileHistory.js'
import {
  cloneFileStateCache,
  type FileStateCache,
} from './utils/fileStateCache.js'
import { headlessProfilerCheckpoint } from './utils/headlessProfiler.js'
import { registerStructuredOutputEnforcement } from './utils/hooks/hookHelpers.js'
import { getInMemoryErrors } from './utils/log.js'
import { countToolCalls, SYNTHETIC_MESSAGES } from './utils/messages.js'
import {
  getMainLoopModel,
  parseUserSpecifiedModel,
} from './utils/model/model.js'
import { loadAllPluginsCacheOnly } from './utils/plugins/pluginLoader.js'
import {
  type ProcessUserInputContext,
  processUserInput,
} from './utils/processUserInput/processUserInputHeadless.js'
import {
  selectAutomationTurnMcpMetadata,
  selectAutomationTurnTools,
} from './utils/browserAutomation/turnToolSurface.js'
import {
  createAutomationTurnSurfaceLock,
  type AutomationTurnSurfaceLock,
} from './utils/browserAutomation/turnSurfaceLock.js'
import { fetchSystemPromptParts } from './utils/queryContext.js'
import { setCwd } from './utils/Shell.js'
import {
  flushSessionStorage,
  recordTranscript,
  removeTranscriptMessage,
} from './utils/sessionStorage.js'
import { buildEffectiveSystemPrompt } from './utils/systemPrompt.js'
import { asSystemPrompt } from './utils/systemPromptType.js'
import { resolveThemeSetting } from './utils/systemTheme.js'
import {
  shouldEnableThinkingByDefault,
  type ThinkingConfig,
} from './utils/thinking.js'
import { selectableUserMessagesFilter } from './utils/selectableUserMessages.js'
import { isDirectTuiBashContentBlocks } from './cli/directTuiInput.js'
import { publishDirectTuiInputEvents } from './cli/directTuiQueryEvents.js'

import {
  automationFailureToSDKAssistantMessage,
  localCommandOutputToSDKAssistantMessage,
  toSDKCompactMetadata,
} from './utils/messages/mappers.js'
import {
  buildSystemInitMessage,
  sdkCompatToolName,
} from './utils/messages/systemInit.js'
import {
  getScratchpadDir,
  isScratchpadEnabled,
} from './utils/permissions/filesystem.js'
/* eslint-enable @typescript-eslint/no-require-imports */
import {
  handleOrphanedPermission,
  isResultSuccessful,
  normalizeMessage,
} from './utils/queryHelpers.js'

// Dead code elimination: conditional import for coordinator mode
/* eslint-disable @typescript-eslint/no-require-imports */
const getCoordinatorUserContext: (
  mcpClients: ReadonlyArray<{ name: string }>,
  scratchpadDir?: string,
) => { [k: string]: string } = feature('COORDINATOR_MODE')
  ? require('./coordinator/coordinatorMode.js').getCoordinatorUserContext
  : () => ({})
/* eslint-enable @typescript-eslint/no-require-imports */

// Dead code elimination: conditional import for snip compaction
/* eslint-disable @typescript-eslint/no-require-imports */
const snipModule = feature('HISTORY_SNIP')
  ? (require('./services/compact/snipCompact.js') as typeof import('./services/compact/snipCompact.js'))
  : null
const snipProjection = feature('HISTORY_SNIP')
  ? (require('./services/compact/snipProjection.js') as typeof import('./services/compact/snipProjection.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

export { createAutomationTurnSurfaceLock }

export type QueryEngineInputMode = 'prompt' | 'bash'

export type QueryEngineSubmitOptions = {
  uuid?: string
  isMeta?: boolean
  /**
   * Explicit input interpretation for this submit only. Omission preserves
   * the historical prompt path; no content heuristic selects bash mode.
   */
  inputMode?: QueryEngineInputMode
}

export type QueryEngineConfig = {
  cwd: string
  tools: Tools
  commands: Command[]
  mcpClients: MCPServerConnection[]
  agents: AgentDefinition[]
  /**
   * Main-thread agent persona, used by `buildEffectiveSystemPrompt` to
   * APPEND `# Custom Agent Instructions\n<agent prompt>` to the default
   * system prompt (W-PROMPT-SYSTEM-ROOTCAUSE RC-1 PR-1). Distinct from
   * `agents` above, which is the AgentTool dispatch catalog.
   */
  mainThreadAgentDefinition?: AgentDefinition
  canUseTool: CanUseToolFn
  revalidateSideEffectAuthority?: ToolUseContext['revalidateSideEffectAuthority']
  getAppState: () => AppState
  setAppState: (f: (prev: AppState) => AppState) => void
  initialMessages?: Message[]
  readFileCache: FileStateCache
  customSystemPrompt?: string
  appendSystemPrompt?: string
  userSpecifiedModel?: string
  fallbackModel?: string
  thinkingConfig?: ThinkingConfig
  crabcodeThinkingMode?: CrabCodeThinkingMode
  accountBridgeRuntimeAccess?: AccountBridgeRuntimeAccess
  maxTurns?: number
  /**
   * True for the human-in-the-loop native direct TUI. Forwarded to `query()` as
   * `isInteractive` so the loop applies the interactive turn ceiling when no
   * explicit maxTurns is set. The standard print/SDK route leaves this false.
   */
  interactive?: boolean
  /**
   * Process-private direct-TUI ingress proof for the fixed composer shape:
   * zero or more non-text attachment blocks followed by one command text
   * block. Ordinary SDK/headless callers omit this and retain the historical
   * string-only bash contract.
   */
  allowDirectTuiBashContentBlocks?: boolean
  /**
   * Backend semantic origin, independent from the transport used to exchange
   * events. StructuredIO-backed native TUI turns use `repl_main_thread`;
   * ordinary SDK/headless callers retain `sdk`.
   */
  querySource?: QuerySource
  /**
   * Process-local observer for a renderer that consumes the established
   * internal message contract directly. Accepted processUserInput messages
   * are observed before query starts, and subsequent query events are observed
   * at the generator boundary. Both paths preserve the original object and
   * source order. Ordinary SDK/headless callers omit the observer and retain
   * their existing output.
   */
  onQueryEvent?: (event: Message) => void
  /**
   * Existing historical direct-TUI ToolUseContext callback. This remains an
   * in-process function and is deliberately absent from ToolUseContextContract
   * and every renderer/public wire. The fixed historical source injected it in
   * src/screens/repl/useQueryBridge.ts@2358212c:440-442.
   */
  sendOSNotification?: ToolUseContext['sendOSNotification']
  /**
   * Trusted foreground provenance for natural-language automation routing.
   * QueryEngine defaults this to false because SDK/headless prompts can be
   * generated by cron, background agents, or remote callers. Interactive
   * adapters must opt in per turn after checking their real origin.
   */
  localAutomationRoutingAllowed?: boolean
  maxBudgetUsd?: number
  taskBudget?: { total: number }
  jsonSchema?: Record<string, unknown>
  verbose?: boolean
  replayUserMessages?: boolean
  /** Handler for URL elicitations triggered by MCP tool -32042 errors. */
  handleElicitation?: ToolUseContext['handleElicitation']
  includePartialMessages?: boolean
  setSDKStatus?: (status: SDKStatus) => void
  abortController?: AbortController
  orphanedPermission?: OrphanedPermission
  /**
   * Snip-boundary handler: receives each yielded system message plus the
   * current mutableMessages store. Returns undefined if the message is not a
   * snip boundary; otherwise returns the replayed snip result. Injected by
   * ask() when HISTORY_SNIP is enabled so feature-gated strings stay inside
   * the gated module (keeps QueryEngine free of excluded strings and testable
   * despite feature() returning false under bun test). QueryEngine applies the
   * returned replay to its conversation store; each caller receives the
   * resulting event through its existing direct-observer or SDK projection.
   */
  snipReplay?: (
    yieldedSystemMsg: Message,
    store: Message[],
  ) => { messages: Message[]; executed: boolean } | undefined
  /**
   * W-STEER-MIDTURN (2026-06-25): per-turn mid-turn steer injection provider.
   * Invoked by `query()` at each tool-round boundary (after tool execution,
   * before the next LLM call). Returns the text prompt(s) to splice into the
   * CURRENT turn as `queued_command` attachments — the same fine-grained
   * injection point used for the process-global queue, but sourced from a
   * turn-scoped closure so independent turns cannot contaminate each other.
   * The provider returns the projected text and returns `[]` when nothing is
   * queued. Absent on the current direct and standard SDK wrappers, it is a
   * no-op.
   */
  midTurnSteerProvider?: () => string[]
  /**
   * Optional outer-turn capability lock shared with both steer drain paths.
   * The current native direct and standard SDK wrappers omit it and retain
   * their normal multi-turn lifecycle.
   */
  automationTurnSurfaceLock?: AutomationTurnSurfaceLock
  /**
   * Fired when a Write/Edit tool lands inside a `.crabcode/skills/` directory
   * this turn. Threaded onto the per-turn `ToolUseContext` (see
   * `ToolUseContext.onSkillsDiskChanged`). A caller may use it to invalidate
   * its own skill catalog after project-level writes that nested discovery
   * misses. The current native direct and standard SDK wrappers leave it
   * unset, so it is a no-op.
   */
  onSkillsDiskChanged?: () => void
  /**
   * Trusted in-process command rules for an internally issued private workflow.
   * Unlike slash-command rules these survive plain-text input processing.
   * External SDK or direct-renderer input must never populate this field.
   */
  privateCommandAllowRules?: readonly string[]
}

/**
 * QueryEngine owns the query lifecycle and session state for a conversation.
 * It extracts the core logic from ask() into a standalone class that can be
 * used by both the standard print/SDK route and the native direct-TUI route.
 *
 * One QueryEngine per conversation. Each submitMessage() call starts a new
 * turn within the same conversation. State (messages, file cache, usage, etc.)
 * persists across turns.
 */
export class QueryEngine {
  private config: QueryEngineConfig
  private mutableMessages: Message[]
  private abortController: AbortController
  private permissionDenials: SDKPermissionDenial[]
  private totalUsage: NonNullableUsage
  private hasHandledOrphanedPermission = false
  private readFileState: FileStateCache
  // Turn-scoped skill discovery tracking (feeds was_discovered on
  // tengu_skill_tool_invocation). Must persist across the two
  // processUserInputContext rebuilds inside submitMessage, but is cleared
  // at the start of each submitMessage to avoid unbounded growth across
  // many turns in SDK mode.
  private discoveredSkillNames = new Set<string>()
  private loadedNestedMemoryPaths = new Set<string>()

  constructor(config: QueryEngineConfig) {
    this.config = config
    this.mutableMessages = config.initialMessages ?? []
    this.abortController = config.abortController ?? createAbortController()
    this.permissionDenials = []
    this.readFileState = config.readFileCache
    this.totalUsage = EMPTY_USAGE
  }

  async *submitMessage(
    prompt: string | ContentBlockParam[],
    options?: QueryEngineSubmitOptions,
  ): AsyncGenerator<SDKMessage, void, unknown> {
    // This is a runtime trust boundary as well as a TypeScript API. StructuredIO
    // frames and JavaScript callers can bypass the static union, so reject
    // every value except the two modes this engine implements.
    const requestedInputMode: unknown = options?.inputMode
    const inputMode =
      requestedInputMode === undefined ? 'prompt' : requestedInputMode
    if (inputMode !== 'prompt' && inputMode !== 'bash') {
      throw new TypeError(
        'QueryEngine submitMessage options.inputMode must be prompt or bash',
      )
    }
    // Ordinary SDK/headless bash remains string-only. The native direct TUI
    // may opt into exactly the fixed composer projection: zero or more
    // non-text attachment blocks followed by one command text block.
    const directTuiBashContentBlocks =
      inputMode === 'bash' &&
      typeof prompt !== 'string' &&
      this.config.allowDirectTuiBashContentBlocks === true &&
      isDirectTuiBashContentBlocks(prompt)
    if (
      inputMode === 'bash' &&
      typeof prompt !== 'string' &&
      !directTuiBashContentBlocks
    ) {
      throw new TypeError(
        'QueryEngine bash inputMode requires a string prompt',
      )
    }
    // Private workflow prompts carry their own tool/canUseTool boundary.
    // processBashCommand is the existing user-owned unsandboxed `!` lane and
    // intentionally bypasses that model-tool permission path, so the two
    // capabilities must never be combined.
    if (
      inputMode === 'bash' &&
      this.config.privateCommandAllowRules !== undefined
    ) {
      throw new TypeError(
        'QueryEngine bash inputMode is unavailable for private workflows',
      )
    }

    const {
      cwd,
      commands,
      tools,
      mcpClients,
      verbose = false,
      thinkingConfig,
      crabcodeThinkingMode,
      accountBridgeRuntimeAccess,
      maxTurns,
      maxBudgetUsd,
      taskBudget,
      canUseTool,
      revalidateSideEffectAuthority,
      customSystemPrompt,
      appendSystemPrompt,
      userSpecifiedModel,
      fallbackModel,
      jsonSchema,
      getAppState,
      setAppState,
      replayUserMessages = false,
      includePartialMessages = false,
      agents = [],
      mainThreadAgentDefinition,
      setSDKStatus,
      orphanedPermission,
      privateCommandAllowRules,
      localAutomationRoutingAllowed = false,
      automationTurnSurfaceLock,
      interactive = false,
      querySource = 'sdk',
    } = this.config
    const isNonInteractiveSession = !interactive

    // A fallback steer calls submitMessage again on the same locked outer turn.
    // Validate its requested surface before attachments/slash expansion can run.
    // The initial submit is intentionally a no-op until its processed surface is
    // captured below.
    if (inputMode === 'prompt' && typeof prompt === 'string') {
      automationTurnSurfaceLock?.assertSteerPrompt(prompt)
    }

    this.discoveredSkillNames.clear()
    setCwd(cwd)
    const persistSession = !isSessionPersistenceDisabled()
    const startTime = Date.now()

    // Wrap canUseTool to track permission denials
    const wrappedCanUseTool: CanUseToolFn = async (
      tool,
      input,
      toolUseContext,
      assistantMessage,
      toolUseID,
      forceDecision,
    ) => {
      const result = await canUseTool(
        tool,
        input,
        toolUseContext,
        assistantMessage,
        toolUseID,
        forceDecision,
      )

      // Track denials for SDK reporting
      if (result.behavior !== 'allow') {
        this.permissionDenials.push({
          tool_name: sdkCompatToolName(tool.name),
          tool_use_id: toolUseID,
          tool_input: input,
        })
      }

      return result
    }

    const initialAppState = getAppState()
    const initialMainLoopModel = userSpecifiedModel
      ? parseUserSpecifiedModel(userSpecifiedModel)
      : getMainLoopModel()

    const initialThinkingConfig: ThinkingConfig = thinkingConfig
      ? thinkingConfig
      : shouldEnableThinkingByDefault() !== false
        ? { type: 'adaptive' }
        : { type: 'disabled' }

    // Match the TUI's REPL/useQueryBridge path: a main-thread `--agent`
    // definition owns both the callable tool subset and any `Agent(...)`
    // dispatch allowlist. Resolve once per submitted turn, then carry the same
    // snapshot through input processing, automation-surface intersection,
    // prompt/schema construction, and execution so those views cannot drift.
    const mainThreadAgentToolResolution = mainThreadAgentDefinition
      ? resolveAgentTools(mainThreadAgentDefinition, tools, false, true)
      : undefined
    const agentScopedTools =
      mainThreadAgentToolResolution?.resolvedTools ?? tools
    const allowedAgentTypes =
      mainThreadAgentToolResolution?.allowedAgentTypes
    const effectiveAgentDefinitions = allowedAgentTypes
      ? { activeAgents: agents, allAgents: [], allowedAgentTypes }
      : { activeAgents: agents, allAgents: [] }

    // Narrow once so TS tracks the type through the conditionals below.
    const customPrompt =
      typeof customSystemPrompt === 'string' ? customSystemPrompt : undefined

    // When an SDK caller provides a custom system prompt AND has set
    // CRABCODE_COWORK_MEMORY_PATH_OVERRIDE, inject the memory-mechanics prompt.
    // The env var is an explicit opt-in signal — the caller has wired up
    // a memory directory and needs CrabCode to know how to use it (which
    // Write/Edit tools to call, MEMORY.md filename, loading semantics).
    // The caller can layer their own policy text via appendSystemPrompt.
    const memoryMechanicsPrompt =
      privateCommandAllowRules === undefined &&
      customPrompt !== undefined &&
      hasAutoMemPathOverride()
        ? await loadMemoryPrompt()
        : null

    // Register function hook for structured output enforcement
    const hasStructuredOutputTool = agentScopedTools.some(t =>
      toolMatchesName(t, SYNTHETIC_OUTPUT_TOOL_NAME),
    )
    if (jsonSchema && hasStructuredOutputTool) {
      registerStructuredOutputEnforcement(setAppState, getSessionId())
    }

    let processUserInputContext: ProcessUserInputContext = {
      messages: this.mutableMessages,
      // Slash commands that mutate the message array (e.g. /force-snip)
      // call setMessages(fn).  In interactive mode this writes back to
      // AppState; in print mode we write back to mutableMessages so the
      // rest of the query loop (push at :389, snapshot at :392) sees
      // the result.  The second processUserInputContext below (after
      // slash-command processing) keeps the no-op — nothing else calls
      // setMessages past that point.
      setMessages: fn => {
        this.mutableMessages = fn(this.mutableMessages)
      },
      onChangeAPIKey: () => {},
      handleElicitation: this.config.handleElicitation,
      sendOSNotification: this.config.sendOSNotification,
      options: {
        commands,
        debug: false, // we use stdout, so don't want to clobber it
        tools: agentScopedTools,
        verbose,
        mainLoopModel: initialMainLoopModel,
        thinkingConfig: initialThinkingConfig,
        crabcodeThinkingMode,
        accountBridgeRuntimeAccess,
        mcpClients,
        mcpResources: {},
        ideInstallationStatus: null,
        isNonInteractiveSession,
        customSystemPrompt,
        appendSystemPrompt,
        agentDefinitions: effectiveAgentDefinitions,
        theme: resolveThemeSetting(getGlobalConfig().theme),
        maxBudgetUsd,
      },
      getAppState,
      setAppState,
      abortController: this.abortController,
      readFileState: this.readFileState,
      nestedMemoryAttachmentTriggers: new Set<string>(),
      loadedNestedMemoryPaths: this.loadedNestedMemoryPaths,
      dynamicSkillDirTriggers: new Set<string>(),
      // A private sealed workflow must revalidate hook-updated input through
      // canUseTool. Without this, a PreToolUse hook returning allow could
      // replace a safe command with arbitrary Bash and bypass the guard.
      requireCanUseTool: privateCommandAllowRules !== undefined,
      revalidateSideEffectAuthority,
      suppressUntrustedHooks: privateCommandAllowRules !== undefined,
      onSkillsDiskChanged: this.config.onSkillsDiskChanged,
      discoveredSkillNames: this.discoveredSkillNames,
      setInProgressToolUseIDs: () => {},
      setResponseLength: () => {},
      updateFileHistoryState: (
        updater: (prev: FileHistoryState) => FileHistoryState,
      ) => {
        setAppState(prev => {
          const updated = updater(prev.fileHistory)
          if (updated === prev.fileHistory) return prev
          return { ...prev, fileHistory: updated }
        })
      },
      updateAttributionState: (
        updater: (prev: AttributionState) => AttributionState,
      ) => {
        setAppState(prev => {
          const updated = updater(prev.attribution)
          if (updated === prev.attribution) return prev
          return { ...prev, attribution: updated }
        })
      },
      setSDKStatus,
    }

    // Handle orphaned permission (only once per engine lifetime)
    if (orphanedPermission && !this.hasHandledOrphanedPermission) {
      this.hasHandledOrphanedPermission = true
      for await (const message of handleOrphanedPermission(
        orphanedPermission,
        agentScopedTools,
        this.mutableMessages,
        processUserInputContext,
      )) {
        yield message
      }
    }

    const {
      messages: messagesFromUserInput,
      shouldQuery,
      allowedTools,
      automationSurface,
      automationFailure,
      model: modelFromUserInput,
      resultText,
    } = await processUserInput({
      input: prompt,
      mode: inputMode,
      setToolJSX: () => {},
      context: {
        ...processUserInputContext,
        messages: this.mutableMessages,
      },
      messages: this.mutableMessages,
      uuid: options?.uuid,
      isMeta: options?.isMeta,
      querySource,
      // The sealed prompt contains untrusted diff text. Treat it as opaque
      // text: @file/skill/MCP discovery and slash-command expansion would
      // otherwise create reads/network/hooks outside the Git capability.
      skipAttachments: privateCommandAllowRules !== undefined,
      skipSlashCommands: privateCommandAllowRules !== undefined,
      localAutomationRoutingAllowed,
      onLocalInputEvent: this.config.onQueryEvent,
    })
    const processedCommandAllowRules = selectTurnCommandAllowRules(
      allowedTools,
      privateCommandAllowRules,
    )
    const lockedSurfaceResolution = automationTurnSurfaceLock?.acceptProcessedSurface(
      automationSurface,
      processedCommandAllowRules,
    )
    const effectiveAutomationSurface = lockedSurfaceResolution
      ? lockedSurfaceResolution.automationSurface
      : automationSurface
    const effectiveCommandAllowRules = lockedSurfaceResolution
      ? lockedSurfaceResolution.commandAllowRules
      : processedCommandAllowRules
    // A hard-routed automation turn gets an actual model-visible tool/schema
    // boundary, not merely permission auto-allow rules. The helper also
    // intersects a fixed per-surface family,
    // so drift in a skill's allowedTools cannot widen this boundary.
    const turnTools = selectAutomationTurnTools(
      agentScopedTools,
      effectiveAutomationSurface,
      effectiveCommandAllowRules,
      {
        localAutomationRoutingAllowed,
      },
    )
    const turnMcpMetadata = selectAutomationTurnMcpMetadata(
      { mcpClients, mcpResources: {} },
      effectiveAutomationSurface,
      turnTools,
    )
    const mainLoopModel = modelFromUserInput ?? initialMainLoopModel

    // Build the model prefix only after the hard route is fixed. Input parsing
    // above intentionally sees the complete agent-scoped catalog, while the
    // model prompt, MCP instruction delta, and tool schemas must all describe
    // one surface.
    headlessProfilerCheckpoint('before_getSystemPrompt')
    const {
      defaultSystemPrompt,
      userContext: baseUserContext,
      systemContext,
    } = await fetchSystemPromptParts({
      tools: turnTools,
      mainLoopModel,
      additionalWorkingDirectories: Array.from(
        initialAppState.toolPermissionContext.additionalWorkingDirectories.keys(),
      ),
      mcpClients: turnMcpMetadata.mcpClients,
      customSystemPrompt: customPrompt,
      // A server-issued private workflow must not execute InstructionsLoaded
      // hooks or inject repository-controlled memory into its model context.
      includeUserContext: privateCommandAllowRules === undefined,
      // getSystemContext performs ordinary Git probes (including status), so
      // the private workflow supplies none and keeps all Git inside its runner.
      includeSystemContext: privateCommandAllowRules === undefined,
    })
    headlessProfilerCheckpoint('after_getSystemPrompt')
    const userContext = {
      ...baseUserContext,
      ...(privateCommandAllowRules === undefined
        ? getCoordinatorUserContext(
            turnMcpMetadata.mcpClients,
            isScratchpadEnabled() ? getScratchpadDir() : undefined,
          )
        : {}),
    }

    // W-PROMPT-SYSTEM-ROOTCAUSE RC-1 PR-1: route through buildEffectiveSystemPrompt
    // so customSystemPrompt / mainThreadAgentDefinition / coordinator paths
    // APPEND to default (with their respective `# Custom System Prompt` /
    // `# Custom Agent Instructions` / `# Coordinator Mode` headers) instead of
    // REPLACING and dropping Product Core + project_rules + memory + env_info.
    //
    // Ordering invariant (audit follow-up to PR-1 self-inconsistency):
    // pre-PR-1 SDK form was `[content, memoryMechanics, appendSystemPrompt]`
    // and the PR-3 side_question fallback preserves this. We pass
    // `appendSystemPrompt: undefined` into buildEffective so the layered
    // body comes back without the user-supplied tail, then splice
    // memoryMechanics → appendSystemPrompt at the very end. This preserves
    // the convention that `--append-system-prompt` is the user's "final say"
    // (last segment wins) and keeps SDK + side_question fingerprints aligned.
    const baseSystemPrompt = buildEffectiveSystemPrompt({
      mainThreadAgentDefinition,
      toolUseContext: {
        options: {
          commands,
          debug: false,
          mainLoopModel,
          tools: turnTools,
          verbose,
          thinkingConfig: initialThinkingConfig,
          crabcodeThinkingMode,
          accountBridgeRuntimeAccess,
          ...turnMcpMetadata,
          isNonInteractiveSession,
          agentDefinitions: effectiveAgentDefinitions,
          maxBudgetUsd,
          customSystemPrompt: customPrompt,
          appendSystemPrompt,
        },
      },
      customSystemPrompt: customPrompt,
      defaultSystemPrompt,
      appendSystemPrompt: undefined,
    })
    const systemPrompt = asSystemPrompt([
      ...baseSystemPrompt,
      ...(memoryMechanicsPrompt ? [memoryMechanicsPrompt] : []),
      ...(appendSystemPrompt ? [appendSystemPrompt] : []),
    ])

    // Push new messages, including user input and any attachments
    this.mutableMessages.push(...messagesFromUserInput)

    // Update params to reflect updates from processing /slash commands
    const messages = [...this.mutableMessages]

    // Persist the user's message(s) to transcript BEFORE entering the query
    // loop. The for-await below only calls recordTranscript when ask() yields
    // an assistant/user/compact_boundary message — which doesn't happen until
    // the API responds. If the process is killed before that (e.g. user clicks
    // Stop in cowork seconds after send), the transcript is left with only
    // queue-operation entries; getLastSessionLog filters those out, returns
    // null, and --resume fails with "No conversation found". Writing now makes
    // the transcript resumable from the point the user message was accepted,
    // even if no API response ever arrives.
    //
    // --bare / SIMPLE: fire-and-forget. Scripted calls don't --resume after
    // kill-mid-request. The await is ~4ms on SSD, ~30ms under disk contention
    // — the single largest controllable critical-path cost after module eval.
    // Transcript is still written (for post-hoc debugging); just not blocking.
    if (persistSession && messagesFromUserInput.length > 0) {
      const transcriptPromise = recordTranscript(messages)
      if (isBareMode()) {
        void transcriptPromise
      } else {
        await transcriptPromise
        if (
          isEnvTruthy(process.env.CRABCODE_EAGER_FLUSH) ||
          isEnvTruthy(process.env.CRABCODE_IS_COWORK)
        ) {
          await flushSessionStorage()
        }
      }
    }

    // Filter messages that should be acknowledged after transcript
    const replayableMessages = messagesFromUserInput.filter(
      msg =>
        (msg.type === 'user' &&
          !msg.isMeta && // Skip synthetic caveat messages
          !msg.toolUseResult && // Skip tool results (they'll be acked from query)
          selectableUserMessagesFilter(msg)) || // Skip non-user-authored messages (task notifications, etc.)
        (msg.type === 'system' && msg.subtype === 'compact_boundary'), // Always ack compact boundaries
    )
    const messagesToAck = replayUserMessages ? replayableMessages : []

    // Update the ToolPermissionContext based on user input processing (as necessary)
    setAppState(prev => ({
      ...prev,
      toolPermissionContext: {
        ...prev.toolPermissionContext,
        alwaysAllowRules: selectTurnAlwaysAllowRules(
          prev.toolPermissionContext.alwaysAllowRules,
          effectiveCommandAllowRules,
          privateCommandAllowRules,
        ),
      },
    }))

    // Recreate after processing the prompt to pick up updated messages and
    // model (from slash commands).
    processUserInputContext = {
      messages,
      setMessages: () => {},
      onChangeAPIKey: () => {},
      handleElicitation: this.config.handleElicitation,
      sendOSNotification: this.config.sendOSNotification,
      options: {
        commands,
        debug: false,
        tools: turnTools,
        verbose,
        mainLoopModel,
        thinkingConfig: initialThinkingConfig,
        crabcodeThinkingMode,
        accountBridgeRuntimeAccess,
        ...turnMcpMetadata,
        ideInstallationStatus: null,
        isNonInteractiveSession,
        customSystemPrompt,
        appendSystemPrompt,
        theme: resolveThemeSetting(getGlobalConfig().theme),
        agentDefinitions: effectiveAgentDefinitions,
        maxBudgetUsd,
      },
      getAppState,
      setAppState,
      abortController: this.abortController,
      readFileState: this.readFileState,
      nestedMemoryAttachmentTriggers: new Set<string>(),
      loadedNestedMemoryPaths: this.loadedNestedMemoryPaths,
      dynamicSkillDirTriggers: new Set<string>(),
      requireCanUseTool: privateCommandAllowRules !== undefined,
      revalidateSideEffectAuthority,
      suppressUntrustedHooks: privateCommandAllowRules !== undefined,
      onSkillsDiskChanged: this.config.onSkillsDiskChanged,
      discoveredSkillNames: this.discoveredSkillNames,
      setInProgressToolUseIDs: () => {},
      setResponseLength: () => {},
      updateFileHistoryState: processUserInputContext.updateFileHistoryState,
      updateAttributionState: processUserInputContext.updateAttributionState,
      setSDKStatus,
    }

    headlessProfilerCheckpoint('before_skills_plugins')
    // Cache-only: headless/SDK/CCR startup must not block on network for
    // ref-tracked plugins. CCR populates the cache via CRABCODE_SYNC_PLUGIN_INSTALL
    // (headlessPluginInstall) or CRABCODE_PLUGIN_SEED_DIR before this runs;
    // SDK callers that need fresh source can call /reload-plugins.
    const [skills, enabledPlugins] =
      privateCommandAllowRules !== undefined
        ? [[], []]
        : await Promise.all([
            getHeadlessSlashCommandToolSkills(getCwd()),
            loadAllPluginsCacheOnly().then(({ enabled }) => enabled),
          ])
    headlessProfilerCheckpoint('after_skills_plugins')

    yield buildSystemInitMessage({
      tools: turnTools,
      mcpClients: turnMcpMetadata.mcpClients,
      model: mainLoopModel,
      permissionMode: initialAppState.toolPermissionContext
        .mode as PermissionMode,
      commands,
      agents,
      skills,
      plugins: enabledPlugins,
      fastMode: initialAppState.fastMode,
    })

    // Record when system message is yielded for headless latency tracking
    headlessProfilerCheckpoint('system_message_yielded')

    // The fixed interactive lifecycle appended this exact processUserInput
    // batch to the transcript before starting query(). The query generator
    // yields later assistant/tool-result events, but not the accepted initial
    // user message. Publish the original objects once at the process-private
    // renderer boundary; ordinary SDK/headless callers have no observer.
    publishDirectTuiInputEvents(
      messagesFromUserInput,
      this.config.onQueryEvent,
    )

    if (!shouldQuery) {
      if (automationFailure) {
        const sourceMessage = messagesFromUserInput.find(
          message => message.type === 'system',
        )
        yield automationFailureToSDKAssistantMessage(
          automationFailure,
          sourceMessage?.uuid ?? randomUUID(),
        )
      }
      // Return the results of local slash commands.
      // Use messagesFromUserInput (not replayableMessages) for command output
      // because selectableUserMessagesFilter excludes local-command-stdout tags.
      for (const msg of messagesFromUserInput) {
        if (
          msg.type === 'user' &&
          typeof msg.message.content === 'string' &&
          (msg.message.content.includes(`<${LOCAL_COMMAND_STDOUT_TAG}>`) ||
            msg.message.content.includes(`<${LOCAL_COMMAND_STDERR_TAG}>`) ||
            msg.isCompactSummary)
        ) {
          yield {
            type: 'user',
            message: {
              ...msg.message,
              content: stripAnsi(msg.message.content),
            },
            session_id: getSessionId(),
            parent_tool_use_id: null,
            uuid: msg.uuid,
            timestamp: msg.timestamp,
            isReplay: !msg.isCompactSummary,
            isSynthetic: msg.isMeta || msg.isVisibleInTranscriptOnly,
          } as SDKUserMessageReplay
        }

        // Local command output — yield as a synthetic assistant message so
        // RC renders it as assistant-style text rather than a user bubble.
        // Emitted as assistant (not the dedicated SDKLocalCommandOutputMessage
        // system subtype) so mobile clients + session-ingress can parse it.
        if (
          msg.type === 'system' &&
          msg.subtype === 'local_command' &&
          typeof msg.content === 'string' &&
          (msg.content.includes(`<${LOCAL_COMMAND_STDOUT_TAG}>`) ||
            msg.content.includes(`<${LOCAL_COMMAND_STDERR_TAG}>`))
        ) {
          yield localCommandOutputToSDKAssistantMessage(msg.content, msg.uuid)
        }

        if (msg.type === 'system' && msg.subtype === 'compact_boundary') {
          yield {
            type: 'system',
            subtype: 'compact_boundary' as const,
            session_id: getSessionId(),
            uuid: msg.uuid,
            compact_metadata: toSDKCompactMetadata(msg.compactMetadata),
          } as SDKCompactBoundaryMessage
        }
      }

      if (persistSession) {
        await recordTranscript(messages)
        if (
          isEnvTruthy(process.env.CRABCODE_EAGER_FLUSH) ||
          isEnvTruthy(process.env.CRABCODE_IS_COWORK)
        ) {
          await flushSessionStorage()
        }
      }

      yield {
        type: 'result',
        subtype: 'success',
        is_error: false,
        duration_ms: Date.now() - startTime,
        duration_api_ms: getTotalAPIDuration(),
        num_turns: messages.length - 1,
        result: resultText ?? '',
        stop_reason: null,
        session_id: getSessionId(),
        total_cost_usd: getTotalCost(),
        usage: this.totalUsage,
        modelUsage: getModelUsage(),
        permission_denials: this.permissionDenials,
        fast_mode_state: getFastModeState(
          mainLoopModel,
          initialAppState.fastMode,
        ),
        uuid: randomUUID(),
      }
      return
    }

    if (fileHistoryEnabled() && persistSession) {
      messagesFromUserInput
        .filter(selectableUserMessagesFilter)
        .forEach(message => {
          void fileHistoryMakeSnapshot(
            (updater: (prev: FileHistoryState) => FileHistoryState) => {
              setAppState(prev => ({
                ...prev,
                fileHistory: updater(prev.fileHistory),
              }))
            },
            message.uuid,
          )
        })
    }

    // Track current message usage (reset on each message_start)
    let currentMessageUsage: NonNullableUsage = EMPTY_USAGE
    let turnCount = 1
    let hasAcknowledgedInitialMessages = false
    // Track structured output from StructuredOutput tool calls
    let structuredOutputFromTool: unknown
    // Track the last stop_reason from assistant messages
    let lastStopReason: string | null = null
    // Reference-based watermark so error_during_execution's errors[] is
    // turn-scoped. A length-based index breaks when the 100-entry ring buffer
    // shift()s during the turn — the index slides. If this entry is rotated
    // out, lastIndexOf returns -1 and we include everything (safe fallback).
    const errorLogWatermark = getInMemoryErrors().at(-1)
    // Snapshot count before this query for delta-based retry limiting
    const initialStructuredOutputCalls = jsonSchema
      ? countToolCalls(this.mutableMessages, SYNTHETIC_OUTPUT_TOOL_NAME)
      : 0
    for await (const message of query({
      messages,
      systemPrompt,
      userContext,
      systemContext,
      canUseTool: wrappedCanUseTool,
      toolUseContext: processUserInputContext,
      fallbackModel,
      querySource,
      maxTurns,
      isInteractive: interactive,
      taskBudget,
      midTurnSteerProvider: this.config.midTurnSteerProvider
        ? () => {
            const prompts = this.config.midTurnSteerProvider!()
            for (const steerPrompt of prompts) {
              automationTurnSurfaceLock?.assertSteerPrompt(steerPrompt)
            }
            return prompts
          }
        : undefined,
    })) {
      // The direct TUI consumes the historical query() event contract. Keep
      // this observation as the first operation in the loop: SDK projection
      // below intentionally drops, selects, or reconstructs several events.
      this.config.onQueryEvent?.(message)

      // Record assistant, user, and compact boundary messages
      if (
        message.type === 'assistant' ||
        message.type === 'user' ||
        (message.type === 'system' && message.subtype === 'compact_boundary')
      ) {
        // Before writing a compact boundary, flush any in-memory-only
        // messages up through the preservedSegment tail. Attachments and
        // progress are now recorded inline (their switch cases below), but
        // this flush still matters for the preservedSegment tail walk.
        // If the SDK subprocess restarts before then (crabcode-desktop kills
        // between turns), tailUuid points to a never-written message →
        // applyPreservedSegmentRelinks fails its tail→head walk → returns
        // without pruning → resume loads full pre-compact history.
        if (
          persistSession &&
          message.type === 'system' &&
          message.subtype === 'compact_boundary'
        ) {
          const tailUuid = message.compactMetadata?.preservedSegment?.tailUuid
          if (tailUuid) {
            const tailIdx = this.mutableMessages.findLastIndex(
              m => m.uuid === tailUuid,
            )
            if (tailIdx !== -1) {
              await recordTranscript(this.mutableMessages.slice(0, tailIdx + 1))
            }
          }
        }
        messages.push(message)
        if (persistSession) {
          // Fire-and-forget for assistant messages. crabcode.ts yields one
          // assistant message per content block, then mutates the last
          // one's message.usage/stop_reason on message_delta — relying on
          // the write queue's 100ms lazy jsonStringify. Awaiting here
          // blocks ask()'s generator, so message_delta can't run until
          // every block is consumed; the drain timer (started at block 1)
          // elapses first. Interactive CC doesn't hit this because
          // useLogMessages.ts fire-and-forgets. enqueueWrite is
          // order-preserving so fire-and-forget here is safe.
          if (message.type === 'assistant') {
            void recordTranscript(messages)
          } else {
            await recordTranscript(messages)
          }
        }

        // Acknowledge initial user messages after first transcript recording
        if (!hasAcknowledgedInitialMessages && messagesToAck.length > 0) {
          hasAcknowledgedInitialMessages = true
          for (const msgToAck of messagesToAck) {
            if (msgToAck.type === 'user') {
              yield {
                type: 'user',
                message: msgToAck.message,
                session_id: getSessionId(),
                parent_tool_use_id: null,
                uuid: msgToAck.uuid,
                timestamp: msgToAck.timestamp,
                isReplay: true,
              } as SDKUserMessageReplay
            }
          }
        }
      }

      if (message.type === 'user') {
        turnCount++
      }

      switch (message.type) {
        case 'tombstone': {
          // The direct TUI receives the original tombstone through
          // onQueryEvent above. Also retract the target from the in-memory
          // query state and persisted transcript before a retry appends its
          // replacement.
          const targetUuid = message.message.uuid
          if (targetUuid === undefined) break
          const mutableIndex = this.mutableMessages.findLastIndex(
            item => item.uuid === targetUuid,
          )
          if (mutableIndex !== -1) this.mutableMessages.splice(mutableIndex, 1)
          const bufferIndex = messages.findLastIndex(
            item => item.uuid === targetUuid,
          )
          if (bufferIndex !== -1) messages.splice(bufferIndex, 1)
          if (persistSession) {
            await removeTranscriptMessage(targetUuid as UUID)
          }
          break
        }
        case 'assistant':
          // Capture stop_reason if already set (synthetic messages). For
          // streamed responses, this is null at content_block_stop time;
          // the real value arrives via message_delta (handled below).
          if (message.message.stop_reason != null) {
            lastStopReason = message.message.stop_reason
          }
          this.mutableMessages.push(message)
          yield* normalizeMessage(message)
          break
        case 'progress':
          this.mutableMessages.push(message)
          // Record inline so the dedup loop in the next ask() call sees it
          // as already-recorded. Without this, deferred progress interleaves
          // with already-recorded tool_results in mutableMessages, and the
          // dedup walk freezes startingParentUuid at the wrong message —
          // forking the chain and orphaning the conversation on resume.
          if (persistSession) {
            messages.push(message)
            void recordTranscript(messages)
          }
          yield* normalizeMessage(message)
          break
        case 'user':
          this.mutableMessages.push(message)
          yield* normalizeMessage(message)
          break
        case 'stream_event':
          if (message.event.type === 'message_start') {
            // Reset current message usage for new message
            currentMessageUsage = EMPTY_USAGE
            currentMessageUsage = updateUsage(
              currentMessageUsage,
              message.event.message.usage,
            )
          }
          if (message.event.type === 'message_delta') {
            currentMessageUsage = updateUsage(
              currentMessageUsage,
              message.event.usage,
            )
            // Capture stop_reason from message_delta. The assistant message
            // is yielded at content_block_stop with stop_reason=null; the
            // real value only arrives here (see crabcode.ts message_delta
            // handler). Without this, result.stop_reason is always null.
            if (message.event.delta.stop_reason != null) {
              lastStopReason = message.event.delta.stop_reason
            }
          }
          if (message.event.type === 'message_stop') {
            // Accumulate current message usage into total
            this.totalUsage = accumulateUsage(
              this.totalUsage,
              currentMessageUsage,
            )
          }

          if (includePartialMessages) {
            yield {
              type: 'stream_event' as const,
              event: message.event,
              session_id: getSessionId(),
              parent_tool_use_id: null,
              uuid: randomUUID(),
            }
          }

          break
        case 'attachment':
          this.mutableMessages.push(message)
          // Record inline (same reason as progress above).
          if (persistSession) {
            messages.push(message)
            void recordTranscript(messages)
          }

          // Extract structured output from StructuredOutput tool calls
          if (message.attachment.type === 'structured_output') {
            structuredOutputFromTool = message.attachment.data
          }
          // Handle max turns reached signal from query.ts
          else if (message.attachment.type === 'max_turns_reached') {
            if (persistSession) {
              if (
                isEnvTruthy(process.env.CRABCODE_EAGER_FLUSH) ||
                isEnvTruthy(process.env.CRABCODE_IS_COWORK)
              ) {
                await flushSessionStorage()
              }
            }
            yield {
              type: 'result',
              subtype: 'error_max_turns',
              duration_ms: Date.now() - startTime,
              duration_api_ms: getTotalAPIDuration(),
              is_error: true,
              num_turns: message.attachment.turnCount,
              stop_reason: lastStopReason,
              session_id: getSessionId(),
              total_cost_usd: getTotalCost(),
              usage: this.totalUsage,
              modelUsage: getModelUsage(),
              permission_denials: this.permissionDenials,
              fast_mode_state: getFastModeState(
                mainLoopModel,
                initialAppState.fastMode,
              ),
              uuid: randomUUID(),
              errors: [
                `Reached maximum number of turns (${message.attachment.maxTurns})`,
              ],
            }
            return
          }
          // Yield queued_command attachments as SDK user message replays
          else if (
            replayUserMessages &&
            message.attachment.type === 'queued_command'
          ) {
            yield {
              type: 'user',
              message: {
                role: 'user' as const,
                content: message.attachment.prompt,
              },
              session_id: getSessionId(),
              parent_tool_use_id: null,
              uuid: message.attachment.source_uuid || message.uuid,
              timestamp: message.timestamp,
              isReplay: true,
            } as SDKUserMessageReplay
          }
          break
        case 'stream_request_start':
          // Don't yield stream request start messages
          break
        case 'system': {
          // Snip boundary: replay on our store to remove zombie messages and
          // stale markers. The yielded boundary is a signal, not data to push —
          // the replay produces its own equivalent boundary. Without this,
          // markers persist and re-trigger on every turn, and mutableMessages
          // never shrinks (memory leak in long SDK sessions). The subtype
          // check lives inside the injected callback so feature-gated strings
          // stay out of this file (excluded-strings check).
          const snipResult = this.config.snipReplay?.(
            message,
            this.mutableMessages,
          )
          if (snipResult !== undefined) {
            if (snipResult.executed) {
              this.mutableMessages.length = 0
              this.mutableMessages.push(...snipResult.messages)
            }
            break
          }
          this.mutableMessages.push(message)
          // Yield compact boundary messages to SDK
          if (
            message.subtype === 'compact_boundary' &&
            message.compactMetadata
          ) {
            // Release pre-compaction messages for GC. The boundary was just
            // pushed so it's the last element. query.ts already uses
            // getMessagesAfterCompactBoundary() internally, so only
            // post-boundary messages are needed going forward.
            const mutableBoundaryIdx = this.mutableMessages.length - 1
            if (mutableBoundaryIdx > 0) {
              this.mutableMessages.splice(0, mutableBoundaryIdx)
            }
            const localBoundaryIdx = messages.length - 1
            if (localBoundaryIdx > 0) {
              messages.splice(0, localBoundaryIdx)
            }

            yield {
              type: 'system',
              subtype: 'compact_boundary' as const,
              session_id: getSessionId(),
              uuid: message.uuid,
              compact_metadata: toSDKCompactMetadata(message.compactMetadata),
            }
          }
          if (message.subtype === 'api_error') {
            yield {
              type: 'system',
              subtype: 'api_retry' as const,
              attempt: message.retryAttempt,
              max_retries: message.maxRetries,
              retry_delay_ms: message.retryInMs,
              error_status: message.error.status ?? null,
              error: categorizeRetryableAPIError(message.error),
              session_id: getSessionId(),
              uuid: message.uuid,
            }
          }
          // Don't yield other system messages in headless mode
          break
        }
        case 'tool_use_summary':
          // Yield tool use summary messages to SDK
          yield {
            type: 'tool_use_summary' as const,
            summary: message.summary,
            preceding_tool_use_ids: message.precedingToolUseIds,
            session_id: getSessionId(),
            uuid: message.uuid,
          }
          break
      }

      // Check if USD budget has been exceeded
      if (maxBudgetUsd !== undefined && getTotalCost() >= maxBudgetUsd) {
        if (persistSession) {
          if (
            isEnvTruthy(process.env.CRABCODE_EAGER_FLUSH) ||
            isEnvTruthy(process.env.CRABCODE_IS_COWORK)
          ) {
            await flushSessionStorage()
          }
        }
        yield {
          type: 'result',
          subtype: 'error_max_budget_usd',
          duration_ms: Date.now() - startTime,
          duration_api_ms: getTotalAPIDuration(),
          is_error: true,
          num_turns: turnCount,
          stop_reason: lastStopReason,
          session_id: getSessionId(),
          total_cost_usd: getTotalCost(),
          usage: this.totalUsage,
          modelUsage: getModelUsage(),
          permission_denials: this.permissionDenials,
          fast_mode_state: getFastModeState(
            mainLoopModel,
            initialAppState.fastMode,
          ),
          uuid: randomUUID(),
          errors: [`Reached maximum budget ($${maxBudgetUsd})`],
        }
        return
      }

      // Check if structured output retry limit exceeded (only on user messages)
      if (message.type === 'user' && jsonSchema) {
        const currentCalls = countToolCalls(
          this.mutableMessages,
          SYNTHETIC_OUTPUT_TOOL_NAME,
        )
        const callsThisQuery = currentCalls - initialStructuredOutputCalls
        const maxRetries = parseInt(
          process.env.MAX_STRUCTURED_OUTPUT_RETRIES || '5',
          10,
        )
        if (callsThisQuery >= maxRetries) {
          if (persistSession) {
            if (
              isEnvTruthy(process.env.CRABCODE_EAGER_FLUSH) ||
              isEnvTruthy(process.env.CRABCODE_IS_COWORK)
            ) {
              await flushSessionStorage()
            }
          }
          yield {
            type: 'result',
            subtype: 'error_max_structured_output_retries',
            duration_ms: Date.now() - startTime,
            duration_api_ms: getTotalAPIDuration(),
            is_error: true,
            num_turns: turnCount,
            stop_reason: lastStopReason,
            session_id: getSessionId(),
            total_cost_usd: getTotalCost(),
            usage: this.totalUsage,
            modelUsage: getModelUsage(),
            permission_denials: this.permissionDenials,
            fast_mode_state: getFastModeState(
              mainLoopModel,
              initialAppState.fastMode,
            ),
            uuid: randomUUID(),
            errors: [
              `Failed to provide valid structured output after ${maxRetries} attempts`,
            ],
          }
          return
        }
      }
    }

    // Stop hooks yield progress/attachment messages AFTER the assistant
    // response (via yield* handleStopHooks in query.ts). Since #23537 pushes
    // those to `messages` inline, last(messages) can be a progress/attachment
    // instead of the assistant — which makes textResult extraction below
    // return '' and -p mode emit a blank line. Allowlist to assistant|user:
    // isResultSuccessful handles both (user with all tool_result blocks is a
    // valid successful terminal state).
    const result = messages.findLast(
      m => m.type === 'assistant' || m.type === 'user',
    )
    // Capture for the error_during_execution diagnostic — isResultSuccessful
    // is a type predicate (message is Message), so inside the false branch
    // `result` narrows to never and these accesses don't typecheck.
    const edeResultType = result?.type ?? 'undefined'
    const edeLastContentType =
      result?.type === 'assistant'
        ? (last(result.message.content)?.type ?? 'none')
        : 'n/a'

    // Flush buffered transcript writes before yielding result.
    // The desktop app kills the CLI process immediately after receiving the
    // result message, so any unflushed writes would be lost.
    if (persistSession) {
      if (
        isEnvTruthy(process.env.CRABCODE_EAGER_FLUSH) ||
        isEnvTruthy(process.env.CRABCODE_IS_COWORK)
      ) {
        await flushSessionStorage()
      }
    }

    if (!isResultSuccessful(result, lastStopReason)) {
      yield {
        type: 'result',
        subtype: 'error_during_execution',
        duration_ms: Date.now() - startTime,
        duration_api_ms: getTotalAPIDuration(),
        is_error: true,
        num_turns: turnCount,
        stop_reason: lastStopReason,
        session_id: getSessionId(),
        total_cost_usd: getTotalCost(),
        usage: this.totalUsage,
        modelUsage: getModelUsage(),
        permission_denials: this.permissionDenials,
        fast_mode_state: getFastModeState(
          mainLoopModel,
          initialAppState.fastMode,
        ),
        uuid: randomUUID(),
        // Diagnostic prefix: these are what isResultSuccessful() checks — if
        // the result type isn't assistant-with-text/thinking or user-with-
        // tool_result, and stop_reason isn't end_turn, that's why this fired.
        // errors[] is turn-scoped via the watermark; previously it dumped the
        // entire process's logError buffer (ripgrep timeouts, ENOENT, etc).
        errors: (() => {
          const all = getInMemoryErrors()
          const start = errorLogWatermark
            ? all.lastIndexOf(errorLogWatermark) + 1
            : 0
          return [
            `[ede_diagnostic] result_type=${edeResultType} last_content_type=${edeLastContentType} stop_reason=${lastStopReason}`,
            ...all.slice(start).map(_ => _.error),
          ]
        })(),
      }
      return
    }

    // Extract the text result based on message type
    let textResult = ''
    let isApiError = false

    if (result.type === 'assistant') {
      const lastContent = last(result.message.content)
      if (
        lastContent?.type === 'text' &&
        !SYNTHETIC_MESSAGES.has(lastContent.text)
      ) {
        textResult = lastContent.text
      }
      isApiError = Boolean(result.isApiErrorMessage)
    }

    yield {
      type: 'result',
      subtype: 'success',
      is_error: isApiError,
      duration_ms: Date.now() - startTime,
      duration_api_ms: getTotalAPIDuration(),
      num_turns: turnCount,
      result: textResult,
      stop_reason: lastStopReason,
      session_id: getSessionId(),
      total_cost_usd: getTotalCost(),
      usage: this.totalUsage,
      modelUsage: getModelUsage(),
      permission_denials: this.permissionDenials,
      structured_output: structuredOutputFromTool,
      fast_mode_state: getFastModeState(
        mainLoopModel,
        initialAppState.fastMode,
      ),
      uuid: randomUUID(),
    }
  }

  interrupt(): void {
    this.abortController.abort()
  }

  getMessages(): readonly Message[] {
    return this.mutableMessages
  }

  getReadFileState(): FileStateCache {
    return this.readFileState
  }

  getSessionId(): string {
    return getSessionId()
  }

  setModel(model: string): void {
    this.config.userSpecifiedModel = model
  }
}

/**
 * Sends one prompt through a caller-supplied QueryEngine context and returns
 * its SDK projection. Standard print/SDK callers remain noninteractive; the
 * native direct route explicitly supplies its interactive callbacks.
 *
 * Convenience wrapper around QueryEngine for one-shot usage.
 */
export async function* ask({
  commands,
  prompt,
  promptUuid,
  isMeta,
  inputMode,
  cwd,
  tools,
  mcpClients,
  verbose = false,
  thinkingConfig,
  crabcodeThinkingMode,
  accountBridgeRuntimeAccess,
  maxTurns,
  maxBudgetUsd,
  taskBudget,
  canUseTool,
  mutableMessages = [],
  getReadFileCache,
  setReadFileCache,
  customSystemPrompt,
  appendSystemPrompt,
  userSpecifiedModel,
  fallbackModel,
  jsonSchema,
  getAppState,
  setAppState,
  abortController,
  replayUserMessages = false,
  includePartialMessages = false,
  handleElicitation,
  agents = [],
  mainThreadAgentDefinition,
  setSDKStatus,
  orphanedPermission,
  interactive,
  allowDirectTuiBashContentBlocks,
  querySource,
  onQueryEvent,
  sendOSNotification,
}: {
  commands: Command[]
  prompt: string | Array<ContentBlockParam>
  promptUuid?: string
  isMeta?: boolean
  inputMode?: QueryEngineInputMode
  cwd: string
  tools: Tools
  verbose?: boolean
  mcpClients: MCPServerConnection[]
  thinkingConfig?: ThinkingConfig
  crabcodeThinkingMode?: CrabCodeThinkingMode
  accountBridgeRuntimeAccess?: AccountBridgeRuntimeAccess
  maxTurns?: number
  maxBudgetUsd?: number
  taskBudget?: { total: number }
  canUseTool: CanUseToolFn
  mutableMessages?: Message[]
  customSystemPrompt?: string
  appendSystemPrompt?: string
  userSpecifiedModel?: string
  fallbackModel?: string
  jsonSchema?: Record<string, unknown>
  getAppState: () => AppState
  setAppState: (f: (prev: AppState) => AppState) => void
  getReadFileCache: () => FileStateCache
  setReadFileCache: (cache: FileStateCache) => void
  abortController?: AbortController
  replayUserMessages?: boolean
  includePartialMessages?: boolean
  handleElicitation?: ToolUseContext['handleElicitation']
  agents?: AgentDefinition[]
  mainThreadAgentDefinition?: AgentDefinition
  setSDKStatus?: (status: SDKStatus) => void
  orphanedPermission?: OrphanedPermission
  interactive?: boolean
  allowDirectTuiBashContentBlocks?: boolean
  querySource?: QuerySource
  onQueryEvent?: (event: Message) => void
  sendOSNotification?: ToolUseContext['sendOSNotification']
}): AsyncGenerator<SDKMessage, void, unknown> {
  const engine = new QueryEngine({
    cwd,
    tools,
    commands,
    mcpClients,
    agents,
    mainThreadAgentDefinition,
    canUseTool,
    getAppState,
    setAppState,
    initialMessages: mutableMessages,
    readFileCache: cloneFileStateCache(getReadFileCache()),
    customSystemPrompt,
    appendSystemPrompt,
    userSpecifiedModel,
    fallbackModel,
    thinkingConfig,
    crabcodeThinkingMode,
    accountBridgeRuntimeAccess,
    maxTurns,
    maxBudgetUsd,
    taskBudget,
    jsonSchema,
    verbose,
    handleElicitation,
    replayUserMessages,
    includePartialMessages,
    setSDKStatus,
    abortController,
    orphanedPermission,
    interactive,
    allowDirectTuiBashContentBlocks,
    querySource,
    onQueryEvent,
    sendOSNotification,
    ...(feature('HISTORY_SNIP')
      ? {
          snipReplay: (yielded: Message, store: Message[]) => {
            if (!snipProjection!.isSnipBoundaryMessage(yielded))
              return undefined
            return snipModule!.snipCompactIfNeeded(store, { force: true })
          },
        }
      : {}),
  })

  try {
    yield* engine.submitMessage(prompt, {
      uuid: promptUuid,
      isMeta,
      inputMode,
    })
  } finally {
    setReadFileCache(engine.getReadFileState())
  }
}
