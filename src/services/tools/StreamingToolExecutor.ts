
import type { ToolUseBlock } from '../../types/api-types.js'
import {
  createUserMessage,
  REJECT_MESSAGE,
  withMemoryCorrectionHint,
} from 'src/utils/messages.js'
import type { CanUseToolFn } from '../../types/canUseTool.js'
import { findToolByName, type Tools, type ToolUseContext } from '../../Tool.js'
import { BASH_TOOL_NAME } from '../../tools/BashTool/toolName.js'
import {
  AGENT_TOOL_NAME,
  LEGACY_AGENT_TOOL_NAME,
} from '../../tools/AgentTool/constants.js'
import type { AssistantMessage, Message } from '../../types/message.js'
import { createChildAbortController } from '../../utils/abortController.js'
import {
  type AgentSlotTicket,
  getBackgroundAgentScheduler,
} from '../../services/agents/BackgroundAgentScheduler.js'
import { runToolUse } from './toolExecution.js'

// Client-side concurrency budget for AgentTool fan-out.
// Caps in-flight agent dispatches to avoid self-induced 429 cascades from
// model-driven unbounded fan-out. Other tools (Read/Bash/Grep) are NOT
// throttled — semaphore is bound to AgentTool only.
//
// Every AgentTool form — foreground here and
// detached background lifecycles (`run_in_background:true`, agent-config
// background, coordinator/fork/KAIROS/proactive forced-async,
// foreground→background handoff, resume) — shares the same process-level
// budget exposed by BackgroundAgentScheduler. Exceeding the budget queues
// without erroring. Foreground holds slots only during tool.call; background
// holds slots until the model stream reaches a terminal state.
//
// Default capacity (3) and env override (CRABCODE_MAX_CONCURRENT_AGENTS) live
// in BackgroundAgentScheduler.ts to keep this contract centralized.

type MessageUpdate = {
  message?: Message
  newContext?: ToolUseContext
}

type ToolStatus = 'queued' | 'executing' | 'completed' | 'yielded'

type TrackedTool = {
  id: string
  block: ToolUseBlock
  assistantMessage: AssistantMessage
  status: ToolStatus
  isConcurrencySafe: boolean
  promise?: Promise<void>
  results?: Message[]
  // Progress messages are stored separately and yielded immediately
  pendingProgress: Message[]
  contextModifiers?: Array<(context: ToolUseContext) => ToolUseContext>
  /**
   * AgentTool-only: BackgroundAgentScheduler slot held while this foreground
   * AgentTool is executing. Released in executeTool's finally hook. Background
   * detached lifecycles own their own tickets via AgentTool.tsx /
   * resumeAgent.ts; this field stays undefined for non-AgentTool entries and
   * for foreground calls borrowing an ancestor's root lease.
   */
  agentSlotTicket?: AgentSlotTicket
}

/**
 * Executes tools as they stream in with concurrency control.
 * - Concurrent-safe tools can execute in parallel with other concurrent-safe tools
 * - Non-concurrent tools must execute alone (exclusive access)
 * - Results are buffered and emitted in the order tools were received
 */
export class StreamingToolExecutor {
  private tools: TrackedTool[] = []
  private toolUseContext: ToolUseContext
  private hasErrored = false
  private erroredToolDescription = ''
  // Child of toolUseContext.abortController. Fires when a Bash tool errors
  // so sibling subprocesses die immediately instead of running to completion.
  // Aborting this does NOT abort the parent — query.ts won't end the turn.
  private siblingAbortController: AbortController
  private discarded = false
  // Signal to wake up getRemainingResults when progress is available
  private progressAvailableResolve?: () => void
  /**
   * Unsubscribe from BackgroundAgentScheduler slot-freed events. The scheduler
   * shares its budget with detached background lifecycles; when a background
   * agent finishes, foreground AgentTools waiting in our queue must be
   * nudged. Cleared once the parent abort signal fires.
   */
  private unsubscribeFromScheduler: (() => void) | undefined

  constructor(
    private readonly toolDefinitions: Tools,
    private readonly canUseTool: CanUseToolFn,
    toolUseContext: ToolUseContext,
  ) {
    this.toolUseContext = toolUseContext
    this.siblingAbortController = createChildAbortController(
      toolUseContext.abortController,
    )
    // Listen for slot frees from the global scheduler. When a background
    // agent finishes, foreground AgentTools queued here must reattempt.
    this.unsubscribeFromScheduler = getBackgroundAgentScheduler().onSlotFreed(
      () => {
        if (this.discarded) return
        void this.processQueue()
      },
    )
    const cleanup = (): void => {
      this.cleanupSchedulerSubscription()
    }
    if (toolUseContext.abortController.signal.aborted) {
      cleanup()
    } else {
      toolUseContext.abortController.signal.addEventListener('abort', cleanup, {
        once: true,
      })
    }
  }

  /**
   * Discards all pending and in-progress tools. Called when streaming fallback
   * occurs and results from the failed attempt should be abandoned.
   * Queued tools won't start, and in-progress tools will receive synthetic errors.
   *
   * S-2 / inc-4258 (2026-06-09 prelaunch audit): also actively abort in-flight
   * tools via siblingAbortController. Without this, a side-effectful tool
   * (Bash write / git) started by the failed streaming attempt keeps running
   * to completion while the non-streaming fallback replays the same tool_use
   * — double execution. The abort cascades to each per-tool child controller
   * (createChildAbortController propagates the reason), killing running
   * subprocesses; collectResults then surfaces the existing
   * 'streaming_fallback' synthetic error (getAbortReason checks `discarded`
   * first, so the message semantics are unchanged and remain distinct from
   * user interrupt / sibling error).
   *
   * Ordering matters: `discarded` MUST be set before the abort fires, so the
   * per-tool abort listener's `!this.discarded` guard blocks bubble-up to the
   * parent query controller (a discard must not end the turn).
   */
  discard(): void {
    if (this.discarded) return
    this.discarded = true
    this.cleanupSchedulerSubscription()
    if (!this.siblingAbortController.signal.aborted) {
      this.siblingAbortController.abort('streaming_fallback')
    }
  }

  private cleanupSchedulerSubscription(): void {
    if (this.unsubscribeFromScheduler) {
      this.unsubscribeFromScheduler()
      this.unsubscribeFromScheduler = undefined
    }
  }

  /**
   * Add a tool to the execution queue. Will start executing immediately if conditions allow.
   */
  addTool(block: ToolUseBlock, assistantMessage: AssistantMessage): void {
    const toolDefinition = findToolByName(this.toolDefinitions, block.name)
    if (!toolDefinition) {
      this.tools.push({
        id: block.id,
        block,
        assistantMessage,
        status: 'completed',
        isConcurrencySafe: true,
        pendingProgress: [],
        results: [
          createUserMessage({
            content: [
              {
                type: 'tool_result',
                content: `<tool_use_error>Error: No such tool available: ${block.name}</tool_use_error>`,
                is_error: true,
                tool_use_id: block.id,
              },
            ],
            toolUseResult: `Error: No such tool available: ${block.name}`,
            sourceToolAssistantUUID: assistantMessage.uuid,
          }),
        ],
      })
      return
    }

    const parsedInput = toolDefinition.inputSchema.safeParse(block.input)
    const isConcurrencySafe = parsedInput?.success
      ? (() => {
          try {
            return Boolean(toolDefinition.isConcurrencySafe(parsedInput.data))
          } catch {
            return false
          }
        })()
      : false
    this.tools.push({
      id: block.id,
      block,
      assistantMessage,
      status: 'queued',
      isConcurrencySafe,
      pendingProgress: [],
    })

    void this.processQueue()
  }

  /**
   * Check if a tool can execute based on current concurrency state.
   *
   * For AgentTool entries, capacity comes from BackgroundAgentScheduler — a
   * process-level singleton shared with detached background lifecycles
   * Over-budget AgentTools stay queued without erroring;
   * release happens via executeTool's finally hook OR via the global
   * scheduler's slot-freed listener (registered in the constructor).
   */
  private canExecuteTool(tool: TrackedTool): boolean {
    const executingTools = this.tools.filter(t => t.status === 'executing')
    const passesConcurrency =
      executingTools.length === 0 ||
      (tool.isConcurrencySafe &&
        executingTools.every(t => t.isConcurrencySafe))
    if (!passesConcurrency) return false

    // A tool destined for a synthetic error (abort / discard / sibling error)
    // can always proceed: collectResults emits the synthetic result without
    // running the agent or holding a scheduler slot. Gating it on capacity here
    // would strand capacity-blocked queued AgentTools on abort (busy-spin / hang).
    if (this.getAbortReason(tool)) return true

    // A root lease may be borrowed by nested foreground AgentTools, but it
    // still represents exactly one unit of process-global agent capacity.
    // Serialize AgentTools within this executor so a single lineage cannot
    // fan out multiple model streams behind that one lease. Other
    // concurrency-safe tools may continue alongside the borrowed AgentTool.
    if (
      this.isBorrowingRootAgentLease(tool) &&
      executingTools.some(isAgentTool)
    ) {
      return false
    }

    if (isAgentTool(tool) && !this.isBorrowingRootAgentLease(tool)) {
      const scheduler = getBackgroundAgentScheduler()
      if (scheduler.runningCount() >= scheduler.capacity()) {
        return false
      }
    }
    return true
  }

  /**
   * Process the queue, starting tools when concurrency conditions allow
   */
  private async processQueue(): Promise<void> {
    // S-2 / inc-4258: once discarded, never start (or synthetically drain)
    // another tool. The executor is abandoned — getCompletedResults yields
    // nothing when discarded, so draining queued tools to synthetic errors
    // here would be pure busywork, and starting a real tool would be the
    // double-execution bug this guard exists to prevent. processQueue is the
    // single funnel for tool starts (addTool / promise.finally / slot-freed
    // listener / getRemainingResults all route through it), so this check
    // covers every entry point.
    if (this.discarded) return
    for (const tool of this.tools) {
      if (tool.status !== 'queued') continue

      if (this.canExecuteTool(tool)) {
        await this.executeTool(tool)
      } else {
        // Non-concurrent tool blocked → preserve order, stop scanning.
        // Concurrent tool blocked by AgentTool semaphore → keep scanning so
        // other non-Agent concurrent tools (Read/Grep/etc) still run.
        if (!tool.isConcurrencySafe) break
      }
    }
  }

  private createSyntheticErrorMessage(
    toolUseId: string,
    reason: 'sibling_error' | 'user_interrupted' | 'streaming_fallback',
    assistantMessage: AssistantMessage,
  ): Message {
    // For user interruptions (ESC to reject), use REJECT_MESSAGE so the UI shows
    // "User rejected edit" instead of "Error editing file"
    if (reason === 'user_interrupted') {
      return createUserMessage({
        content: [
          {
            type: 'tool_result',
            content: withMemoryCorrectionHint(REJECT_MESSAGE),
            is_error: true,
            tool_use_id: toolUseId,
          },
        ],
        toolUseResult: 'User rejected tool use',
        sourceToolAssistantUUID: assistantMessage.uuid,
      })
    }
    if (reason === 'streaming_fallback') {
      return createUserMessage({
        content: [
          {
            type: 'tool_result',
            content:
              '<tool_use_error>Error: Streaming fallback - tool execution discarded</tool_use_error>',
            is_error: true,
            tool_use_id: toolUseId,
          },
        ],
        toolUseResult: 'Streaming fallback - tool execution discarded',
        sourceToolAssistantUUID: assistantMessage.uuid,
      })
    }
    const desc = this.erroredToolDescription
    const msg = desc
      ? `Cancelled: parallel tool call ${desc} errored`
      : 'Cancelled: parallel tool call errored'
    return createUserMessage({
      content: [
        {
          type: 'tool_result',
          content: `<tool_use_error>${msg}</tool_use_error>`,
          is_error: true,
          tool_use_id: toolUseId,
        },
      ],
      toolUseResult: msg,
      sourceToolAssistantUUID: assistantMessage.uuid,
    })
  }

  /**
   * Determine why a tool should be cancelled.
   */
  private getAbortReason(
    tool: TrackedTool,
  ): 'sibling_error' | 'user_interrupted' | 'streaming_fallback' | null {
    if (this.discarded) {
      return 'streaming_fallback'
    }
    if (this.hasErrored) {
      return 'sibling_error'
    }
    if (this.toolUseContext.abortController.signal.aborted) {
      // 'interrupt' means the user typed a new message while tools were
      // running. Only cancel tools whose interruptBehavior is 'cancel';
      // 'block' tools shouldn't reach here (abort isn't fired).
      if (this.toolUseContext.abortController.signal.reason === 'interrupt') {
        return this.getToolInterruptBehavior(tool) === 'cancel'
          ? 'user_interrupted'
          : null
      }
      return 'user_interrupted'
    }
    return null
  }

  private getToolInterruptBehavior(tool: TrackedTool): 'cancel' | 'block' {
    const definition = findToolByName(this.toolDefinitions, tool.block.name)
    if (!definition?.interruptBehavior) return 'block'
    try {
      return definition.interruptBehavior()
    } catch {
      return 'block'
    }
  }

  private getToolDescription(tool: TrackedTool): string {
    const input = tool.block.input as Record<string, unknown> | undefined
    const summary = input?.command ?? input?.file_path ?? input?.pattern ?? ''
    if (typeof summary === 'string' && summary.length > 0) {
      const truncated =
        summary.length > 40 ? summary.slice(0, 40) + '\u2026' : summary
      return `${tool.block.name}(${truncated})`
    }
    return tool.block.name
  }

  private updateInterruptibleState(): void {
    const executing = this.tools.filter(t => t.status === 'executing')
    this.toolUseContext.setHasInterruptibleToolInProgress?.(
      executing.length > 0 &&
        executing.every(t => this.getToolInterruptBehavior(t) === 'cancel'),
    )
  }

  /**
   * Execute a tool and collect its results
   */
  private async executeTool(tool: TrackedTool): Promise<void> {
    // For AgentTool entries, atomically acquire a slot from the shared
    // BackgroundAgentScheduler BEFORE marking the tool executing, unless this
    // context is borrowing an ancestor's root lease. canExecuteTool gated on
    // capacity, but that read and this acquire are NOT atomic — a detached
    // background lifecycle can take the last slot in between (TOCTOU).
    // If the atomic tryAcquireSync fails we must NOT execute: running anyway
    // would exceed the §8 concurrency budget and self-inflict a 429 storm. Leave
    // the tool queued; the scheduler's slot-freed listener (registered in the
    // constructor) re-pumps processQueue when a slot frees. The §8 contract is
    // "queues, never fails" — queue, do not bypass.
    // When the tool is already destined for a synthetic error (sibling error /
    // user interrupt / streaming-fallback discard), collectResults short-circuits
    // without running the agent — so it does NOT need a scheduler slot. Skipping
    // the acquire here lets capacity-blocked queued AgentTools drain to their
    // synthetic error on abort instead of staying queued forever (the wait loop
    // would otherwise never settle them). The §8 budget is only meaningful for
    // tools that actually start a model stream.
    if (
      isAgentTool(tool) &&
      !this.isBorrowingRootAgentLease(tool) &&
      !this.getAbortReason(tool)
    ) {
      const ticket = getBackgroundAgentScheduler().tryAcquireSync(tool.id)
      if (!ticket) {
        tool.status = 'queued'
        return
      }
      tool.agentSlotTicket = ticket
    }
    tool.status = 'executing'
    this.toolUseContext.setInProgressToolUseIDs(prev =>
      new Set(prev).add(tool.id),
    )
    this.updateInterruptibleState()

    const messages: Message[] = []
    const contextModifiers: Array<(context: ToolUseContext) => ToolUseContext> =
      []

    const collectResults = async () => {
      // If already aborted (by error or user), generate synthetic error block instead of running the tool
      const initialAbortReason = this.getAbortReason(tool)
      if (initialAbortReason) {
        messages.push(
          this.createSyntheticErrorMessage(
            tool.id,
            initialAbortReason,
            tool.assistantMessage,
          ),
        )
        tool.results = messages
        tool.contextModifiers = contextModifiers
        tool.status = 'completed'
        this.updateInterruptibleState()
        return
      }

      // Per-tool child controller. Lets siblingAbortController kill running
      // subprocesses (Bash spawns listen to this signal) when a Bash error
      // cascades. Permission-dialog rejection also aborts this controller
      // (PermissionContext.ts cancelAndAbort) — that abort must bubble up to
      // the query controller so the query loop's post-tool abort check ends
      // the turn. Without bubble-up, ExitPlanMode "clear context + auto"
      // sends REJECT_MESSAGE to the model instead of aborting (#21056 regression).
      const toolAbortController = createChildAbortController(
        this.siblingAbortController,
      )
      toolAbortController.signal.addEventListener(
        'abort',
        () => {
          if (
            toolAbortController.signal.reason !== 'sibling_error' &&
            !this.toolUseContext.abortController.signal.aborted &&
            !this.discarded
          ) {
            this.toolUseContext.abortController.abort(
              toolAbortController.signal.reason,
            )
          }
        },
        { once: true },
      )

      const generator = runToolUse(
        tool.block,
        tool.assistantMessage,
        this.canUseTool,
        { ...this.toolUseContext, abortController: toolAbortController },
      )

      // Track if this specific tool has produced an error result.
      // This prevents the tool from receiving a duplicate "sibling error"
      // message when it is the one that caused the error.
      let thisToolErrored = false

      for await (const update of generator) {
        // Check if we were aborted by a sibling tool error or user interruption.
        // Only add the synthetic error if THIS tool didn't produce the error.
        const abortReason = this.getAbortReason(tool)
        if (abortReason && !thisToolErrored) {
          messages.push(
            this.createSyntheticErrorMessage(
              tool.id,
              abortReason,
              tool.assistantMessage,
            ),
          )
          break
        }

        const isErrorResult =
          update.message.type === 'user' &&
          Array.isArray(update.message.message.content) &&
          update.message.message.content.some(
            _ => _.type === 'tool_result' && _.is_error === true,
          )

        if (isErrorResult) {
          thisToolErrored = true
          // Only Bash errors cancel siblings. Bash commands often have implicit
          // dependency chains (e.g. mkdir fails → subsequent commands pointless).
          // Read/WebFetch/etc are independent — one failure shouldn't nuke the rest.
          if (tool.block.name === BASH_TOOL_NAME) {
            this.hasErrored = true
            this.erroredToolDescription = this.getToolDescription(tool)
            this.siblingAbortController.abort('sibling_error')
          }
        }

        if (update.message) {
          // Progress messages go to pendingProgress for immediate yielding
          if (update.message.type === 'progress') {
            tool.pendingProgress.push(update.message)
            // Signal that progress is available
            if (this.progressAvailableResolve) {
              this.progressAvailableResolve()
              this.progressAvailableResolve = undefined
            }
          } else {
            messages.push(update.message)
          }
        }
        if (update.contextModifier) {
          contextModifiers.push(update.contextModifier.modifyContext)
        }
      }
      tool.results = messages
      tool.contextModifiers = contextModifiers
      tool.status = 'completed'
      this.updateInterruptibleState()

      // NOTE: we currently don't support context modifiers for concurrent
      //       tools. None are actively being used, but if we want to use
      //       them in concurrent tools, we need to support that here.
      if (!tool.isConcurrencySafe && contextModifiers.length > 0) {
        for (const modifier of contextModifiers) {
          this.toolUseContext = modifier(this.toolUseContext)
        }
      }
    }

    const promise = collectResults()
    tool.promise = promise

    // Process more queue when done; release any AgentTool slot before the
    // next processQueue() so other foreground / background AgentTools can
    // pick it up.
    void promise.finally(() => {
      if (tool.agentSlotTicket) {
        tool.agentSlotTicket.release()
        tool.agentSlotTicket = undefined
      }
      void this.processQueue()
    })
  }

  /**
   * True only for foreground AgentTool calls in a context whose ancestor
   * already owns the scheduler slot for the full lineage. Background AgentTool
   * lifecycles keep their existing independently-owned ticket contract.
   */
  private isBorrowingRootAgentLease(tool: TrackedTool): boolean {
    return (
      isAgentTool(tool) &&
      this.toolUseContext.agentSchedulerRootLeaseHeld === true
    )
  }

  /**
   * Get any completed results that haven't been yielded yet (non-blocking)
   * Maintains order where necessary
   * Also yields any pending progress messages immediately
   */
  *getCompletedResults(): Generator<MessageUpdate, void> {
    if (this.discarded) {
      return
    }

    for (const tool of this.tools) {
      // Always yield pending progress messages immediately, regardless of tool status
      while (tool.pendingProgress.length > 0) {
        const progressMessage = tool.pendingProgress.shift()!
        yield { message: progressMessage, newContext: this.toolUseContext }
      }

      if (tool.status === 'yielded') {
        continue
      }

      if (tool.status === 'completed' && tool.results) {
        tool.status = 'yielded'

        for (const message of tool.results) {
          yield { message, newContext: this.toolUseContext }
        }

        markToolUseAsComplete(this.toolUseContext, tool.id)
      } else if (tool.status === 'executing' && !tool.isConcurrencySafe) {
        break
      }
    }
  }

  /**
   * Check if any tool has pending progress messages
   */
  private hasPendingProgress(): boolean {
    return this.tools.some(t => t.pendingProgress.length > 0)
  }

  /**
   * Wait for remaining tools and yield their results as they complete
   * Also yields progress messages as they become available
   */
  async *getRemainingResults(): AsyncGenerator<MessageUpdate, void> {
    if (this.discarded) {
      this.cleanupSchedulerSubscription()
      return
    }

    try {
      while (this.hasUnfinishedTools()) {
        // S-2/inc-4258: discard() can fire mid-loop → re-check or busy-spin.
        if (this.discarded) return
        await this.processQueue()

        for (const result of this.getCompletedResults()) {
          yield result
        }

        // If we still have executing tools but nothing completed, wait for any to complete
        // OR for progress to become available
        if (
          this.hasExecutingTools() &&
          !this.hasCompletedResults() &&
          !this.hasPendingProgress()
        ) {
          const executingPromises = this.tools
            .filter(t => t.status === 'executing' && t.promise)
            .map(t => t.promise!)

          // Also wait for progress to become available
          const progressPromise = new Promise<void>(resolve => {
            this.progressAvailableResolve = resolve
          })

          if (executingPromises.length > 0) {
            await Promise.race([...executingPromises, progressPromise])
          }
        } else if (
          // Only scheduler-blocked queued AgentTools remain (zero local
          // executing tools). Spinning here pins a CPU core until a global slot
          // frees (P1-14 / #78). Park on onSlotFreed instead — see
          // waitForSlotOrAbort.
          !this.hasExecutingTools() &&
          !this.hasCompletedResults() &&
          !this.hasPendingProgress() &&
          this.hasSchedulerBlockedQueuedTools()
        ) {
          await this.waitForSlotOrAbort()
        }
      }

      for (const result of this.getCompletedResults()) {
        yield result
      }
    } finally {
      this.cleanupSchedulerSubscription()
    }
  }

  /**
   * Check if there are any completed results ready to yield
   */
  private hasCompletedResults(): boolean {
    return this.tools.some(t => t.status === 'completed')
  }

  /**
   * Check if there are any tools still executing
   */
  private hasExecutingTools(): boolean {
    return this.tools.some(t => t.status === 'executing')
  }

  /**
   * Check if there are any unfinished tools
   */
  private hasUnfinishedTools(): boolean {
    return this.tools.some(t => t.status !== 'yielded')
  }

  /**
   * True when there is at least one queued AgentTool that cannot start solely
   * because the BackgroundAgentScheduler is at capacity (no abort pending, no
   * concurrency conflict from a local non-Agent executing tool). These are the
   * tools that, with zero local executing tools, would cause getRemainingResults
   * to busy-spin until a global slot frees. Used to decide whether to park on
   * the onSlotFreed signal instead of spinning.
   */
  private hasSchedulerBlockedQueuedTools(): boolean {
    return this.tools.some(
      t =>
        t.status === 'queued' &&
        isAgentTool(t) &&
        !this.getAbortReason(t) &&
        this.canExecuteTool(t) === false,
    )
  }

  /**
   * Park until the BackgroundAgentScheduler frees a slot (existing onSlotFreed
   * signal) OR the turn is aborted. One-shot: resolves on the first of these,
   * then the caller's loop re-evaluates and re-pumps the queue. Avoids the tight
   * busy-spin when only scheduler-blocked queued AgentTools remain with zero
   * local executing tools.
   *
   * Reuses the scheduler's onSlotFreed (NO new semaphore, NO lifecycle moved
   * into this executor — §8 v2-wrong-direction guard). The per-wait subscription
   * is unsubscribed before returning; the constructor's long-lived subscription
   * (which re-pumps the queue) is untouched.
   *
   * Progress is intentionally not awaited here: this branch is reached only when
   * there are zero local executing tools (nothing can emit progress). Once a slot
   * frees, the constructor's listener re-pumps the queue and the next loop
   * iteration takes the executing-tools branch, which sets up the progress await.
   */
  private async waitForSlotOrAbort(): Promise<void> {
    // Park on siblingAbortController's signal rather than the parent query
    // signal: it is a child of the parent (parent aborts cascade into it), so
    // this is a strict superset — and it additionally wakes on discard(),
    // which (S-2 / inc-4258) aborts siblingAbortController without aborting
    // the parent. Without this, a discard while parked here would leave the
    // generator suspended until GC.
    const abortSignal = this.siblingAbortController.signal
    if (abortSignal.aborted || this.discarded) return

    let unsubscribeSlot = (): void => {}
    let removeAbort = (): void => {}
    await new Promise<void>(resolve => {
      let settled = false
      const settle = (): void => {
        if (settled) return
        settled = true
        unsubscribeSlot()
        removeAbort()
        resolve()
      }

      // (a) A scheduler slot freed (e.g. a background agent finished).
      unsubscribeSlot = getBackgroundAgentScheduler().onSlotFreed(settle)
      // (b) The turn was aborted (interrupt / sibling error / discard). Wake so
      //     the loop can drain queued tools to synthetic errors instead of hanging.
      const onAbort = (): void => settle()
      abortSignal.addEventListener('abort', onAbort, { once: true })
      removeAbort = () => abortSignal.removeEventListener('abort', onAbort)
    })
  }

  /**
   * Get the current tool use context (may have been modified by context modifiers)
   */
  getUpdatedContext(): ToolUseContext {
    return this.toolUseContext
  }
}

function markToolUseAsComplete(
  toolUseContext: ToolUseContext,
  toolUseID: string,
) {
  toolUseContext.setInProgressToolUseIDs(prev => {
    const next = new Set(prev)
    next.delete(toolUseID)
    return next
  })
}

function isAgentTool(tool: TrackedTool): boolean {
  return (
    tool.block.name === AGENT_TOOL_NAME ||
    tool.block.name === LEGACY_AGENT_TOOL_NAME
  )
}
