import type { UUID } from 'crypto'
import { randomUUID } from 'crypto'
import {
  getIsNonInteractiveSession,
  getSessionId,
  getUsesStructuredIoTransport,
} from '../bootstrap/state.js'
import type { SdkWorkflowProgress } from '../types/tools.js'

type TaskStartedEvent = {
  type: 'system'
  subtype: 'task_started'
  task_id: string
  tool_use_id?: string
  description: string
  task_type?: string
  workflow_name?: string
  prompt?: string
}

type TaskProgressEvent = {
  type: 'system'
  subtype: 'task_progress'
  task_id: string
  tool_use_id?: string
  description: string
  usage: {
    total_tokens: number
    tool_uses: number
    duration_ms: number
  }
  last_tool_name?: string
  summary?: string
  // Delta batch of workflow state changes. Clients upsert by
  // `${type}:${index}` then group by phaseIndex to rebuild the phase tree,
  // same fold as collectFromEvents + groupByPhase in PhaseProgress.tsx.
  workflow_progress?: SdkWorkflowProgress[]
}

// Emitted when a foreground agent completes without being backgrounded.
// Drained by drainSdkEvents() directly into the output stream — does NOT
// go through the print.ts XML task_notification parser and does NOT trigger
// the LLM loop. Consumers (e.g. VS Code session.ts) use this to remove the
// task from the subagent panel.
type TaskNotificationSdkEvent = {
  type: 'system'
  subtype: 'task_notification'
  task_id: string
  tool_use_id?: string
  status: 'completed' | 'failed' | 'stopped'
  output_file: string
  summary: string
  usage?: {
    total_tokens: number
    tool_uses: number
    duration_ms: number
  }
}

// Mirrors notifySessionStateChanged. The CCR bridge already receives this
// via its own listener; SDK consumers (scmuxd, VS Code) need the same signal
// to know when the main turn's generator is idle vs actively producing.
// The 'idle' transition fires AFTER heldBackResult flushes and the bg-agent
// do-while loop exits — so SDK consumers can trust it as the authoritative
// "turn is over" signal even when result was withheld for background agents.
type SessionStateChangedEvent = {
  type: 'system'
  subtype: 'session_state_changed'
  state: 'idle' | 'running' | 'requires_action'
}

export type SdkEvent =
  | TaskStartedEvent
  | TaskProgressEvent
  | TaskNotificationSdkEvent
  | SessionStateChangedEvent

// Queue elements are stamped AT ENQUEUE with their identity: `uuid` plus the
// PRODUCER's ambient session (ALS override-aware `getSessionId()` — a detached
// background lifecycle inherits its spawn turn's ALS, so the stamp is the
// owning session). Stamping at drain time let one worker turn's drain claim
// another session's events and re-label them with the drainer's sessionId.
type StampedSdkEvent = SdkEvent & { uuid: UUID; session_id: string }

const MAX_QUEUE_SIZE = 1000
const queue: StampedSdkEvent[] = []

export function enqueueSdkEvent(event: SdkEvent): void {
  // Guard semantics — "does this process have a drain consumer?"
  //   - headless/print (non-interactive): queryExecution.ts drains → enqueue.
  //   - Native Rust TUI StructuredIO: private transport drains these events,
  //     although the product session remains interactive.
  //   - Any runtime with no drainer: drop instead of
  //     accumulating to the cap.
  if (
    !(
      getIsNonInteractiveSession() ||
      getUsesStructuredIoTransport()
    )
  ) {
    return
  }
  if (queue.length >= MAX_QUEUE_SIZE) {
    // Oldest-first eviction — this is also how orphaned events of sessions
    // that never drain (e.g. a foreign session's leftovers) age out.
    queue.shift()
  }
  queue.push({
    ...event,
    uuid: randomUUID(),
    session_id: getSessionId(),
  })
}

export function drainSdkEvents(): Array<
  SdkEvent & { uuid: UUID; session_id: string }
> {
  if (queue.length === 0) {
    return []
  }
  // Session-filtered drain: remove ONLY the events stamped with the drainer's
  // own ambient session; foreign-session events stay queued in their original
  // relative order (single-pass partition, both sides order-preserving).
  const sessionId = getSessionId()
  const drained: StampedSdkEvent[] = []
  const kept: StampedSdkEvent[] = []
  for (const event of queue) {
    if (event.session_id === sessionId) {
      drained.push(event)
    } else {
      kept.push(event)
    }
  }
  if (drained.length === 0) {
    return []
  }
  queue.length = 0
  queue.push(...kept)
  return drained
}

// Test-only reset. Production code must never call this.
export function _resetSdkEventQueueForTests(): void {
  queue.length = 0
}

/**
 * Emit a task_notification SDK event for a task reaching a terminal state.
 *
 * registerTask() always emits task_started; this is the closing bookend.
 * Call this from any exit path that sets a task terminal WITHOUT going
 * through enqueuePendingNotification-with-<task-id> (print.ts parses that
 * XML into the same SDK event, so paths that do both would double-emit).
 * Paths that suppress the XML notification (notified:true pre-set, kill
 * paths, abort branches) must call this directly so SDK consumers
 * (Scuttle's bg-task dot, VS Code subagent panel) see the task close.
 */
export function emitTaskTerminatedSdk(
  taskId: string,
  status: 'completed' | 'failed' | 'stopped',
  opts?: {
    toolUseId?: string
    summary?: string
    outputFile?: string
    usage?: { total_tokens: number; tool_uses: number; duration_ms: number }
  },
): void {
  enqueueSdkEvent({
    type: 'system',
    subtype: 'task_notification',
    task_id: taskId,
    tool_use_id: opts?.toolUseId,
    status,
    output_file: opts?.outputFile ?? '',
    summary: opts?.summary ?? '',
    usage: opts?.usage,
  })
}
