/**
 * Renderer-neutral compatibility facade for the application state contract.
 *
 * The former React context/provider lived at this path. Pure TUI and backend
 * callers consume only the store contract and defaults.
 */
export {
  type AppState,
  type AppStateStore,
  type CompletionBoundary,
  getDefaultAppState,
  IDLE_SPECULATION_STATE,
  type SpeculationResult,
  type SpeculationState,
} from './AppStateStore.js'
