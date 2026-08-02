import type { ToolResultBlockParam, ToolUseBlock } from './types/api-types.js'
import type { CanUseToolFn } from './types/canUseTool.js'
import { FallbackTriggeredError } from './services/api/withRetry.js'
import {
  calculateTokenWarningState,
  isAutoCompactEnabled,
  type AutoCompactTrackingState,
} from './services/compact/autoCompact.js'
import { buildPostCompactMessages } from './services/compact/compact.js'
/* eslint-disable @typescript-eslint/no-require-imports */
const reactiveCompact = feature('REACTIVE_COMPACT')
  ? (require('./services/compact/reactiveCompact.js') as typeof import('./services/compact/reactiveCompact.js'))
  : null
const contextCollapse = feature('CONTEXT_COLLAPSE')
  ? (require('./services/contextCollapse/index.js') as typeof import('./services/contextCollapse/index.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */
import {
  logEvent,
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
} from 'src/services/analytics/index.js'
import { ImageSizeError } from './utils/imageValidation.js'
import { ImageResizeError } from './utils/imageResizer.js'
import { findToolByName, type ToolUseContext } from './Tool.js'
import { asSystemPrompt, type SystemPrompt } from './utils/systemPromptType.js'
import type {
  AssistantMessage,
  AttachmentMessage,
  Message,
  RequestStartEvent,
  StreamEvent,
  ToolUseSummaryMessage,
  UserMessage,
  TombstoneMessage,
} from './types/message.js'
import { logError } from './utils/log.js'
import {
  PROMPT_TOO_LONG_ERROR_MESSAGE,
  isPromptTooLongMessage,
} from './services/api/errors.js'
import { logAntError, logForDebugging } from './utils/debug.js'
import {
  createUserMessage,
  createUserInterruptionMessage,
  normalizeMessagesForAPI,
  createSystemMessage,
  createAssistantAPIErrorMessage,
  getMessagesAfterCompactBoundary,
  createToolUseSummaryMessage,
  createMicrocompactBoundaryMessage,
  stripSignatureBlocks,
} from './utils/messages.js'
import { generateToolUseSummary } from './services/toolUseSummary/toolUseSummaryGenerator.js'
import { prependUserContext, appendSystemContext } from './utils/api.js'
import {
  createAttachmentMessage,
  filterDuplicateMemoryAttachments,
  getAttachmentMessages,
  startRelevantMemoryPrefetch,
} from './utils/attachments.js'
/* eslint-disable @typescript-eslint/no-require-imports */
const skillPrefetch = feature('EXPERIMENTAL_SKILL_SEARCH')
  ? (require('./services/skillSearch/prefetch.js') as typeof import('./services/skillSearch/prefetch.js'))
  : null
const jobClassifier = feature('TEMPLATES')
  ? (require('./jobs/classifier.js') as typeof import('./jobs/classifier.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */
import {
  remove as removeFromQueue,
  getCommandsByMaxPriority,
  isSlashCommand,
  isTaskNotificationClaimed,
} from './utils/messageQueueManager.js'
import { notifyCommandLifecycle } from './utils/commandLifecycle.js'
import { headlessProfilerCheckpoint } from './utils/headlessProfiler.js'
import {
  getRuntimeMainLoopModel,
  renderModelName,
} from './utils/model/model.js'
import {
  doesMostRecentAssistantMessageExceed200k,
  finalContextTokensFromLastResponse,
  tokenCountWithEstimation,
} from './utils/tokens.js'
import { ESCALATED_MAX_TOKENS } from './utils/context.js'
import { isWithheldMaxOutputTokens } from './services/agentLoop/withheldMaxOutputTokens.js'
import { getMaxOutputTokensForModel } from './services/api/params.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from './services/analytics/growthbook.js'
import { SLEEP_TOOL_NAME } from './tools/SleepTool/prompt.js'
import { executePostSamplingHooks } from './utils/hooks/postSamplingHooks.js'
import { executeStopFailureHooks } from './utils/hooks.js'
import type { QuerySource } from './constants/querySource.js'
import { createDumpPromptsFetch } from './services/api/dumpPrompts.js'
import { StreamingToolExecutor } from './services/tools/StreamingToolExecutor.js'
import { queryCheckpoint } from './utils/queryProfiler.js'
import { runTools } from './services/tools/toolOrchestration.js'
import {
  buildToolInputQuarantineError,
  decideToolInputQuarantine,
  getToolInputQuarantineReason,
  isMalformedToolInput,
  type ToolInputQuarantineReason,
} from './services/tools/toolInputQuarantine.js'
import {
  summarizeTurnToolResults,
  TERMINAL_CIRCUIT_BREAK_THRESHOLD,
} from './services/tools/terminalToolError.js'
import {
  autoInjectContentKey,
  AUTO_INJECT_REPEAT_LIMIT,
  isInteractiveMainLoopSource,
  NO_PROGRESS_TURN_LIMIT,
  resolveInteractiveMaxTurns,
} from './services/agentLoop/serialDepthBudget.js'
import { applyToolResultBudget } from './utils/toolResultStorage.js'
import { recordContentReplacement } from './utils/sessionStorage.js'
import { handleStopHooks } from './query/stopHooks.js'
import { buildQueryConfig } from './query/config.js'
import { productionDeps, type QueryDeps } from './query/deps.js'
import type { Terminal, Continue } from './query/transitions.js'
import { feature } from './utils/featurePolyfill.js'
import {
  getCurrentTurnThreadId,
  getCurrentTurnTokenBudget,
  getTurnOutputTokens,
  incrementBudgetContinuationCount,
} from './bootstrap/state.js'
import { createBudgetTracker, checkTokenBudget } from './query/tokenBudget.js'
import { count } from './utils/array.js'

/* eslint-disable @typescript-eslint/no-require-imports */
const snipModule = feature('HISTORY_SNIP')
  ? (require('./services/compact/snipCompact.js') as typeof import('./services/compact/snipCompact.js'))
  : null
const taskSummaryModule = feature('BG_SESSIONS')
  ? (require('./utils/taskSummary.js') as typeof import('./utils/taskSummary.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

function* yieldMissingToolResultBlocks(
  assistantMessages: AssistantMessage[],
  errorMessage: string,
) {
  for (const assistantMessage of assistantMessages) {
    // Extract all tool use blocks from this assistant message
    const toolUseBlocks = assistantMessage.message.content.filter(
      content => content.type === 'tool_use',
    ) as ToolUseBlock[]

    // Emit an interruption message for each tool use
    for (const toolUse of toolUseBlocks) {
      yield createUserMessage({
        content: [
          {
            type: 'tool_result',
            content: errorMessage,
            is_error: true,
            tool_use_id: toolUse.id,
          },
        ],
        toolUseResult: errorMessage,
        sourceToolAssistantUUID: assistantMessage.uuid,
      })
    }
  }
}

type QuarantinedToolUse = {
  block: ToolUseBlock
  assistantMessage: AssistantMessage
  reason: ToolInputQuarantineReason
}

type MediaSidecarUsage = NonNullable<AssistantMessage['mediaSidecarUsed']>

function mergeMediaSidecarUsage(
  current: MediaSidecarUsage | undefined,
  next: MediaSidecarUsage | undefined,
): MediaSidecarUsage | undefined {
  if (!next) return current
  if (!current) return next
  // PR-R2a: per-kind reason tallies merge by key; omitted on both sides stays
  // omitted (legacy transcripts must not grow an empty record on merge).
  let placeholderFallbackKinds: Record<string, number> | undefined
  if (current.placeholderFallbackKinds || next.placeholderFallbackKinds) {
    placeholderFallbackKinds = { ...(current.placeholderFallbackKinds ?? {}) }
    for (const [kind, count] of Object.entries(
      next.placeholderFallbackKinds ?? {},
    )) {
      placeholderFallbackKinds[kind] =
        (placeholderFallbackKinds[kind] ?? 0) + count
    }
  }
  return {
    sidecarDescribed: current.sidecarDescribed + next.sidecarDescribed,
    placeholderFallback:
      current.placeholderFallback + next.placeholderFallback,
    freshDescriptions: current.freshDescriptions + next.freshDescriptions,
    memoryCacheHits: current.memoryCacheHits + next.memoryCacheHits,
    diskCacheHits: current.diskCacheHits + next.diskCacheHits,
    placeholderFallbacks:
      current.placeholderFallbacks + next.placeholderFallbacks,
    ...(placeholderFallbackKinds ? { placeholderFallbackKinds } : {}),
  }
}

/**
 * The rules of thinking are lengthy and fortuitous. They require plenty of thinking
 * of most long duration and deep meditation for a wizard to wrap one's noggin around.
 *
 * The rules follow:
 * 1. A message that contains a thinking or redacted_thinking block must be part of a query whose max_thinking_length > 0
 * 2. A thinking block may not be the last message in a block
 * 3. Thinking blocks must be preserved for the duration of an assistant trajectory (a single turn, or if that turn includes a tool_use block then also its subsequent tool_result and the following assistant message)
 *
 * Heed these rules well, young wizard. For they are the rules of thinking, and
 * the rules of thinking are the rules of the universe. If ye does not heed these
 * rules, ye will be punished with an entire day of debugging and hair pulling.
 */
const MAX_OUTPUT_TOKENS_RECOVERY_LIMIT = 3

// isWithheldMaxOutputTokens moved to services/agentLoop/withheldMaxOutputTokens.ts
// (2026-07-17 audit RC-2: predicate must read the `error` field the producer
// actually sets; extracted so the producer↔predicate bridge test can import it
// without dragging in this module's world).

export type QueryParams = {
  messages: Message[]
  systemPrompt: SystemPrompt
  userContext: { [k: string]: string }
  systemContext: { [k: string]: string }
  canUseTool: CanUseToolFn
  toolUseContext: ToolUseContext
  fallbackModel?: string
  querySource: QuerySource
  maxOutputTokensOverride?: number
  maxTurns?: number
  /**
   * Runtime-only hard cost boundary for workflow agent lineages. The callback
   * returns an Error once no further model request may start. It is checked
   * before and after each observable model response; a single in-flight
   * response may cross the threshold because its final cost is unknowable at
   * dispatch time.
   */
  modelBudgetGuard?: () => Error | undefined
  /**
   * W-AGENT-LOOP-CIRCUIT-BREAKER: marks an open-ended INTERACTIVE session whose
   * runaway would surprise a watching human. When true and no
   * explicit maxTurns is set, the loop applies the interactive safety ceiling
   * (resolveInteractiveMaxTurns). The TUI REPL is detected via querySource
   * instead, so it needn't set this. headless --print / subagents leave it unset.
   */
  isInteractive?: boolean
  skipCacheWrite?: boolean
  // API task_budget (output_config.task_budget, beta task-budgets-2026-03-13).
  // Distinct from the tokenBudget +500k auto-continue feature. `total` is the
  // budget for the whole agentic turn; `remaining` is computed per iteration
  // from cumulative API usage. See configureTaskBudgetParams in crabcode.ts.
  taskBudget?: { total: number }
  deps?: QueryDeps
  /**
   * W-STEER-MIDTURN (2026-06-25): per-turn mid-turn steer injection provider.
   * Called at each tool-round boundary (after tool execution, before the next
   * LLM call — the SAME point the process-global queue drains). Returns text
   * prompt(s) to splice into the current turn as `queued_command` attachments.
   * Sourced from a TURN-SCOPED closure (worker drains its own (threadId,turnId)
   * pending-steer queue) so concurrent cross-thread turns in one worker process
   * never contaminate each other — unlike feeding the process-global
   * messageQueueManager, which has no turnId dimension (audit P2). The closure
   * also emits the user bubble before returning. `[]` / absent → no-op.
   */
  midTurnSteerProvider?: () => string[]
}

// -- query loop state

// Mutable state carried between loop iterations
type State = {
  messages: Message[]
  toolUseContext: ToolUseContext
  autoCompactTracking: AutoCompactTrackingState | undefined
  maxOutputTokensRecoveryCount: number
  hasAttemptedReactiveCompact: boolean
  maxOutputTokensOverride: number | undefined
  pendingToolUseSummary: Promise<ToolUseSummaryMessage | null> | undefined
  stopHookActive: boolean | undefined
  turnCount: number
  // Why the previous iteration continued. Undefined on first iteration.
  // Lets tests assert recovery paths fired without inspecting message contents.
  transition: Continue | undefined
}

export async function* query(
  params: QueryParams,
): AsyncGenerator<
  | StreamEvent
  | RequestStartEvent
  | Message
  | TombstoneMessage
  | ToolUseSummaryMessage,
  Terminal
> {
  const consumedCommandUuids: string[] = []
  const terminal = yield* queryLoop(params, consumedCommandUuids)
  // Only reached if queryLoop returned normally. Skipped on throw (error
  // propagates through yield*) and on .return() (Return completion closes
  // both generators). This gives the same asymmetric started-without-completed
  // signal as print.ts's drainCommandQueue when the turn fails.
  for (const uuid of consumedCommandUuids) {
    notifyCommandLifecycle(uuid, 'completed')
  }
  return terminal
}

async function* queryLoop(
  params: QueryParams,
  consumedCommandUuids: string[],
): AsyncGenerator<
  | StreamEvent
  | RequestStartEvent
  | Message
  | TombstoneMessage
  | ToolUseSummaryMessage,
  Terminal
> {
  // Immutable params — never reassigned during the query loop.
  const {
    systemPrompt,
    userContext,
    systemContext,
    canUseTool,
    fallbackModel,
    querySource,
    maxTurns,
    skipCacheWrite,
  } = params
  const deps = params.deps ?? productionDeps()

  // W-AGENT-LOOP-CIRCUIT-BREAKER: resolve the effective turn ceiling once at
  // loop entry (single chokepoint — avoids per-entry drift into dead code). An
  // explicit caller maxTurns always wins. Otherwise open-ended interactive main
  // interactive loops (TUI REPL via querySource/params.isInteractive) get a
  // high safety ceiling; headless --print / subagents / background utility
  // queries stay unbounded-by-turns as before.
  const effectiveMaxTurns =
    maxTurns ??
    (params.isInteractive || isInteractiveMainLoopSource(querySource)
      ? resolveInteractiveMaxTurns()
      : undefined)

  // Mutable cross-iteration state. The loop body destructures this at the top
  // of each iteration so reads stay bare-name (`messages`, `toolUseContext`).
  // Continue sites write `state = { ... }` instead of 9 separate assignments.
  let state: State = {
    messages: params.messages,
    toolUseContext: params.toolUseContext,
    maxOutputTokensOverride: params.maxOutputTokensOverride,
    autoCompactTracking: undefined,
    stopHookActive: undefined,
    maxOutputTokensRecoveryCount: 0,
    hasAttemptedReactiveCompact: false,
    turnCount: 1,
    pendingToolUseSummary: undefined,
    transition: undefined,
  }
  const budgetTracker = feature('TOKEN_BUDGET') ? createBudgetTracker() : null

  // task_budget.remaining tracking across compaction boundaries. Undefined
  // until first compact fires — while context is uncompacted the server can
  // see the full history and handles the countdown from {total} itself (see
  // api/api/sampling/prompt/renderer.py:292). After a compact, the server sees
  // only the summary and would under-count spend; remaining tells it the
  // pre-compact final window that got summarized away. Cumulative across
  // multiple compacts: each subtracts the final context at that compact's
  // trigger point. Loop-local (not on State) to avoid touching the 7 continue
  // sites.
  let taskBudgetRemaining: number | undefined = undefined

  // W-AGENT-LOOP-CIRCUIT-BREAKER P1-A no-progress watchdog: consecutive turns
  // where EVERY tool_result was is_error (zero successful output). Loop-local
  // (like taskBudgetRemaining) so it survives across iterations without touching
  // the State type or the continue sites. Reset on any successful tool result.
  let consecutiveAllErrorTurns = 0

  // W-CRON-SESSION-INJECTION-ISOLATION L5: within-loop auto-injection runaway
  // guard state. Tracks the content key of the last auto-injected (isMeta)
  // drain and how many consecutive turns drained the SAME one. Loop-local (like
  // consecutiveAllErrorTurns) so it resets when this loop returns to idle — a
  // legitimately recurring cron therefore never accumulates across scheduled
  // fires. See AUTO_INJECT_REPEAT_LIMIT.
  let lastAutoInjectContentKey: string | null = null
  let consecutiveSameAutoInject = 0

  // Snapshot immutable env/statsig/session state once at entry. See QueryConfig
  // for what's included and why feature() gates are intentionally excluded.
  const config = buildQueryConfig()

  // Fired once per user turn — the prompt is invariant across loop iterations,
  // so per-iteration firing would ask sideQuery the same question N times.
  // Consume point polls settledAt (never blocks). `using` disposes on all
  // generator exit paths — see MemoryPrefetch for dispose/telemetry semantics.
  using pendingMemoryPrefetch = state.toolUseContext.suppressUntrustedHooks
    ? null
    : startRelevantMemoryPrefetch(state.messages, state.toolUseContext)

  // eslint-disable-next-line no-constant-condition
  while (true) {
    const preRequestBudgetError = params.modelBudgetGuard?.()
    if (preRequestBudgetError) throw preRequestBudgetError

    // Destructure state at the top of each iteration. toolUseContext alone
    // is reassigned within an iteration (queryTracking, messages updates);
    // the rest are read-only between continue sites.
    let { toolUseContext } = state
    const {
      messages,
      autoCompactTracking,
      maxOutputTokensRecoveryCount,
      hasAttemptedReactiveCompact,
      maxOutputTokensOverride,
      pendingToolUseSummary,
      stopHookActive,
      turnCount,
    } = state

    // Skill discovery prefetch — per-iteration (uses findWritePivot guard
    // that returns early on non-write iterations). Discovery runs while the
    // model streams and tools execute; awaited post-tools alongside the
    // memory prefetch consume. Replaces the blocking assistant_turn path
    // that ran inside getAttachmentMessages (97% of those calls found
    // nothing in prod). Turn-0 user-input discovery still blocks in
    // userInputAttachments — that's the one signal where there's no prior
    // work to hide under.
    const pendingSkillPrefetch = toolUseContext.suppressUntrustedHooks
      ? null
      : skillPrefetch?.startSkillDiscoveryPrefetch(
          null,
          messages,
          toolUseContext,
        )

    yield { type: 'stream_request_start' }

    queryCheckpoint('query_fn_entry')

    // Record query start for headless latency tracking (skip for subagents)
    if (!toolUseContext.agentId) {
      headlessProfilerCheckpoint('query_started')
    }

    // Initialize or increment query chain tracking
    const queryTracking = toolUseContext.queryTracking
      ? {
          chainId: toolUseContext.queryTracking.chainId,
          depth: toolUseContext.queryTracking.depth + 1,
        }
      : {
          chainId: deps.uuid(),
          depth: 0,
        }

    const queryChainIdForAnalytics =
      queryTracking.chainId as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS

    toolUseContext = {
      ...toolUseContext,
      queryTracking,
    }
    const suppressAmbientContext = toolUseContext.suppressUntrustedHooks === true

    let messagesForQuery = [...getMessagesAfterCompactBoundary(messages)]

    let tracking = autoCompactTracking

    // Enforce per-message budget on aggregate tool result size. Runs BEFORE
    // microcompact — cached MC operates purely by tool_use_id (never inspects
    // content), so content replacement is invisible to it and the two compose
    // cleanly. No-ops when contentReplacementState is undefined (feature off).
    // Persist only for querySources that read records back on resume: agentId
    // routes to sidechain file (AgentTool resume) or session file (/resume).
    // Ephemeral runForkedAgent callers (agent_summary etc.) don't persist.
    const persistReplacements =
      querySource.startsWith('agent:') ||
      querySource.startsWith('repl_main_thread')
    if (!suppressAmbientContext) {
      messagesForQuery = await applyToolResultBudget(
        messagesForQuery,
        toolUseContext.contentReplacementState,
        persistReplacements
          ? (records) =>
              void recordContentReplacement(
                records,
                toolUseContext.agentId,
              ).catch(logError)
          : undefined,
        new Set(
          toolUseContext.options.tools
            .filter((t) => !Number.isFinite(t.maxResultSizeChars))
            .map((t) => t.name),
        ),
      )
    }

    // Apply snip before microcompact (both may run — they are not mutually exclusive).
    // snipTokensFreed is plumbed to autocompact so its threshold check reflects
    // what snip removed; tokenCountWithEstimation alone can't see it (reads usage
    // from the protected-tail assistant, which survives snip unchanged).
    let snipTokensFreed = 0
    if (!suppressAmbientContext && feature('HISTORY_SNIP')) {
      queryCheckpoint('query_snip_start')
      const snipResult = snipModule!.snipCompactIfNeeded(messagesForQuery)
      messagesForQuery = snipResult.messages
      snipTokensFreed = snipResult.tokensFreed
      if (snipResult.boundaryMessage) {
        yield snipResult.boundaryMessage as import('./types/message.js').Message
      }
      queryCheckpoint('query_snip_end')
    }

    // Apply microcompact before autocompact
    queryCheckpoint('query_microcompact_start')
    const microcompactResult = suppressAmbientContext
      ? { messages: messagesForQuery, compactionInfo: undefined }
      : await deps.microcompact(
          messagesForQuery,
          toolUseContext,
          querySource,
        )
    messagesForQuery = microcompactResult.messages
    // For cached microcompact (cache editing), defer boundary message until after
    // the API response so we can use actual cache_deleted_input_tokens.
    // Gated behind feature() so the string is eliminated from external builds.
    const pendingCacheEdits = feature('CACHED_MICROCOMPACT')
      ? microcompactResult.compactionInfo?.pendingCacheEdits
      : undefined
    queryCheckpoint('query_microcompact_end')

    // Project the collapsed context view and maybe commit more collapses.
    // Runs BEFORE autocompact so that if collapse gets us under the
    // autocompact threshold, autocompact is a no-op and we keep granular
    // context instead of a single summary.
    //
    // Nothing is yielded — the collapsed view is a read-time projection
    // over the REPL's full history. Summary messages live in the collapse
    // store, not the REPL array. This is what makes collapses persist
    // across turns: projectView() replays the commit log on every entry.
    // Within a turn, the view flows forward via state.messages at the
    // continue site (query.ts:1192), and the next projectView() no-ops
    // because the archived messages are already gone from its input.
    if (
      !suppressAmbientContext &&
      feature('CONTEXT_COLLAPSE') &&
      contextCollapse
    ) {
      const collapseResult = await contextCollapse.applyCollapsesIfNeeded(
        messagesForQuery,
        toolUseContext,
        querySource,
      )
      messagesForQuery = collapseResult.messages
    }

    const fullSystemPrompt = asSystemPrompt(
      appendSystemContext(systemPrompt, systemContext),
    )

    queryCheckpoint('query_autocompact_start')
    const { compactionResult, consecutiveFailures } =
      toolUseContext.suppressUntrustedHooks
        ? { compactionResult: null, consecutiveFailures: 0 }
        : await deps.autocompact(
            messagesForQuery,
            toolUseContext,
            {
              systemPrompt,
              userContext,
              systemContext,
              toolUseContext,
              forkContextMessages: messagesForQuery,
            },
            querySource,
            tracking,
            snipTokensFreed,
          )
    queryCheckpoint('query_autocompact_end')

    if (compactionResult) {
      const {
        preCompactTokenCount,
        postCompactTokenCount,
        truePostCompactTokenCount,
        compactionUsage,
      } = compactionResult

      logEvent('tengu_auto_compact_succeeded', {
        originalMessageCount: messages.length,
        compactedMessageCount:
          compactionResult.summaryMessages.length +
          compactionResult.attachments.length +
          compactionResult.hookResults.length,
        preCompactTokenCount,
        postCompactTokenCount,
        truePostCompactTokenCount,
        compactionInputTokens: compactionUsage?.input_tokens,
        compactionOutputTokens: compactionUsage?.output_tokens,
        compactionCacheReadTokens:
          compactionUsage?.cache_read_input_tokens ?? 0,
        compactionCacheCreationTokens:
          compactionUsage?.cache_creation_input_tokens ?? 0,
        compactionTotalTokens: compactionUsage
          ? compactionUsage.input_tokens +
            (compactionUsage.cache_creation_input_tokens ?? 0) +
            (compactionUsage.cache_read_input_tokens ?? 0) +
            compactionUsage.output_tokens
          : 0,

        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })

      // task_budget: capture pre-compact final context window before
      // messagesForQuery is replaced with postCompactMessages below.
      // iterations[-1] is the authoritative final window (post server tool
      // loops); see #304930.
      if (params.taskBudget) {
        const preCompactContext =
          finalContextTokensFromLastResponse(messagesForQuery)
        taskBudgetRemaining = Math.max(
          0,
          (taskBudgetRemaining ?? params.taskBudget.total) - preCompactContext,
        )
      }

      // Reset on every compact so turnCounter/turnId reflect the MOST RECENT
      // compact. recompactionInfo (autoCompact.ts:190) already captured the
      // old values for turnsSincePreviousCompact/previousCompactTurnId before
      // the call, so this reset doesn't lose those.
      tracking = {
        compacted: true,
        turnId: deps.uuid(),
        turnCounter: 0,
        consecutiveFailures: 0,
      }

      const postCompactMessages = buildPostCompactMessages(compactionResult)

      for (const message of postCompactMessages) {
        yield message
      }

      // Continue on with the current query call using the post compact messages
      messagesForQuery = postCompactMessages
    } else if (consecutiveFailures !== undefined) {
      // Autocompact failed — propagate failure count so the circuit breaker
      // can stop retrying on the next iteration.
      tracking = {
        ...(tracking ?? { compacted: false, turnId: '', turnCounter: 0 }),
        consecutiveFailures,
      }
    }

    // Re-assign messages to ensure toolUseContext stays in sync.
    toolUseContext = {
      ...toolUseContext,
      messages: messagesForQuery,
    }

    const assistantMessages: AssistantMessage[] = []
    const toolResults: (UserMessage | AttachmentMessage)[] = []
    // @see https://acosmi.com/zh/docs/sdk-api/streaming
    // Note: stop_reason === 'tool_use' is unreliable -- it's not always set correctly.
    // Set during streaming whenever a tool_use block arrives — the sole
    // loop-exit signal. If false after streaming, we're done (modulo stop-hook retry).
    const toolUseBlocks: ToolUseBlock[] = []
    // R3: file mutations and malformed JSON stay out of the streaming
    // executor until message_delta has supplied authoritative usage/stop
    // metadata. This is the side-effect boundary, not merely a UI flag.
    const quarantinedToolUses: QuarantinedToolUse[] = []
    let needsFollowUp = false

    queryCheckpoint('query_setup_start')
    const useStreamingToolExecution = config.gates.streamingToolExecution
    let streamingToolExecutor = useStreamingToolExecution
      ? new StreamingToolExecutor(
          toolUseContext.options.tools,
          canUseTool,
          toolUseContext,
        )
      : null

    const appState = toolUseContext.getAppState()
    const permissionMode = appState.toolPermissionContext.mode
    let currentModel = getRuntimeMainLoopModel({
      permissionMode,
      mainLoopModel: toolUseContext.options.mainLoopModel,
      exceeds200kTokens:
        permissionMode === 'plan' &&
        doesMostRecentAssistantMessageExceed200k(messagesForQuery),
    })

    queryCheckpoint('query_setup_end')

    // Create fetch wrapper once per query session to avoid memory retention.
    // Each call to createDumpPromptsFetch creates a closure that captures the request body.
    // Creating it once means only the latest request body is retained (~700KB),
    // instead of all request bodies from the session (~500MB for long sessions).
    // Note: agentId is effectively constant during a query() call - it only changes
    // between queries (e.g., /clear command or session resume).
    const dumpPromptsFetch = config.gates.isAnt
      ? createDumpPromptsFetch(toolUseContext.agentId ?? config.sessionId)
      : undefined

    // Block if we've hit the hard blocking limit (only applies when auto-compact is OFF)
    // This reserves space so users can still run /compact manually
    // Skip this check if compaction just happened - the compaction result is already
    // validated to be under the threshold, and tokenCountWithEstimation would use
    // stale input_tokens from kept messages that reflect pre-compaction context size.
    // Same staleness applies to snip: subtract snipTokensFreed (otherwise we'd
    // falsely block in the window where snip brought us under autocompact threshold
    // but the stale usage is still above blocking limit — before this PR that
    // window never existed because autocompact always fired on the stale count).
    // Also skip for compact/session_memory queries — these are forked agents that
    // inherit the full conversation and would deadlock if blocked here (the compact
    // agent needs to run to REDUCE the token count).
    // Also skip when reactive compact is enabled and automatic compaction is
    // allowed — the preempt's synthetic error returns before the API call,
    // so reactive compact would never see a prompt-too-long to react to.
    // Widened to walrus so RC can act as fallback when proactive fails.
    //
    // Same skip for context-collapse: its recoverFromOverflow drains
    // staged collapses on a REAL API 413, then falls through to
    // reactiveCompact. A synthetic preempt here would return before the
    // API call and starve both recovery paths. The isAutoCompactEnabled()
    // conjunct preserves the user's explicit "no automatic anything"
    // config — if they set DISABLE_AUTO_COMPACT, they get the preempt.
    let collapseOwnsIt = false
    if (!suppressAmbientContext && feature('CONTEXT_COLLAPSE')) {
      collapseOwnsIt =
        (contextCollapse?.isContextCollapseEnabled() ?? false) &&
        isAutoCompactEnabled()
    }
    // Hoist media-recovery gate once per turn. Withholding (inside the
    // stream loop) and recovery (after) must agree; CACHED_MAY_BE_STALE can
    // flip during the 5-30s stream, and withhold-without-recover would eat
    // the message. PTL doesn't hoist because its withholding is ungated —
    // it predates the experiment and is already the control-arm baseline.
    const mediaRecoveryEnabled =
      !suppressAmbientContext &&
      (reactiveCompact?.isReactiveCompactEnabled() ?? false)
    if (
      !compactionResult &&
      querySource !== 'compact' &&
      querySource !== 'session_memory' &&
      !(
        !suppressAmbientContext &&
        reactiveCompact?.isReactiveCompactEnabled() &&
        isAutoCompactEnabled()
      ) &&
      !collapseOwnsIt
    ) {
      const { isAtBlockingLimit } = calculateTokenWarningState(
        tokenCountWithEstimation(messagesForQuery) - snipTokensFreed,
        toolUseContext.options.mainLoopModel,
      )
      if (isAtBlockingLimit) {
        yield createAssistantAPIErrorMessage({
          content: PROMPT_TOO_LONG_ERROR_MESSAGE,
          error: 'invalid_request',
        })
        return { reason: 'blocking_limit' }
      }
    }

    let attemptWithFallback = true
    // A primary-model attempt may already have made a fresh vision-sidecar
    // call before withRetry switches models. Carry that actual usage onto the
    // fallback response instead of losing it or showing only the later cache
    // hit performed by the fallback attempt.
    let pendingFallbackMediaUsage: MediaSidecarUsage | undefined

    queryCheckpoint('query_api_loop_start')
    let fallbackBudgetError: Error | undefined
    try {
      while (attemptWithFallback) {
        attemptWithFallback = false
        try {
          let streamingFallbackOccured = false
          queryCheckpoint('query_api_streaming_start')
          // Model requests are externally visible, billable side effects just
          // like tool calls. Managed turns and durable background deliveries
          // must revalidate their current authority immediately before every
          // primary, retry, or fallback request.
          await toolUseContext.revalidateSideEffectAuthority?.({
            phase: 'preModelCall',
          })
          for await (const streamedMessage of deps.callModel({
            messages: prependUserContext(messagesForQuery, userContext),
            systemPrompt: fullSystemPrompt,
            thinkingConfig: toolUseContext.options.thinkingConfig,
            tools: toolUseContext.options.tools,
            signal: toolUseContext.abortController.signal,
            options: {
              async getToolPermissionContext() {
                const appState = toolUseContext.getAppState()
                return appState.toolPermissionContext
              },
              model: currentModel,
              ...(config.gates.fastModeEnabled && {
                fastMode: appState.fastMode,
              }),
              toolChoice: undefined,
              isNonInteractiveSession:
                toolUseContext.options.isNonInteractiveSession,
              fallbackModel,
              onStreamingFallback: () => {
                streamingFallbackOccured = true
              },
              querySource,
              agents: toolUseContext.options.agentDefinitions.activeAgents,
              allowedAgentTypes:
                toolUseContext.options.agentDefinitions.allowedAgentTypes,
              hasAppendSystemPrompt:
                !!toolUseContext.options.appendSystemPrompt,
              maxOutputTokensOverride,
              fetchOverride: dumpPromptsFetch,
              mcpTools: appState.mcp.tools,
              hasPendingMcpServers: appState.mcp.clients.some(
                c => c.type === 'pending',
              ),
              queryTracking,
              effortValue: appState.effortValue,
              crabcodeThinkingMode:
                toolUseContext.options.crabcodeThinkingMode,
              accountBridgeRuntimeAccess:
                toolUseContext.options.accountBridgeRuntimeAccess,
              advisorModel: appState.advisorModel,
              skipCacheWrite,
              agentId: toolUseContext.agentId,
              addNotification: toolUseContext.addNotification,
              ...(params.taskBudget && {
                taskBudget: {
                  total: params.taskBudget.total,
                  ...(taskBudgetRemaining !== undefined && {
                    remaining: taskBudgetRemaining,
                  }),
                },
              }),
            },
          })) {
            let message = streamedMessage
            if (message.type === 'assistant' && pendingFallbackMediaUsage) {
              message = {
                ...message,
                mediaSidecarUsed: mergeMediaSidecarUsage(
                  pendingFallbackMediaUsage,
                  message.mediaSidecarUsed,
                ),
              }
              pendingFallbackMediaUsage = undefined
            }
            // We won't use the tool_calls from the first attempt
            // We could.. but then we'd have to merge assistant messages
            // with different ids and double up on full the tool_results
            if (streamingFallbackOccured) {
              // Yield tombstones for orphaned messages so they're removed from UI and transcript.
              // These partial messages (especially thinking blocks) have invalid signatures
              // that would cause "thinking blocks cannot be modified" API errors.
              for (const msg of assistantMessages) {
                yield { type: 'tombstone' as const, message: msg }
              }
              logEvent('tengu_orphaned_messages_tombstoned', {
                orphanedMessageCount: assistantMessages.length,
                queryChainId: queryChainIdForAnalytics,
                queryDepth: queryTracking.depth,
              })

              assistantMessages.length = 0
              toolResults.length = 0
              toolUseBlocks.length = 0
              quarantinedToolUses.length = 0
              needsFollowUp = false

              // Discard pending results from the failed streaming attempt and create
              // a fresh executor. This prevents orphan tool_results (with old tool_use_ids)
              // from being yielded after the fallback response arrives.
              if (streamingToolExecutor) {
                streamingToolExecutor.discard()
                streamingToolExecutor = new StreamingToolExecutor(
                  toolUseContext.options.tools,
                  canUseTool,
                  toolUseContext,
                )
              }
            }
            // Backfill tool_use inputs on a cloned message before yield so
            // SDK stream output and transcript serialization see legacy/derived
            // fields. The original `message` is left untouched for
            // assistantMessages.push below — it flows back to the API and
            // mutating it would break prompt caching (byte mismatch).
            let yieldMessage: typeof message = message
            if (message.type === 'assistant') {
              let clonedContent: typeof message.message.content | undefined
              for (let i = 0; i < message.message.content.length; i++) {
                const block = message.message.content[i]!
                if (
                  block.type === 'tool_use' &&
                  typeof block.input === 'object' &&
                  block.input !== null
                ) {
                  const tool = findToolByName(
                    toolUseContext.options.tools,
                    block.name,
                  )
                  if (tool?.backfillObservableInput) {
                    const originalInput = block.input as Record<string, unknown>
                    const inputCopy = { ...originalInput }
                    tool.backfillObservableInput(inputCopy)
                    // Only yield a clone when backfill ADDED fields; skip if
                    // it only OVERWROTE existing ones (e.g. file tools
                    // expanding file_path). Overwrites change the serialized
                    // transcript and break VCR fixture hashes on resume,
                    // while adding nothing the SDK stream needs — hooks get
                    // the expanded path via toolExecution.ts separately.
                    const addedFields = Object.keys(inputCopy).some(
                      k => !(k in originalInput),
                    )
                    if (addedFields) {
                      clonedContent ??= [...message.message.content]
                      clonedContent[i] = { ...block, input: inputCopy }
                    }
                  }
                }
              }
              if (clonedContent) {
                yieldMessage = {
                  ...message,
                  message: { ...message.message, content: clonedContent },
                }
              }
            }
            // Withhold recoverable errors (prompt-too-long, max-output-tokens)
            // until we know whether recovery (collapse drain / reactive
            // compact / truncation retry) can succeed. Still pushed to
            // assistantMessages so the recovery checks below find them.
            // Either subsystem's withhold is sufficient — they're
            // independent so turning one off doesn't break the other's
            // recovery path.
            //
            // feature() only works in if/ternary conditions (bun:bundle
            // tree-shaking constraint), so the collapse check is nested
            // rather than composed.
            let withheld = false
            if (feature('CONTEXT_COLLAPSE')) {
              if (
                contextCollapse?.isWithheldPromptTooLong(
                  message,
                  isPromptTooLongMessage,
                  querySource,
                )
              ) {
                withheld = true
              }
            }
            if (reactiveCompact?.isWithheldPromptTooLong(message)) {
              withheld = true
            }
            if (
              mediaRecoveryEnabled &&
              reactiveCompact?.isWithheldMediaSizeError(message)
            ) {
              withheld = true
            }
            if (isWithheldMaxOutputTokens(message)) {
              withheld = true
            }
            if (!withheld) {
              yield yieldMessage
            }
            if (message.type === 'assistant') {
              assistantMessages.push(message)

              const msgToolUseBlocks = message.message.content.filter(
                content => content.type === 'tool_use',
              ) as ToolUseBlock[]
              if (msgToolUseBlocks.length > 0) {
                toolUseBlocks.push(...msgToolUseBlocks)
                needsFollowUp = true
              }

              for (const toolBlock of msgToolUseBlocks) {
                const quarantineReason =
                  getToolInputQuarantineReason(toolBlock)
                if (quarantineReason) {
                  quarantinedToolUses.push({
                    block: toolBlock,
                    assistantMessage: message,
                    reason: quarantineReason,
                  })
                } else if (
                  streamingToolExecutor &&
                  !toolUseContext.abortController.signal.aborted
                ) {
                  streamingToolExecutor.addTool(toolBlock, message)
                }
              }
            }

            if (
              streamingToolExecutor &&
              !toolUseContext.abortController.signal.aborted
            ) {
              for (const result of streamingToolExecutor.getCompletedResults()) {
                if (result.message) {
                  yield result.message
                  toolResults.push(
                    ...normalizeMessagesForAPI(
                      [result.message],
                      toolUseContext.options.tools,
                    ).filter(_ => _.type === 'user'),
                  )
                }
              }
            }
          }
          queryCheckpoint('query_api_streaming_end')

          // Yield deferred microcompact boundary message using actual API-reported
          // token deletion count instead of client-side estimates.
          // Entire block gated behind feature() so the excluded string
          // is eliminated from external builds.
          if (feature('CACHED_MICROCOMPACT') && pendingCacheEdits) {
            const lastAssistant = assistantMessages.at(-1)
            // The API field is cumulative/sticky across requests, so we
            // subtract the baseline captured before this request to get the delta.
            const usage = lastAssistant?.message.usage
            const cumulativeDeleted = usage
              ? ((usage as unknown as Record<string, number>)
                  .cache_deleted_input_tokens ?? 0)
              : 0
            const deletedTokens = Math.max(
              0,
              cumulativeDeleted - pendingCacheEdits.baselineCacheDeletedTokens,
            )
            if (deletedTokens > 0) {
              yield createMicrocompactBoundaryMessage(
                pendingCacheEdits.trigger,
                0,
                deletedTokens,
                pendingCacheEdits.deletedToolIds,
                [],
              )
            }
          }
        } catch (innerError) {
          if (innerError instanceof FallbackTriggeredError && fallbackModel) {
            fallbackBudgetError = params.modelBudgetGuard?.()
            if (fallbackBudgetError) {
              streamingToolExecutor?.discard()
              throw fallbackBudgetError
            }
            pendingFallbackMediaUsage = mergeMediaSidecarUsage(
              pendingFallbackMediaUsage,
              innerError.mediaSidecarUsed,
            )
            // Fallback was triggered - switch model and retry
            currentModel = fallbackModel
            attemptWithFallback = true

            // Clear assistant messages since we'll retry the entire request
            yield* yieldMissingToolResultBlocks(
              assistantMessages,
              'Model fallback triggered',
            )
            assistantMessages.length = 0
            toolResults.length = 0
            toolUseBlocks.length = 0
            quarantinedToolUses.length = 0
            needsFollowUp = false

            // Discard pending results from the failed attempt and create a
            // fresh executor. This prevents orphan tool_results (with old
            // tool_use_ids) from leaking into the retry.
            if (streamingToolExecutor) {
              streamingToolExecutor.discard()
              streamingToolExecutor = new StreamingToolExecutor(
                toolUseContext.options.tools,
                canUseTool,
                toolUseContext,
              )
            }

            // Update tool use context with new model
            toolUseContext.options.mainLoopModel = fallbackModel

            // Thinking signatures are model-bound: replaying a protected-thinking
            // block from a gated SKU on an unprotected fallback model 400s.
            // Strip before retry so the fallback model gets clean history.
            if (process.env.USER_TYPE === 'ant') {
              messagesForQuery = stripSignatureBlocks(messagesForQuery)
            }

            // Log the fallback event
            logEvent('tengu_model_fallback_triggered', {
              original_model:
                innerError.originalModel as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              fallback_model:
                fallbackModel as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              entrypoint:
                'cli' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
              queryChainId: queryChainIdForAnalytics,
              queryDepth: queryTracking.depth,
            })

            // Yield system message about fallback — use 'warning' level so
            // users see the notification without needing verbose mode.
            // CrabCode: 中文化 + 同时覆盖 model_not_found / 持续 529 触发。
            yield createSystemMessage(
              `[系统] 当前模型不可用，已切换到 ${renderModelName(innerError.fallbackModel)}（原模型 ${renderModelName(innerError.originalModel)}）`,
              'warning',
            )

            continue
          }
          throw innerError
        }
      }
    } catch (error) {
      if (error === fallbackBudgetError) throw error
      logError(error)
      const errorMessage =
        error instanceof Error ? error.message : String(error)
      logEvent('tengu_query_error', {
        assistantMessages: assistantMessages.length,
        toolUses: assistantMessages.flatMap(_ =>
          _.message.content.filter(content => content.type === 'tool_use'),
        ).length,

        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })

      // Handle image size/resize errors with user-friendly messages
      if (
        error instanceof ImageSizeError ||
        error instanceof ImageResizeError
      ) {
        const terminal = createAssistantAPIErrorMessage({
          content: error.message,
        })
        yield pendingFallbackMediaUsage
          ? { ...terminal, mediaSidecarUsed: pendingFallbackMediaUsage }
          : terminal
        pendingFallbackMediaUsage = undefined
        return { reason: 'image_error' }
      }

      // Generally queryModelWithStreaming should not throw errors but instead
      // yield them as synthetic assistant messages. However if it does throw
      // due to a bug, we may end up in a state where we have already emitted
      // a tool_use block but will stop before emitting the tool_result.
      yield* yieldMissingToolResultBlocks(assistantMessages, errorMessage)

      // Surface the real error instead of a misleading "[Request interrupted
      // by user]" — this path is a model/runtime failure, not a user action.
      // SDK consumers were seeing phantom interrupts on e.g. Node 18's missing
      // Array.prototype.with(), masking the actual cause.
      const terminal = createAssistantAPIErrorMessage({
        content: errorMessage,
      })
      yield pendingFallbackMediaUsage
        ? { ...terminal, mediaSidecarUsed: pendingFallbackMediaUsage }
        : terminal
      pendingFallbackMediaUsage = undefined

      // To help track down bugs, log loudly for ants
      logAntError('Query error', error)
      return { reason: 'model_error', error }
    }

    const postResponseBudgetError = params.modelBudgetGuard?.()
    if (postResponseBudgetError) {
      // Streaming tool execution may have begun before terminal usage arrived.
      // Discard aborts those children without reclassifying the budget failure
      // as a user cancellation, then the hard error propagates to the workflow.
      streamingToolExecutor?.discard()
      throw postResponseBudgetError
    }

    // R3 side-effect gate. queryModel yields tool_use at content_block_stop,
    // then mutates the final assistant message with usage/stop_reason when the
    // later message_delta arrives. File mutations (and every malformed JSON
    // input) were withheld above, so this is the first point where they may be
    // released, retried, or failed with an explicit diagnostic.
    if (
      quarantinedToolUses.length > 0 &&
      toolUseContext.abortController.signal.aborted
    ) {
      // Pairing-only path: the executor observes the already-aborted parent
      // and emits synthetic tool_results without calling any tool. Without
      // registering these deferred blocks, the generic abort drain below
      // would not know they exist.
      if (streamingToolExecutor) {
        for (const quarantined of quarantinedToolUses) {
          streamingToolExecutor.addTool(
            quarantined.block,
            quarantined.assistantMessage,
          )
        }
      }
    } else if (quarantinedToolUses.length > 0) {
      let terminalMessage: AssistantMessage | undefined
      for (let i = assistantMessages.length - 1; i >= 0; i--) {
        const candidate = assistantMessages[i]
        if (
          candidate &&
          typeof candidate.message.stop_reason === 'string' &&
          candidate.message.stop_reason.length > 0
        ) {
          terminalMessage = candidate
          break
        }
      }

      const maxOutputTokens =
        maxOutputTokensOverride ?? getMaxOutputTokensForModel(currentModel)
      const onlyQuarantinedTools =
        toolUseBlocks.length === quarantinedToolUses.length
      const capEnabled = getFeatureValue_CACHED_MAY_BE_STALE(
        'tengu_otk_slot_v1',
        false,
      )
      const mayRetryWithLargerOutput =
        onlyQuarantinedTools &&
        toolResults.length === 0 &&
        capEnabled &&
        maxOutputTokensOverride === undefined &&
        !process.env.CRABCODE_MAX_OUTPUT_TOKENS
      const terminal = {
        outputTokens: terminalMessage?.message.usage?.output_tokens,
        stopReason: terminalMessage?.message.stop_reason,
        maxOutputTokens,
      }
      const decisions = quarantinedToolUses.map(quarantined => ({
        quarantined,
        decision: decideToolInputQuarantine({
          reason: quarantined.reason,
          terminal,
          mayRetryWithLargerOutput,
        }),
      }))

      if (decisions.every(({ decision }) => decision.kind === 'release')) {
        // Low-token, terminally complete file mutations are safe to hand to
        // the ordinary executor now. A real Write {} will fail its schema in
        // the normal way, but it never ran before final usage was known.
        if (streamingToolExecutor) {
          for (const { quarantined } of decisions) {
            streamingToolExecutor.addTool(
              quarantined.block,
              quarantined.assistantMessage,
            )
          }
        }
      } else if (
        decisions.every(
          ({ decision }) => decision.kind === 'retry_with_larger_output',
        )
      ) {
        // Retry is permitted only when every tool in the response was held in
        // quarantine and no result escaped. Therefore discard cannot cancel a
        // previously-started side effect in this branch.
        streamingToolExecutor?.discard()
        const tombstoned = new Set<string>()
        for (const message of assistantMessages) {
          if (message.isApiErrorMessage || tombstoned.has(message.uuid)) continue
          tombstoned.add(message.uuid)
          yield { type: 'tombstone' as const, message }
        }
        logEvent('tengu_tool_input_truncation_recovery', {
          tool_count: quarantinedToolUses.length,
          output_tokens: terminal.outputTokens ?? 0,
          max_tokens: maxOutputTokens,
          queryChainId: queryChainIdForAnalytics,
          queryDepth: queryTracking.depth,
        })
        state = {
          messages: messagesForQuery,
          toolUseContext,
          autoCompactTracking: tracking,
          maxOutputTokensRecoveryCount,
          hasAttemptedReactiveCompact,
          maxOutputTokensOverride: ESCALATED_MAX_TOKENS,
          pendingToolUseSummary: undefined,
          stopHookActive: undefined,
          turnCount,
          transition: { reason: 'tool_input_truncation_recovery' },
        }
        continue
      } else {
        // A mixed response may already have started non-file tools. Drain those
        // results before removing only the quarantined assistant blocks so the
        // transcript never contains an orphan tool_use/tool_result pair.
        if (streamingToolExecutor) {
          for await (const update of streamingToolExecutor.getRemainingResults()) {
            if (!update.message) continue
            yield update.message
            toolResults.push(
              ...normalizeMessagesForAPI(
                [update.message],
                toolUseContext.options.tools,
              ).filter(_ => _.type === 'user'),
            )
          }
        } else {
          const quarantinedIds = new Set(
            quarantinedToolUses.map(({ block }) => block.id),
          )
          const ordinaryBlocks = toolUseBlocks.filter(
            block => !quarantinedIds.has(block.id),
          )
          if (ordinaryBlocks.length > 0) {
            for await (const update of runTools(
              ordinaryBlocks,
              assistantMessages,
              canUseTool,
              toolUseContext,
            )) {
              if (!update.message) continue
              yield update.message
              toolResults.push(
                ...normalizeMessagesForAPI(
                  [update.message],
                  toolUseContext.options.tools,
                ).filter(_ => _.type === 'user'),
              )
            }
          }
        }

        const quarantinedMessageUuids = new Set(
          quarantinedToolUses.map(({ assistantMessage }) =>
            assistantMessage.uuid,
          ),
        )
        for (const message of assistantMessages) {
          if (quarantinedMessageUuids.has(message.uuid)) {
            yield { type: 'tombstone' as const, message }
          }
        }

        const failed = decisions.find(
          ({ decision }) => decision.kind === 'fail',
        )
        const representative = failed?.quarantined ?? quarantinedToolUses[0]!
        const decision =
          failed?.decision.kind === 'fail'
            ? failed.decision
            : {
                kind: 'fail' as const,
                reason: 'likely_truncated_tool_input' as const,
              }
        const malformed = isMalformedToolInput(representative.block.input)
          ? representative.block.input.__crabcodeToolInputError
          : undefined
        const errorMessage = buildToolInputQuarantineError({
          toolName: representative.block.name,
          decision,
          inputLength: malformed?.inputLength,
          terminal,
        })
        logEvent('tengu_tool_input_quarantine_failed', {
          tool_count: quarantinedToolUses.length,
          output_tokens: terminal.outputTokens ?? 0,
          max_tokens: maxOutputTokens,
          queryChainId: queryChainIdForAnalytics,
          queryDepth: queryTracking.depth,
        })
        yield createAssistantAPIErrorMessage({ content: errorMessage })
        return { reason: 'malformed_tool_input', error: new Error(errorMessage) }
      }
    }

    // Execute post-sampling hooks after model response is complete
    if (
      assistantMessages.length > 0 &&
      !toolUseContext.suppressUntrustedHooks
    ) {
      void executePostSamplingHooks(
        [...messagesForQuery, ...assistantMessages],
        systemPrompt,
        userContext,
        systemContext,
        toolUseContext,
        querySource,
      )
    }

    // We need to handle a streaming abort before anything else.
    // When using streamingToolExecutor, we must consume getRemainingResults() so the
    // executor can generate synthetic tool_result blocks for queued/in-progress tools.
    // Without this, tool_use blocks would lack matching tool_result blocks.
    if (toolUseContext.abortController.signal.aborted) {
      if (streamingToolExecutor) {
        // Consume remaining results - executor generates synthetic tool_results for
        // aborted tools since it checks the abort signal in executeTool()
        for await (const update of streamingToolExecutor.getRemainingResults()) {
          if (update.message) {
            yield update.message
          }
        }
      } else {
        yield* yieldMissingToolResultBlocks(
          assistantMessages,
          'Interrupted by user',
        )
      }
      // Skip the interruption message for submit-interrupts — the queued
      // user message that follows provides sufficient context.
      if (toolUseContext.abortController.signal.reason !== 'interrupt') {
        yield createUserInterruptionMessage({
          toolUse: false,
        })
      }
      return { reason: 'aborted_streaming' }
    }

    // Yield tool use summary from previous turn — fast-mode tier (~1s) resolved during main-tier streaming (5-30s)
    if (pendingToolUseSummary) {
      const summary = await pendingToolUseSummary
      if (summary) {
        yield summary
      }
    }

    if (!needsFollowUp) {
      const lastMessage = assistantMessages.at(-1)

      // Prompt-too-long recovery: the streaming loop withheld the error
      // (see withheldByCollapse / withheldByReactive above). Try collapse
      // drain first (cheap, keeps granular context), then reactive compact
      // (full summary). Single-shot on each — if a retry still 413's,
      // the next stage handles it or the error surfaces.
      const isWithheld413 =
        lastMessage?.type === 'assistant' &&
        lastMessage.isApiErrorMessage &&
        isPromptTooLongMessage(lastMessage)
      // Media-size rejections (image/PDF/many-image) are recoverable via
      // reactive compact's strip-retry. Unlike PTL, media errors skip the
      // collapse drain — collapse doesn't strip images. mediaRecoveryEnabled
      // is the hoisted gate from before the stream loop (same value as the
      // withholding check — these two must agree or a withheld message is
      // lost). If the oversized media is in the preserved tail, the
      // post-compact turn will media-error again; hasAttemptedReactiveCompact
      // prevents a spiral and the error surfaces.
      const isWithheldMedia =
        mediaRecoveryEnabled &&
        reactiveCompact?.isWithheldMediaSizeError(lastMessage)
      if (isWithheld413) {
        // First: drain all staged context-collapses. Gated on the PREVIOUS
        // transition not being collapse_drain_retry — if we already drained
        // and the retry still 413'd, fall through to reactive compact.
        if (
          !suppressAmbientContext &&
          feature('CONTEXT_COLLAPSE') &&
          contextCollapse &&
          state.transition?.reason !== 'collapse_drain_retry'
        ) {
          const drained = contextCollapse.recoverFromOverflow(
            messagesForQuery,
            querySource,
          )
          if (drained.committed > 0) {
            const next: State = {
              messages: drained.messages,
              toolUseContext,
              autoCompactTracking: tracking,
              maxOutputTokensRecoveryCount,
              hasAttemptedReactiveCompact,
              maxOutputTokensOverride: undefined,
              pendingToolUseSummary: undefined,
              stopHookActive: undefined,
              turnCount,
              transition: {
                reason: 'collapse_drain_retry',
                committed: drained.committed,
              },
            }
            state = next
            continue
          }
        }
      }
      if (
        !suppressAmbientContext &&
        (isWithheld413 || isWithheldMedia) &&
        reactiveCompact
      ) {
        const compacted = await reactiveCompact.tryReactiveCompact({
          hasAttempted: hasAttemptedReactiveCompact,
          querySource,
          aborted: toolUseContext.abortController.signal.aborted,
          messages: messagesForQuery,
          cacheSafeParams: {
            systemPrompt,
            userContext,
            systemContext,
            toolUseContext,
            forkContextMessages: messagesForQuery,
          },
        })

        if (compacted) {
          // task_budget: same carryover as the proactive path above.
          // messagesForQuery still holds the pre-compact array here (the
          // 413-failed attempt's input).
          if (params.taskBudget) {
            const preCompactContext =
              finalContextTokensFromLastResponse(messagesForQuery)
            taskBudgetRemaining = Math.max(
              0,
              (taskBudgetRemaining ?? params.taskBudget.total) -
                preCompactContext,
            )
          }

          const postCompactMessages = buildPostCompactMessages(compacted)
          for (const msg of postCompactMessages) {
            yield msg
          }
          const next: State = {
            messages: postCompactMessages,
            toolUseContext,
            autoCompactTracking: undefined,
            maxOutputTokensRecoveryCount,
            hasAttemptedReactiveCompact: true,
            maxOutputTokensOverride: undefined,
            pendingToolUseSummary: undefined,
            stopHookActive: undefined,
            turnCount,
            transition: { reason: 'reactive_compact_retry' },
          }
          state = next
          continue
        }

        // No recovery — surface the withheld error and exit. Do NOT fall
        // through to stop hooks: the model never produced a valid response,
        // so hooks have nothing meaningful to evaluate. Running stop hooks
        // on prompt-too-long creates a death spiral: error → hook blocking
        // → retry → error → … (the hook injects more tokens each cycle).
        yield lastMessage!
        if (!toolUseContext.suppressUntrustedHooks) {
          void executeStopFailureHooks(lastMessage!, toolUseContext)
        }
        return { reason: isWithheldMedia ? 'image_error' : 'prompt_too_long' }
      } else if (feature('CONTEXT_COLLAPSE') && isWithheld413) {
        // reactiveCompact compiled out but contextCollapse withheld and
        // couldn't recover (staged queue empty/stale). Surface. Same
        // early-return rationale — don't fall through to stop hooks.
        yield lastMessage!
        if (!toolUseContext.suppressUntrustedHooks) {
          void executeStopFailureHooks(lastMessage!, toolUseContext)
        }
        return { reason: 'prompt_too_long' }
      }

      // Check for max_output_tokens and inject recovery message. The error
      // was withheld from the stream above; only surface it if recovery
      // exhausts.
      if (isWithheldMaxOutputTokens(lastMessage)) {
        // Escalating retry: if we used the capped 8k default and hit the
        // limit, retry the SAME request at 64k — no meta message, no
        // multi-turn dance. This fires once per turn (guarded by the
        // override check), then falls through to multi-turn recovery if
        // 64k also hits the cap.
        // 3P default: false (not validated on Bedrock/Vertex)
        const capEnabled = getFeatureValue_CACHED_MAY_BE_STALE(
          'tengu_otk_slot_v1',
          false,
        )
        if (
          capEnabled &&
          maxOutputTokensOverride === undefined &&
          !process.env.CRABCODE_MAX_OUTPUT_TOKENS
        ) {
          logEvent('tengu_max_tokens_escalate', {
            escalatedTo: ESCALATED_MAX_TOKENS,
          })
          const next: State = {
            messages: messagesForQuery,
            toolUseContext,
            autoCompactTracking: tracking,
            maxOutputTokensRecoveryCount,
            hasAttemptedReactiveCompact,
            maxOutputTokensOverride: ESCALATED_MAX_TOKENS,
            pendingToolUseSummary: undefined,
            stopHookActive: undefined,
            turnCount,
            transition: { reason: 'max_output_tokens_escalate' },
          }
          state = next
          continue
        }

        if (maxOutputTokensRecoveryCount < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT) {
          const recoveryMessage = createUserMessage({
            content:
              `Output token limit hit. Resume directly — no apology, no recap of what you were doing. ` +
              `Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.`,
            isMeta: true,
          })

          const next: State = {
            messages: [
              ...messagesForQuery,
              ...assistantMessages,
              recoveryMessage,
            ],
            toolUseContext,
            autoCompactTracking: tracking,
            maxOutputTokensRecoveryCount: maxOutputTokensRecoveryCount + 1,
            hasAttemptedReactiveCompact,
            maxOutputTokensOverride: undefined,
            pendingToolUseSummary: undefined,
            stopHookActive: undefined,
            turnCount,
            transition: {
              reason: 'max_output_tokens_recovery',
              attempt: maxOutputTokensRecoveryCount + 1,
            },
          }
          state = next
          continue
        }

        // Recovery exhausted — surface the withheld error now.
        yield lastMessage
      }

      // Skip stop hooks when the last message is an API error (rate limit,
      // prompt-too-long, auth failure, etc.). The model never produced a
      // real response — hooks evaluating it create a death spiral:
      // error → hook blocking → retry → error → …
      if (lastMessage?.isApiErrorMessage) {
        if (!toolUseContext.suppressUntrustedHooks) {
          void executeStopFailureHooks(lastMessage, toolUseContext)
        }
        return { reason: 'completed' }
      }

      const stopHookResult = yield* handleStopHooks(
        messagesForQuery,
        assistantMessages,
        systemPrompt,
        userContext,
        systemContext,
        toolUseContext,
        querySource,
        stopHookActive,
      )

      if (stopHookResult.preventContinuation) {
        return { reason: 'stop_hook_prevented' }
      }

      if (stopHookResult.blockingErrors.length > 0) {
        const next: State = {
          messages: [
            ...messagesForQuery,
            ...assistantMessages,
            ...stopHookResult.blockingErrors,
          ],
          toolUseContext,
          autoCompactTracking: tracking,
          maxOutputTokensRecoveryCount: 0,
          // Preserve the reactive compact guard — if compact already ran and
          // couldn't recover from prompt-too-long, retrying after a stop-hook
          // blocking error will produce the same result. Resetting to false
          // here caused an infinite loop: compact → still too long → error →
          // stop hook blocking → compact → … burning thousands of API calls.
          hasAttemptedReactiveCompact,
          maxOutputTokensOverride: undefined,
          pendingToolUseSummary: undefined,
          stopHookActive: true,
          turnCount,
          transition: { reason: 'stop_hook_blocking' },
        }
        state = next
        continue
      }

      if (feature('TOKEN_BUDGET')) {
        const decision = checkTokenBudget(
          budgetTracker!,
          toolUseContext.agentId,
          getCurrentTurnTokenBudget(),
          getTurnOutputTokens(),
        )

        if (decision.action === 'continue') {
          incrementBudgetContinuationCount()
          logForDebugging(
            `Token budget continuation #${decision.continuationCount}: ${decision.pct}% (${decision.turnTokens.toLocaleString()} / ${decision.budget.toLocaleString()})`,
          )
          state = {
            messages: [
              ...messagesForQuery,
              ...assistantMessages,
              createUserMessage({
                content: decision.nudgeMessage,
                isMeta: true,
              }),
            ],
            toolUseContext,
            autoCompactTracking: tracking,
            maxOutputTokensRecoveryCount: 0,
            hasAttemptedReactiveCompact: false,
            maxOutputTokensOverride: undefined,
            pendingToolUseSummary: undefined,
            stopHookActive: undefined,
            turnCount,
            transition: { reason: 'token_budget_continuation' },
          }
          continue
        }

        if (decision.completionEvent) {
          if (decision.completionEvent.diminishingReturns) {
            logForDebugging(
              `Token budget early stop: diminishing returns at ${decision.completionEvent.pct}%`,
            )
          }
          logEvent('tengu_token_budget_completed', {
            ...decision.completionEvent,
            queryChainId: queryChainIdForAnalytics,
            queryDepth: queryTracking.depth,
          })
        }
      }

      return { reason: 'completed' }
    }

    let shouldPreventContinuation = false
    let updatedToolUseContext = toolUseContext

    queryCheckpoint('query_tool_execution_start')

    if (streamingToolExecutor) {
      logEvent('tengu_streaming_tool_execution_used', {
        tool_count: toolUseBlocks.length,
        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })
    } else {
      logEvent('tengu_streaming_tool_execution_not_used', {
        tool_count: toolUseBlocks.length,
        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })
    }

    const toolUpdates = streamingToolExecutor
      ? streamingToolExecutor.getRemainingResults()
      : runTools(toolUseBlocks, assistantMessages, canUseTool, toolUseContext)

    for await (const update of toolUpdates) {
      if (update.message) {
        yield update.message

        if (
          update.message.type === 'attachment' &&
          update.message.attachment.type === 'hook_stopped_continuation'
        ) {
          shouldPreventContinuation = true
        }

        toolResults.push(
          ...normalizeMessagesForAPI(
            [update.message],
            toolUseContext.options.tools,
          ).filter(_ => _.type === 'user'),
        )
      }
      if (update.newContext) {
        updatedToolUseContext = {
          ...update.newContext,
          queryTracking,
        }
      }
    }
    queryCheckpoint('query_tool_execution_end')

    // Generate tool use summary after tool batch completes — passed to next recursive call
    let nextPendingToolUseSummary:
      | Promise<ToolUseSummaryMessage | null>
      | undefined
    if (
      config.gates.emitToolUseSummaries &&
      toolUseBlocks.length > 0 &&
      !toolUseContext.abortController.signal.aborted &&
      !toolUseContext.agentId // subagents don't surface in mobile UI — skip the fast-mode helper call
    ) {
      // Extract the last assistant text block for context
      const lastAssistantMessage = assistantMessages.at(-1)
      let lastAssistantText: string | undefined
      if (lastAssistantMessage) {
        const textBlocks = lastAssistantMessage.message.content.filter(
          block => block.type === 'text',
        )
        if (textBlocks.length > 0) {
          const lastTextBlock = textBlocks.at(-1)
          if (lastTextBlock && 'text' in lastTextBlock) {
            lastAssistantText = lastTextBlock.text
          }
        }
      }

      // Collect tool info for summary generation
      const toolUseIds = toolUseBlocks.map(block => block.id)
      const toolInfoForSummary = toolUseBlocks.map(block => {
        // Find the corresponding tool result
        const toolResult = toolResults.find(
          result =>
            result.type === 'user' &&
            Array.isArray(result.message.content) &&
            result.message.content.some(
              content =>
                content.type === 'tool_result' &&
                content.tool_use_id === block.id,
            ),
        )
        const resultContent =
          toolResult?.type === 'user' &&
          Array.isArray(toolResult.message.content)
            ? toolResult.message.content.find(
                (c): c is ToolResultBlockParam =>
                  c.type === 'tool_result' && c.tool_use_id === block.id,
              )
            : undefined
        return {
          name: block.name,
          input: block.input,
          output:
            resultContent && 'content' in resultContent
              ? resultContent.content
              : null,
        }
      })

      // Fire off summary generation without blocking the next API call
      nextPendingToolUseSummary = generateToolUseSummary({
        tools: toolInfoForSummary,
        signal: toolUseContext.abortController.signal,
        isNonInteractiveSession: toolUseContext.options.isNonInteractiveSession,
        sessionModel: toolUseContext.options.mainLoopModel,
        accountBridgeRuntimeAccess:
          toolUseContext.options.accountBridgeRuntimeAccess,
        lastAssistantText,
      })
        .then(summary => {
          if (summary) {
            return createToolUseSummaryMessage(summary, toolUseIds)
          }
          return null
        })
        .catch(() => null)
    }

    // We were aborted during tool calls
    if (toolUseContext.abortController.signal.aborted) {
      // PR-1 (2026-06-08 audit §8.5): record the abort reason so a future
      // background-agent repro can pin WHICH path directly aborted the agent
      // (StreamingToolExecutor bubble / permission / explicit kill). The fix
      // (forced summary turn + honest finalize) is reason-agnostic; this is
      // observability, not control flow.
      logForDebugging(
        `[query] aborted during tool calls (source=${querySource}); reason=${String(
          toolUseContext.abortController.signal.reason,
        )}`,
      )
      // Skip the interruption message for submit-interrupts — the queued
      // user message that follows provides sufficient context.
      if (toolUseContext.abortController.signal.reason !== 'interrupt') {
        yield createUserInterruptionMessage({
          toolUse: true,
        })
      }
      // Check maxTurns before returning when aborted
      const nextTurnCountOnAbort = turnCount + 1
      if (effectiveMaxTurns && nextTurnCountOnAbort > effectiveMaxTurns) {
        yield createAttachmentMessage({
          type: 'max_turns_reached',
          maxTurns: effectiveMaxTurns,
          turnCount: nextTurnCountOnAbort,
        })
      }
      return { reason: 'aborted_tools' }
    }

    // If a hook indicated to prevent continuation, stop here
    if (shouldPreventContinuation) {
      return { reason: 'hook_stopped' }
    }

    if (tracking?.compacted) {
      tracking.turnCounter++
      logEvent('tengu_post_autocompact_turn', {
        turnId:
          tracking.turnId as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        turnCounter: tracking.turnCounter,

        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })
    }

    // Be careful to do this after tool calls are done, because the API
    // will error if we interleave tool_result messages with regular user messages.

    // Instrumentation: Track message count before attachments
    logEvent('tengu_query_before_attachments', {
      messagesForQueryCount: messagesForQuery.length,
      assistantMessagesCount: assistantMessages.length,
      toolResultsCount: toolResults.length,
      queryChainId: queryChainIdForAnalytics,
      queryDepth: queryTracking.depth,
    })

    // Get queued commands snapshot before processing attachments.
    // These will be sent as attachments so CrabCode can respond to them in the current turn.
    //
    // Drain pending notifications. LocalShellTask completions are 'next'
    // (when MONITOR_TOOL is on) and drain without Sleep. Other task types
    // (agent/workflow/framework) still default to 'later' — the Sleep flush
    // covers those. If all task types move to 'next', this branch could go.
    //
    // Slash commands are excluded from mid-turn drain — they must go through
    // processSlashCommand after the turn ends (via useQueueProcessor), not be
    // sent to the model as text. Bash-mode commands are already excluded by
    // INLINE_NOTIFICATION_MODES in getQueuedCommandAttachments.
    //
    // Agent scoping: the queue is a process-global singleton shared by the
    // coordinator and all in-process subagents. Each loop drains only what's
    // addressed to it — main thread drains agentId===undefined, subagents
    // drain their own agentId. User prompts (mode:'prompt') still go to main
    // only; subagents never see the prompt stream.
    // eslint-disable-next-line custom-rules/require-tool-match-name -- ToolUseBlock.name has no aliases
    const sleepRan = toolUseBlocks.some(b => b.name === SLEEP_TOOL_NAME)
    const isMainThread =
      querySource.startsWith('repl_main_thread') || querySource === 'sdk'
    const currentAgentId = toolUseContext.agentId
    const currentTurnThreadId = getCurrentTurnThreadId()
    const queuedCommandsDrainable = getCommandsByMaxPriority(
      sleepRan ? 'later' : 'next',
    ).filter(cmd => {
      if (isSlashCommand(cmd)) return false
      // W-CRON-SESSION-INJECTION-ISOLATION: a thread-owned command (e.g. a cron
      // fire routed to a specific thread) may only be drained by a turn running
      // under that thread. In the multi-session worker (querySource:'sdk', one
      // process hosting N threads on this module-global queue) this stops
      // thread B's turn from stealing thread A's injected command and answering
      // it under B's transcript. Unowned (undefined) commands drain as before;
      // the TUI installs no override so `currentTurnThreadId` is undefined and
      // every unowned command still passes.
      if (
        cmd.ownerThreadId !== undefined &&
        cmd.ownerThreadId !== currentTurnThreadId
      )
        return false
      if (isMainThread) return cmd.agentId === undefined
      // Subagents only drain task-notifications addressed to them — never
      // user prompts, even if someone stamps an agentId on one.
      return cmd.mode === 'task-notification' && cmd.agentId === currentAgentId
    })
    // W-GOAL-HANG fix (2026-06-11): task-notifications whose task was
    // already explicitly retrieved via TaskOutput (claimed) are redundant —
    // converting them to attachments would inject a user-visible-only-to-
    // the-model message and continue an extra turn. Drop them from BOTH the
    // snapshot (so they aren't injected) and the queue itself (so no later
    // drain resurrects them). Other commands flow through untouched.
    const claimedNotifications = queuedCommandsDrainable.filter(
      cmd =>
        cmd.mode === 'task-notification' &&
        isTaskNotificationClaimed(
          cmd,
          taskId => toolUseContext.getAppState().tasks?.[taskId],
        ),
    )
    if (claimedNotifications.length > 0) {
      logForDebugging(
        `[query] dropping ${claimedNotifications.length} claimed task-notification(s) (already retrieved via TaskOutput)`,
      )
      removeFromQueue(claimedNotifications)
    }
    const queuedCommandsSnapshot = queuedCommandsDrainable.filter(
      cmd => !claimedNotifications.includes(cmd),
    )

    if (!updatedToolUseContext.suppressUntrustedHooks) {
      for await (const attachment of getAttachmentMessages(
        null,
        updatedToolUseContext,
        null,
        queuedCommandsSnapshot,
        [...messagesForQuery, ...assistantMessages, ...toolResults],
        querySource,
      )) {
        yield attachment
        toolResults.push(attachment)
      }
    }

    // W-STEER-MIDTURN (2026-06-25): drain turn-scoped steer at this tool-round
    // boundary (same fine-grained injection point as the process-global queue
    // above, but the SOURCE is a turn-keyed closure — see QueryParams.
    // midTurnSteerProvider docstring; audit P2). The provider drains its own
    // (threadId, turnId) pending-steer queue, emits the user bubble, and returns
    // the projected text(s). Inject each as a human-typed `queued_command`
    // attachment so the model sees the steer in the CURRENT turn's next round —
    // not as a new turn after the whole turn completes. Reached only on the
    // tool-bearing path (the no-tool final round returns at the top before
    // here); leftover (no further tool round) is handled by the worker's outer
    // drain fallback (audit P1).
    // Defensive: a steer-injection failure (e.g. the worker's bubble emit
    // throwing on a closed channel) must NEVER kill the user's in-flight turn.
    // Priority is turn survival over steer delivery — if the provider throws
    // after it has already drained its queue, that one steer may be lost, but a
    // lost steer is far less harmful than a crashed turn (the channel is likely
    // already dead in that case). Degrade to "no mid-turn inject" and continue.
    let midTurnSteerTexts: string[] = []
    try {
      midTurnSteerTexts = params.midTurnSteerProvider?.() ?? []
    } catch (err) {
      logForDebugging(
        `[query] midTurnSteerProvider threw; skipping mid-turn steer injection (turn continues): ${String(err)}`,
      )
      midTurnSteerTexts = []
    }
    for (const steerText of midTurnSteerTexts) {
      const steerMsg = createAttachmentMessage({
        type: 'queued_command',
        prompt: steerText,
        commandMode: 'prompt',
        isMeta: false,
      })
      yield steerMsg
      toolResults.push(steerMsg)
    }

    // Memory prefetch consume: only if settled and not already consumed on
    // an earlier iteration. If not settled yet, skip (zero-wait) and retry
    // next iteration — the prefetch gets as many chances as there are loop
    // iterations before the turn ends. readFileState (cumulative across
    // iterations) filters out memories the model already Read/Wrote/Edited
    // — including in earlier iterations, which the per-iteration
    // toolUseBlocks array would miss.
    if (
      pendingMemoryPrefetch &&
      pendingMemoryPrefetch.settledAt !== null &&
      pendingMemoryPrefetch.consumedOnIteration === -1
    ) {
      const memoryAttachments = filterDuplicateMemoryAttachments(
        await pendingMemoryPrefetch.promise,
        toolUseContext.readFileState,
      )
      for (const memAttachment of memoryAttachments) {
        const msg = createAttachmentMessage(memAttachment)
        yield msg
        toolResults.push(msg)
      }
      pendingMemoryPrefetch.consumedOnIteration = turnCount - 1
    }

    // Inject prefetched skill discovery. collectSkillDiscoveryPrefetch emits
    // hidden_by_main_turn — true when the prefetch resolved before this point
    // (should be >98% at AKI@250ms / fast-mode helper@573ms vs turn durations of 2-30s).
    if (skillPrefetch && pendingSkillPrefetch) {
      const skillAttachments =
        await skillPrefetch.collectSkillDiscoveryPrefetch(pendingSkillPrefetch)
      for (const att of skillAttachments) {
        const msg = createAttachmentMessage(att as import('./utils/attachments-types.js').Attachment)
        yield msg
        toolResults.push(msg)
      }
    }

    // Remove only commands that were actually consumed as attachments.
    // Prompt and task-notification commands are converted to attachments above.
    const consumedCommands = updatedToolUseContext.suppressUntrustedHooks
      ? []
      : queuedCommandsSnapshot.filter(
          (cmd) => cmd.mode === 'prompt' || cmd.mode === 'task-notification',
        )
    if (consumedCommands.length > 0) {
      for (const cmd of consumedCommands) {
        if (cmd.uuid) {
          consumedCommandUuids.push(cmd.uuid)
          notifyCommandLifecycle(cmd.uuid, 'started')
        }
      }
      removeFromQueue(consumedCommands)
    }

    // Instrumentation: Track file change attachments after they're added
    const fileChangeAttachmentCount = count(
      toolResults,
      tr =>
        tr.type === 'attachment' && tr.attachment.type === 'edited_text_file',
    )

    logEvent('tengu_query_after_attachments', {
      totalToolResultsCount: toolResults.length,
      fileChangeAttachmentCount,
      queryChainId: queryChainIdForAnalytics,
      queryDepth: queryTracking.depth,
    })

    // Refresh tools between turns so newly-connected MCP servers become available
    if (updatedToolUseContext.options.refreshTools) {
      const refreshedTools = updatedToolUseContext.options.refreshTools()
      if (refreshedTools !== updatedToolUseContext.options.tools) {
        updatedToolUseContext = {
          ...updatedToolUseContext,
          options: {
            ...updatedToolUseContext.options,
            tools: refreshedTools,
          },
        }
      }
    }

    const toolUseContextWithQueryTracking = {
      ...updatedToolUseContext,
      queryTracking,
    }

    // Each time we have tool results and are about to recurse, that's a turn
    const nextTurnCount = turnCount + 1

    // Periodic task summary for `crabcode ps` — fires mid-turn so a
    // long-running agent still refreshes what it's working on. Gated
    // only on !agentId so every top-level conversation (REPL, SDK, HFI,
    // remote) generates summaries; subagents/forks don't.
    if (feature('BG_SESSIONS')) {
      if (
        !toolUseContext.agentId &&
        taskSummaryModule!.shouldGenerateTaskSummary()
      ) {
        taskSummaryModule!.maybeGenerateTaskSummary({
          systemPrompt,
          userContext,
          systemContext,
          toolUseContext,
          forkContextMessages: [
            ...messagesForQuery,
            ...assistantMessages,
            ...toolResults,
          ],
        })
      }
    }

    // W-AGENT-LOOP-CIRCUIT-BREAKER: serial-depth circuit breakers, evaluated at
    // the turn boundary just before recursing. Both fail-loud via a VISIBLE
    // assistant message before returning — the max_turns_reached attachment is
    // null-rendering (nullRenderingAttachments.ts) so it is silent in the
    // interactive TUI where these breakers matter most.
    const turnToolResults = summarizeTurnToolResults(toolResults)

    // P0-B stage 2: the same session-terminal error signature has now repeated
    // past the N+1 threshold — retrying is provably doomed. Stop and surface.
    if (
      turnToolResults.terminal &&
      turnToolResults.terminal.occurrence >= TERMINAL_CIRCUIT_BREAK_THRESHOLD
    ) {
      const { signature, occurrence } = turnToolResults.terminal
      yield createAssistantAPIErrorMessage({
        content:
          `⛔ 已自动停止：工具连续 ${occurrence} 次遇到本会话内不可恢复的同一错误` +
          `（${signature}）。继续重试只会消耗你的轮次/代币预算而不会成功。请查看上方最后一条` +
          `工具错误里的处置建议（通常是安装缺失组件或重启会话），处理后再继续。`,
      })
      logEvent('tengu_agent_loop_circuit_break', {
        reason:
          'tool_circuit_break' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        signature:
          signature as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })
      return { reason: 'tool_circuit_break', turnCount: nextTurnCount, signature }
    }

    // P1-A: no-progress watchdog. A turn whose tool_results are ALL is_error
    // makes zero forward progress; K consecutive such turns → stop. Any
    // successful tool result resets the counter.
    if (turnToolResults.hadToolResults) {
      consecutiveAllErrorTurns = turnToolResults.allErrored
        ? consecutiveAllErrorTurns + 1
        : 0
    }
    if (consecutiveAllErrorTurns >= NO_PROGRESS_TURN_LIMIT) {
      yield createAssistantAPIErrorMessage({
        content:
          `⛔ 已自动停止：连续 ${consecutiveAllErrorTurns} 轮所有工具调用都失败、零成功产出，` +
          `判定为无进展空转。请查看上方的工具错误，调整方案或告诉我下一步怎么做。`,
      })
      logEvent('tengu_agent_loop_circuit_break', {
        reason:
          'no_progress' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })
      return { reason: 'no_progress', turnCount: nextTurnCount }
    }

    // W-CRON-SESSION-INJECTION-ISOLATION L5: within-loop auto-injection runaway.
    // If this ONE continuous loop keeps draining the SAME auto-injected (isMeta)
    // content turn after turn with no user interaction, a system injection is
    // feeding back on itself — stop fail-loud. Loop-local + reset-on-idle means
    // a legitimately recurring cron (fresh loop per scheduled fire) can never
    // trip this; only a within-turn-chain runaway does.
    const autoInjectKey = autoInjectContentKey(queuedCommandsSnapshot)
    if (autoInjectKey !== null && autoInjectKey === lastAutoInjectContentKey) {
      consecutiveSameAutoInject += 1
    } else {
      consecutiveSameAutoInject = autoInjectKey !== null ? 1 : 0
      lastAutoInjectContentKey = autoInjectKey
    }
    if (consecutiveSameAutoInject >= AUTO_INJECT_REPEAT_LIMIT) {
      yield createAssistantAPIErrorMessage({
        content:
          `⛔ 已自动停止：同一条自动注入的提示在本轮智能体循环内被连续重复注入 ${consecutiveSameAutoInject} 次且无用户交互，` +
          `判定为自动注入回环（如定时任务 / 通道 / 代理反复投递同一内容）。已停止以防空转刷屏。` +
          `请检查触发源（cron 任务 / 通道推送），必要时删除或修正该来源。`,
      })
      logEvent('tengu_agent_loop_circuit_break', {
        reason:
          'auto_inject_repeat' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        queryChainId: queryChainIdForAnalytics,
        queryDepth: queryTracking.depth,
      })
      return { reason: 'auto_inject_repeat', turnCount: nextTurnCount }
    }

    // Check if we've reached the max turns limit
    if (effectiveMaxTurns && nextTurnCount > effectiveMaxTurns) {
      // Fail-loud ONLY for the interactive safety ceiling (maxTurns unset by the
      // caller → injected by W-AGENT-LOOP-CIRCUIT-BREAKER): the max_turns_reached
      // attachment is null-rendering, so without this the interactive ceiling
      // would stop the turn silently. When the caller set --max-turns explicitly
      // (headless --print), the existing error_max_turns result IS the fail-loud
      // signal, so we leave that path byte-for-byte unchanged.
      if (maxTurns === undefined) {
        yield createAssistantAPIErrorMessage({
          content: `⛔ 已自动停止：本轮对话已达到 ${effectiveMaxTurns} 轮上限。这是防跑飞的安全上界；如确需更长的连续自动执行，可设置环境变量 CRABCODE_MAX_TURNS_INTERACTIVE 提高或设为 0 关闭。`,
        })
      }
      yield createAttachmentMessage({
        type: 'max_turns_reached',
        maxTurns: effectiveMaxTurns,
        turnCount: nextTurnCount,
      })
      return { reason: 'max_turns', turnCount: nextTurnCount }
    }

    queryCheckpoint('query_recursive_call')
    const next: State = {
      messages: [...messagesForQuery, ...assistantMessages, ...toolResults],
      toolUseContext: toolUseContextWithQueryTracking,
      autoCompactTracking: tracking,
      turnCount: nextTurnCount,
      maxOutputTokensRecoveryCount: 0,
      hasAttemptedReactiveCompact: false,
      pendingToolUseSummary: nextPendingToolUseSummary,
      maxOutputTokensOverride: undefined,
      stopHookActive,
      transition: { reason: 'next_turn' },
    }
    state = next
  } // while (true)
}
