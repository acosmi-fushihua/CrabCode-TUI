// Phase -1 stub: 待后续实现
// Proactive mode state management — stub providing safe defaults.
// All consumers use feature('PROACTIVE') || feature('KAIROS') guards before
// calling these functions, so they are only reachable when the feature flags
// are enabled. Returning inert values ensures no runtime crash.

type ProactiveActivationSource = 'command' | 'env' | 'sdk'

// ── Internal state ──────────────────────────────────────────────────────────
let _active = false
let _paused = false
let _contextBlocked = false
let _nextTickAt: number | null = null
const _listeners = new Set<() => void>()

function _notify(): void {
  for (const cb of _listeners) {
    try {
      cb()
    } catch {
      // Swallow listener errors
    }
  }
}

// ── Public API (matches all call-site expectations) ─────────────────────────

/**
 * Returns whether proactive mode is currently activated.
 * Used as a React useSyncExternalStore snapshot.
 */
export function isProactiveActive(): boolean {
  return _active
}

/**
 * Returns whether proactive mode is temporarily paused (e.g. user pressed Esc).
 */
export function isProactivePaused(): boolean {
  return _paused
}

/**
 * Activate proactive mode.
 * @param _source - Activation source (unused in stub)
 */
export function activateProactive(_source: ProactiveActivationSource): void {
  _active = true
  _paused = false
  _notify()
}

/**
 * Deactivate proactive mode entirely.
 */
export function deactivateProactive(): void {
  _active = false
  _paused = false
  _contextBlocked = false
  _nextTickAt = null
  _notify()
}

/**
 * Temporarily pause proactive ticks (e.g. user cancelled).
 * Ticks resume when resumeProactive() is called.
 */
export function pauseProactive(): void {
  _paused = true
  _notify()
}

/**
 * Resume proactive ticks after a pause.
 */
export function resumeProactive(): void {
  _paused = false
  _notify()
}

/**
 * Set the context-blocked flag.  When true, ticks should not fire
 * (e.g. after an API error to prevent runaway tick→error loops).
 */
export function setContextBlocked(blocked: boolean): void {
  _contextBlocked = blocked
  _notify()
}

/**
 * Returns true when context-blocked (caller should suppress ticks).
 */
export function isContextBlocked(): boolean {
  return _contextBlocked
}

/**
 * Subscribe to proactive state changes.
 * Compatible with React.useSyncExternalStore.
 * @returns An unsubscribe function.
 */
export function subscribeToProactiveChanges(cb: () => void): () => void {
  _listeners.add(cb)
  return () => {
    _listeners.delete(cb)
  }
}

/**
 * Returns the timestamp (ms) of the next scheduled tick, or null.
 * Compatible with React.useSyncExternalStore snapshot.
 */
export function getNextTickAt(): number | null {
  return _nextTickAt
}
