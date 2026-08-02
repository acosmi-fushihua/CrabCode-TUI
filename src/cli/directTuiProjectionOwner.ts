import { getSessionId } from '../bootstrap/state.js'
import { runWithBackgroundTaskOwner } from '../services/agents/agentTaskMetadata.js'

/**
 * Bind one direct-TUI turn to the session identity already used by transcript
 * persistence. The identity is captured once
 * at turn entry so a later session switch cannot rewrite ownership mid-turn.
 *
 * Non-interactive routes pass `enabled = false` and therefore do not acquire
 * this TUI-owned context.
 */
export function runWithDirectTuiProjectionOwner<T>(
  enabled: boolean,
  fn: () => T,
): T {
  if (!enabled) return fn()
  const sessionId = getSessionId()
  return runWithBackgroundTaskOwner(sessionId, fn)
}
