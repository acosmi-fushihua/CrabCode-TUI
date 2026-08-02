/**
 * Types for the message queue system.
 *
 * QueueOperation — the action that was performed on the queue.
 * QueueOperationMessage — a serializable log entry recorded to the transcript.
 */

/** Operations that can be performed on the command queue. */
export type QueueOperation = 'enqueue' | 'dequeue' | 'remove' | 'popAll'

/** Transcript log entry that records a queue operation for debugging/replay. */
export type QueueOperationMessage = {
  type: 'queue-operation'
  operation: QueueOperation
  timestamp: string
  sessionId: string
  /**
   * W-CRON-SESSION-INJECTION-ISOLATION L6: the threadId of the turn that
   * actually performed this operation (`getCurrentTurnThreadId()`), when a
   * worker session override is installed. `sessionId` above is the
   * process-global identity, which can be a stale "ghost" under multi-session
   * worker multiplexing (the value that made this incident hard to diagnose —
   * an enqueue logged under one session while another thread drained it).
   * A divergence between `consumerThreadId` and a command's owner is a
   * cross-session leak, now visible directly in the transcript. Undefined in
   * the single-session TUI (no override).
   */
  consumerThreadId?: string
  content?: string
}
