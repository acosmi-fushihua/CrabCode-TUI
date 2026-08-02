/**
 * Process-level scheduler for AgentTool concurrency budget.
 *
 * Every AgentTool form — foreground, async-from-start
 * (`run_in_background:true`), agent-config background, coordinator/fork/
 * KAIROS/proactive forced-async, foreground→background handoff, resume — must
 * occupy one slot of the same agent-worker process-level budget. Slot is
 * released once the model stream reaches a terminal state (success / abort /
 * error). Classifier / worktree cleanup must NOT block release.
 *
 * Capacity: `CRABCODE_MAX_CONCURRENT_AGENTS` env (default 3). When all slots
 * are taken, callers acquire a queued ticket whose `whenAvailable` promise
 * resolves once a slot frees. Queued tasks never start their model stream.
 */
import { randomBytes } from 'crypto'

export type AgentSlotTicket = {
  readonly ticketId: string
  readonly taskId: string
  /** Resolves once this ticket holds an in-flight slot. */
  readonly whenAvailable: Promise<void>
  /**
   * True iff the ticket was created in the queued state (i.e. the slot was
   * not available synchronously). Snapshotted at acquire time; does not
   * change as the queue drains. Callers use this to set task state to
   * "awaiting slot" without racing the queue pump.
   */
  readonly queuedAtCreation: boolean
  /** Cancel while still queued; no-op once running. Returns true if it removed a queued entry. */
  cancelIfQueued(): boolean
  /** Release the slot. Idempotent; safe after cancel. */
  release(): void
  /** True until released. */
  isActive(): boolean
}

type QueueEntry = {
  ticketId: string
  taskId: string
  resolve: () => void
  reject: (err: Error) => void
  cancelled: boolean
}

type SlotFreedListener = () => void

/**
 * R3 observability (2026-06-18): per-running-slot bookkeeping. `acquiredAt` lets
 * us measure how long a slot has been held without changing any release timing.
 */
type RunningSlot = {
  taskId: string
  /** `Date.now()` at the moment the slot started running (allocate / promote). */
  acquiredAt: number
}

/** A running slot whose hold duration crossed the diagnostic threshold. */
export type LongHeldSlot = {
  ticketId: string
  taskId: string
  heldMs: number
}

const DEFAULT_MAX_CONCURRENT_AGENTS = 3

// R3 observability (2026-06-18): warn when a running slot has been held longer
// than this. This is NOT a lease — it never force-releases a slot, only logs.
// A slot is supposed to release when its model stream reaches a terminal state
// (onModelStreamTerminal, +retry backoff); a slot held far past any plausible
// turn is the symptom of the latent risk R3 flags (terminal release failing to
// fire). The threshold sits ABOVE the 30-min turn watchdog so a normal long
// turn never trips it; a slot held > 45 min is genuinely anomalous. If this
// never logs in production, the lease (R3 PR-2) is unnecessary.
const DEFAULT_SLOT_HELD_WARN_MS = 45 * 60_000

function readSlotHeldWarnMsFromEnv(): number {
  const raw = process.env.CRABCODE_AGENT_SLOT_WARN_MS
  if (raw) {
    const parsed = parseInt(raw, 10)
    if (!Number.isNaN(parsed) && parsed > 0) {
      return parsed
    }
  }
  return DEFAULT_SLOT_HELD_WARN_MS
}

function readCapacityFromEnv(): number {
  const raw = process.env.CRABCODE_MAX_CONCURRENT_AGENTS
  if (raw) {
    const parsed = parseInt(raw, 10)
    if (!Number.isNaN(parsed) && parsed > 0) {
      return parsed
    }
  }
  return DEFAULT_MAX_CONCURRENT_AGENTS
}

function newTicketId(): string {
  return randomBytes(8).toString('hex')
}

class BackgroundAgentScheduler {
  private running = new Map<string, RunningSlot>() // ticketId -> running slot
  private queue: QueueEntry[] = []
  private slotFreedListeners = new Set<SlotFreedListener>()
  private shuttingDown = false
  private detachedTasks = new Map<
    string,
    { controller: AbortController; ticket: AgentSlotTicket }
  >()
  private shutdownWaiters = new Set<() => void>()
  private workspaceTransitionCount = 0
  private workspaceTransitionTail: Promise<void> = Promise.resolve()

  capacity(): number {
    return readCapacityFromEnv()
  }

  runningCount(): number {
    return this.running.size
  }

  queueLength(): number {
    return this.queue.length
  }

  /**
   * R3 observability (2026-06-18): list running slots whose hold duration has
   * crossed `thresholdMs`. Pure read of acquire timestamps — never mutates
   * scheduler state, never releases a slot. `nowMs` is injectable for tests.
   *
   * A slot legitimately runs as long as its turn (bounded by the 30-min turn
   * watchdog + the 90s stream-idle watchdog in queryModel). A slot held well
   * past that is the symptom of the latent risk R3 flags: the model-stream
   * terminal release (onModelStreamTerminal) failing to fire. Surfacing it here
   * lets ops confirm whether that ever actually happens before building a lease.
   */
  longHeldSlots(
    thresholdMs: number = readSlotHeldWarnMsFromEnv(),
    nowMs: number = Date.now(),
  ): LongHeldSlot[] {
    const out: LongHeldSlot[] = []
    for (const [ticketId, slot] of this.running) {
      const heldMs = nowMs - slot.acquiredAt
      if (heldMs >= thresholdMs) {
        out.push({ ticketId, taskId: slot.taskId, heldMs })
      }
    }
    return out
  }

  /**
   * Try to acquire a slot synchronously. Returns a ticket if a slot was
   * available, otherwise null. Use when the caller wants to detect "would
   * queue" without committing to await.
   *
   * For background AgentTool entry points, prefer `acquire` so the queue path
   * is taken transparently.
   */
  tryAcquireSync(taskId: string): AgentSlotTicket | null {
    if (this.admissionClosed()) return null
    if (this.running.size >= this.capacity()) {
      return null
    }
    return this.allocate(taskId)
  }

  /**
   * Acquire a slot, queuing if necessary. The returned ticket's
   * `whenAvailable` promise resolves once the slot is held. Callers must
   * `release()` after the model stream reaches a terminal state.
   *
   * Queued tickets may be cancelled via `cancelIfQueued()`. Cancelled queued
   * tickets reject `whenAvailable` with `BackgroundSlotCancelledError`.
   */
  acquire(taskId: string): AgentSlotTicket {
    if (this.admissionClosed()) {
      return this.cancelledTicket(taskId)
    }
    if (this.running.size < this.capacity()) {
      return this.allocate(taskId)
    }
    const ticketId = newTicketId()
    let resolveFn!: () => void
    let rejectFn!: (err: Error) => void
    const whenAvailable = new Promise<void>((resolve, reject) => {
      resolveFn = resolve
      rejectFn = reject
    })
    const entry: QueueEntry = {
      ticketId,
      taskId,
      resolve: resolveFn,
      reject: rejectFn,
      cancelled: false,
    }
    this.queue.push(entry)
    // R3 observability (2026-06-18): contention just happened (a caller had to
    // queue because every slot is busy). This is the cheapest natural trigger to
    // check "is some slot stuck?" — if a running slot has been held abnormally
    // long, log it. Pure diagnostic; does not affect this acquire or any slot.
    this.warnIfSlotsLongHeld()
    this.notifyStateChanged()
    let released = false
    const ticket: AgentSlotTicket = {
      ticketId,
      taskId,
      queuedAtCreation: true,
      whenAvailable: whenAvailable.then(() => {
        // Promoted from queue → running: counter already incremented in pumpQueue.
      }),
      cancelIfQueued: () => {
        if (released) return false
        if (this.running.has(ticketId)) return false
        if (entry.cancelled) return false
        entry.cancelled = true
        entry.reject(new BackgroundSlotCancelledError(taskId))
        const idx = this.queue.indexOf(entry)
        if (idx >= 0) this.queue.splice(idx, 1)
        this.notifyStateChanged()
        return true
      },
      release: () => {
        if (released) return
        released = true
        if (this.running.delete(ticketId)) {
          this.pumpQueue()
          this.notifySlotFreed()
          this.notifyStateChanged()
        } else {
          // Was queued at release time (e.g. caller bailed on whenAvailable).
          const idx = this.queue.indexOf(entry)
          if (idx >= 0) {
            this.queue.splice(idx, 1)
            entry.cancelled = true
            // Avoid leaving a pending promise: reject as cancelled.
            entry.reject(new BackgroundSlotCancelledError(taskId))
            this.notifyStateChanged()
          }
        }
      },
      isActive: () => !released,
    }
    return ticket
  }

  /**
   * Cancel a queued ticket by taskId. Returns true if at least one queued
   * entry was removed. Running tickets are NOT cancelled (use the ticket's
   * `release()` after the caller aborts the agent).
   */
  cancelQueuedByTaskId(taskId: string): boolean {
    let removed = false
    for (let i = this.queue.length - 1; i >= 0; i--) {
      const entry = this.queue[i]!
      if (entry.taskId === taskId && !entry.cancelled) {
        entry.cancelled = true
        entry.reject(new BackgroundSlotCancelledError(taskId))
        this.queue.splice(i, 1)
        removed = true
      }
    }
    if (removed) {
      this.notifyStateChanged()
    }
    return removed
  }

  /**
   * Cancel every scheduler-owned lifecycle for `taskId`. Queued tickets are
   * rejected immediately; tracked running work receives its exact
   * AbortController and retains the slot until its promise actually settles.
   */
  cancelByTaskId(taskId: string): boolean {
    let cancelled = this.cancelQueuedByTaskId(taskId)
    for (const tracked of this.detachedTasks.values()) {
      if (tracked.ticket.taskId !== taskId) continue
      tracked.controller.abort()
      cancelled = true
    }
    return cancelled
  }

  /**
   * Subscribe to slot-freed events. Foreground tool executors (e.g. the
   * StreamingToolExecutor) use this to nudge their own queues when a
   * background slot is released, since they don't otherwise observe
   * scheduler state. Returns an unsubscribe function.
   */
  onSlotFreed(listener: SlotFreedListener): () => void {
    this.slotFreedListeners.add(listener)
    return () => {
      this.slotFreedListeners.delete(listener)
    }
  }

  /**
   * Bind a fire-and-forget AgentTool lifecycle to its abort authority and
   * scheduler ticket. The normalized settlement handler owns the final
   * idempotent release, so thrown detached tasks cannot strand a running slot.
   */
  trackDetachedTask(
    ticket: AgentSlotTicket,
    controller: AbortController,
    task: PromiseLike<unknown>,
  ): void {
    this.detachedTasks.set(ticket.ticketId, { controller, ticket })
    if (this.admissionClosed()) controller.abort()
    Promise.resolve(task).then(
      () => this.finishDetachedTask(ticket),
      () => this.finishDetachedTask(ticket),
    )
    this.notifyStateChanged()
  }

  /** Close admission synchronously, cancel queued/running detached work. */
  beginShutdown(): void {
    if (this.shuttingDown) return
    this.shuttingDown = true
    this.cancelQueuedAndAbortTracked()
  }

  /**
   * Wait until all tracked detached lifecycles and every scheduler slot have
   * settled. Untracked slots belong to an admitted foreground turn; worker
   * shutdown must wait for that turn instead of clearing bookkeeping and
   * pretending its side effects stopped.
   */
  async shutdown(): Promise<void> {
    this.beginShutdown()
    await this.waitUntilIdle()
  }

  /**
   * Serialize a workspace security-state transition behind a full background
   * agent quiescence barrier. Admission closes synchronously when this method
   * is invoked; queued work is rejected and tracked work is aborted, but its
   * slot is retained until the detached promise settles.
   *
   * Admission reopens only after the last queued transition completes and
   * only if permanent runtime shutdown has not begun.
   */
  async runWorkspaceTransition<T>(operation: () => Promise<T>): Promise<T> {
    this.beginWorkspaceTransitionQuiesce()

    const predecessor = this.workspaceTransitionTail
    let releaseTransition!: () => void
    const transitionComplete = new Promise<void>(resolve => {
      releaseTransition = resolve
    })
    this.workspaceTransitionTail = predecessor
      .catch(() => {})
      .then(() => transitionComplete)

    await predecessor.catch(() => {})
    try {
      await this.settleWorkspaceTransitionQuiesce()
      return await operation()
    } finally {
      releaseTransition()
      this.endWorkspaceTransitionQuiesce()
    }
  }

  /** Close temporary workspace-transition admission synchronously. */
  beginWorkspaceTransitionQuiesce(): void {
    this.workspaceTransitionCount += 1
    this.cancelQueuedAndAbortTracked()
  }

  /** Wait for every scheduler slot/task admitted before the transition. */
  settleWorkspaceTransitionQuiesce(): Promise<void> {
    return this.waitUntilIdle()
  }

  /** Release one temporary transition latch without overriding shutdown. */
  endWorkspaceTransitionQuiesce(): void {
    if (this.workspaceTransitionCount <= 0) return
    this.workspaceTransitionCount -= 1
    this.notifyStateChanged()
  }

  /**
   * Reset state for tests. Not for production use; running callbacks will
   * leak. Queued tickets are silently dropped — we do NOT call their reject
   * to avoid unhandled-rejection noise in test runners. Tests that need to
   * observe rejection should call `cancelQueuedByTaskId` explicitly with a
   * `.catch` already attached.
   */
  resetForTests(): void {
    for (const tracked of this.detachedTasks.values()) {
      tracked.controller.abort()
    }
    for (const entry of this.queue) {
      entry.cancelled = true
    }
    this.queue = []
    this.running.clear()
    this.detachedTasks.clear()
    this.shuttingDown = false
    this.workspaceTransitionCount = 0
    this.workspaceTransitionTail = Promise.resolve()
    this.slotFreedListeners.clear()
    for (const resolve of this.shutdownWaiters) resolve()
    this.shutdownWaiters.clear()
  }

  private finishDetachedTask(ticket: AgentSlotTicket): void {
    ticket.release()
    this.detachedTasks.delete(ticket.ticketId)
    this.notifyStateChanged()
  }

  private admissionClosed(): boolean {
    return this.shuttingDown || this.workspaceTransitionCount > 0
  }

  private cancelQueuedAndAbortTracked(): void {
    const queued = this.queue.splice(0)
    for (const entry of queued) {
      if (entry.cancelled) continue
      entry.cancelled = true
      entry.reject(new BackgroundSlotCancelledError(entry.taskId))
    }
    for (const tracked of this.detachedTasks.values()) {
      tracked.controller.abort()
    }
    this.notifyStateChanged()
  }

  private async waitUntilIdle(): Promise<void> {
    while (
      this.detachedTasks.size > 0 ||
      this.running.size > 0 ||
      this.queue.length > 0
    ) {
      for (const tracked of this.detachedTasks.values()) {
        tracked.controller.abort()
      }
      await new Promise<void>(resolve => {
        this.shutdownWaiters.add(resolve)
      })
    }
  }

  private cancelledTicket(taskId: string): AgentSlotTicket {
    const ticketId = newTicketId()
    const whenAvailable = Promise.reject(
      new BackgroundSlotCancelledError(taskId),
    )
    // Keep the public promise rejected for callers while marking it handled
    // during the synchronous acquire→detached-callback handoff.
    void whenAvailable.catch(() => {})
    let active = true
    return {
      ticketId,
      taskId,
      queuedAtCreation: true,
      whenAvailable,
      cancelIfQueued: () => {
        const wasActive = active
        active = false
        return wasActive
      },
      release: () => {
        active = false
      },
      isActive: () => active,
    }
  }

  private allocate(taskId: string): AgentSlotTicket {
    const ticketId = newTicketId()
    this.running.set(ticketId, { taskId, acquiredAt: Date.now() })
    this.notifyStateChanged()
    let released = false
    const whenAvailable = Promise.resolve()
    const ticket: AgentSlotTicket = {
      ticketId,
      taskId,
      queuedAtCreation: false,
      whenAvailable,
      cancelIfQueued: () => false,
      release: () => {
        if (released) return
        released = true
        if (this.running.delete(ticketId)) {
          this.pumpQueue()
          this.notifySlotFreed()
          this.notifyStateChanged()
        }
      },
      isActive: () => !released,
    }
    return ticket
  }

  private pumpQueue(): void {
    while (this.running.size < this.capacity() && this.queue.length > 0) {
      const entry = this.queue.shift()
      if (!entry || entry.cancelled) continue
      this.running.set(entry.ticketId, { taskId: entry.taskId, acquiredAt: Date.now() })
      try {
        entry.resolve()
      } catch {
        // Listener failures must not stall the pump.
      }
    }
    // Callers notify after the pump completes so waiters observe final state.
  }

  private notifySlotFreed(): void {
    for (const listener of this.slotFreedListeners) {
      try {
        listener()
      } catch {
        // Listener failures must not stall the pump.
      }
    }
  }

  private notifyStateChanged(): void {
    if (this.shutdownWaiters.size > 0) {
      const waiters = Array.from(this.shutdownWaiters)
      this.shutdownWaiters.clear()
      for (const resolve of waiters) resolve()
    }
  }

  /**
   * R3 observability (2026-06-18): emit a single aggregated warning when one or
   * more running slots have been held past the diagnostic threshold. Called on
   * queue contention (see `acquire`). Best-effort: a logging failure must never
   * affect scheduler state, so it is swallowed.
   */
  private warnIfSlotsLongHeld(): void {
    try {
      const stuck = this.longHeldSlots()
      if (stuck.length === 0) return
      const detail = stuck
        .map(s => `${s.taskId}=${Math.round(s.heldMs / 1000)}s`)
        .join(', ')
      console.warn(
        `[BackgroundAgentScheduler] ${stuck.length} running slot(s) held past ` +
          `${Math.round(readSlotHeldWarnMsFromEnv() / 60_000)}min while a new agent ` +
          `is queued — possible un-released slot (model-stream terminal release may ` +
          `have failed to fire): ${detail}`,
      )
    } catch {
      // Diagnostics must never destabilize the scheduler.
    }
  }
}

export class BackgroundSlotCancelledError extends Error {
  constructor(public readonly taskId: string) {
    super(`Background agent slot cancelled for task ${taskId}`)
    this.name = 'BackgroundSlotCancelledError'
  }
}

const singleton = new BackgroundAgentScheduler()

/** Process-singleton scheduler used by all AgentTool entry points. */
export function getBackgroundAgentScheduler(): BackgroundAgentScheduler {
  return singleton
}
