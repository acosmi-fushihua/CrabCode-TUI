export type SessionState = 'idle' | 'running' | 'requires_action'

/** Context retained while a permission request blocks the TUI runtime. */
export type RequiresActionDetails = {
  tool_name: string
  /** Human-readable summary, e.g. "Editing src/foo.ts", "Running npm test" */
  action_description: string
  tool_use_id: string
  request_id: string
  /** Raw tool input for the local permission request lifecycle. */
  input?: Record<string, unknown>
}

import { isEnvTruthy } from './envUtils.js'
import type { PermissionMode } from './permissions/PermissionMode.js'
import { enqueueSdkEvent } from './sdkEventQueue.js'

type PermissionModeChangedListener = (mode: PermissionMode) => void

let permissionModeListener: PermissionModeChangedListener | null = null

/**
 * Register a listener for permission-mode changes from onChangeAppState.
 * Wired by the TUI runtime to emit an SDK status message regardless of which
 * code path mutated toolPermissionContext.mode.
 */
export function setPermissionModeChangedListener(
  cb: PermissionModeChangedListener | null,
): void {
  permissionModeListener = cb
}

let currentState: SessionState = 'idle'

export function getSessionState(): SessionState {
  return currentState
}

export function notifySessionStateChanged(
  state: SessionState,
): void {
  currentState = state

  // Mirror to the local SDK event stream so the native TUI can observe the
  // authoritative idle/running transition.
  // 'idle' fires after heldBackResult flushes so the renderer cannot retain a
  // stale generating state.
  //
  // Kept behind an opt-in because consumers may treat any trailing system
  // event as an active turn.
  if (isEnvTruthy(process.env.CRABCODE_EMIT_SESSION_STATE_EVENTS)) {
    enqueueSdkEvent({
      type: 'system',
      subtype: 'session_state_changed',
      state,
    })
  }
}

/**
 * Fired by onChangeAppState when toolPermissionContext.mode changes.
 * The SDK status stream is wired through this choke point so no mode-mutation
 * path can silently bypass it.
 */
export function notifyPermissionModeChanged(mode: PermissionMode): void {
  permissionModeListener?.(mode)
}
