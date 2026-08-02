import { logForDebugging } from '../../utils/debug.js'
import { isBareMode } from '../../utils/envUtils.js'
import { errorMessage } from '../../utils/errors.js'
import { logError } from '../../utils/log.js'
import {
  createLSPServerManager,
  type LSPServerManager,
} from './LSPServerManager.js'
import { registerLSPNotificationHandlers } from './passiveFeedback.js'

/**
 * Initialization state of the LSP server manager
 */
type InitializationState = 'not-started' | 'pending' | 'success' | 'failed'

/**
 * Global singleton instance of the LSP server manager.
 * Initialized during CrabCode startup.
 */
let lspManagerInstance: LSPServerManager | undefined

/**
 * Current initialization state
 */
let initializationState: InitializationState = 'not-started'

/**
 * Error from last initialization attempt, if any
 */
let initializationError: Error | undefined

/**
 * Generation counter to prevent stale initialization promises from updating state
 */
let initializationGeneration = 0

/**
 * Promise that resolves when initialization completes (success or failure)
 */
let initializationPromise: Promise<void> | undefined
let reinitializationPromise: Promise<void> | undefined
let lspRuntimeShuttingDown = false

/**
 * Test-only sync reset. shutdownLspServerManager() is async and tears down
 * real connections; this only clears the module-scope singleton state so
 * reinitializeLspServerManager() early-returns on 'not-started' in downstream
 * tests on the same shard.
 */
export function _resetLspManagerForTesting(): void {
  lspManagerInstance = undefined
  initializationState = 'not-started'
  initializationError = undefined
  initializationPromise = undefined
  reinitializationPromise = undefined
  lspRuntimeShuttingDown = false
  initializationGeneration++
}

/**
 * Get the singleton LSP server manager instance.
 * Returns undefined if not yet initialized, initialization failed, or still pending.
 *
 * Callers should check for undefined and handle gracefully, as initialization happens
 * asynchronously during CrabCode startup. Use getInitializationStatus() to
 * distinguish between pending, failed, and not-started states.
 */
export function getLspServerManager(): LSPServerManager | undefined {
  if (lspRuntimeShuttingDown) return undefined
  // Don't return a broken instance if initialization failed
  if (initializationState === 'failed') {
    return undefined
  }
  return lspManagerInstance
}

/**
 * Get the current initialization status of the LSP server manager.
 *
 * @returns Status object with current state and error (if failed)
 */
export function getInitializationStatus():
  | { status: 'not-started' }
  | { status: 'pending' }
  | { status: 'success' }
  | { status: 'failed'; error: Error } {
  if (initializationState === 'failed') {
    return {
      status: 'failed',
      error: initializationError || new Error('Initialization failed'),
    }
  }
  if (initializationState === 'not-started') {
    return { status: 'not-started' }
  }
  if (initializationState === 'pending') {
    return { status: 'pending' }
  }
  return { status: 'success' }
}

/**
 * Check whether at least one language server is connected and healthy.
 * Backs LSPTool.isEnabled().
 */
export function isLspConnected(): boolean {
  if (initializationState === 'failed') return false
  const manager = getLspServerManager()
  if (!manager) return false
  const servers = manager.getAllServers()
  if (servers.size === 0) return false
  for (const server of servers.values()) {
    if (server.state !== 'error') return true
  }
  return false
}

/**
 * Wait for LSP server manager initialization to complete.
 *
 * Returns immediately if initialization has already completed (success or failure).
 * If initialization is pending, waits for it to complete.
 * If initialization hasn't started, returns immediately.
 *
 * @returns Promise that resolves when initialization is complete
 */
export async function waitForInitialization(): Promise<void> {
  // If already initialized or failed, return immediately
  if (initializationState === 'success' || initializationState === 'failed') {
    return
  }

  // If pending and we have a promise, wait for it
  if (initializationState === 'pending' && initializationPromise) {
    await initializationPromise
  }

  // If not started, return immediately (nothing to wait for)
}

/**
 * Initialize the LSP server manager singleton.
 *
 * This function is called during CrabCode startup. It synchronously creates
 * the manager instance, then starts async initialization (loading LSP configs)
 * in the background without blocking the startup process.
 *
 * Safe to call multiple times - will only initialize once (idempotent).
 * However, if initialization previously failed, calling again will retry.
 */
export function initializeLspServerManager(): void {
  // --bare / SIMPLE: no LSP. LSP is for editor integration (diagnostics,
  // hover, go-to-def in the REPL). Scripted -p calls have no use for it.
  if (isBareMode() || lspRuntimeShuttingDown) {
    return
  }
  logForDebugging('[LSP MANAGER] initializeLspServerManager() called')

  // Skip if already initialized or currently initializing
  if (lspManagerInstance !== undefined && initializationState !== 'failed') {
    logForDebugging(
      '[LSP MANAGER] Already initialized or initializing, skipping',
    )
    return
  }

  // Reset state for retry if previous initialization failed
  if (initializationState === 'failed') {
    lspManagerInstance = undefined
    initializationError = undefined
  }

  // Create the manager instance and mark as pending
  lspManagerInstance = createLSPServerManager()
  initializationState = 'pending'
  logForDebugging('[LSP MANAGER] Created manager instance, state=pending')

  // Increment generation to invalidate any pending initializations
  const currentGeneration = ++initializationGeneration
  logForDebugging(
    `[LSP MANAGER] Starting async initialization (generation ${currentGeneration})`,
  )

  // Start initialization asynchronously without blocking
  // Store the promise so callers can await it via waitForInitialization()
  initializationPromise = lspManagerInstance
    .initialize()
    .then(() => {
      // Only update state if this is still the current initialization
      if (currentGeneration === initializationGeneration) {
        initializationState = 'success'
        logForDebugging('LSP server manager initialized successfully')

        // Register passive notification handlers for diagnostics
        if (lspManagerInstance) {
          registerLSPNotificationHandlers(lspManagerInstance)
        }
      }
    })
    .catch((error: unknown) => {
      // Only update state if this is still the current initialization
      if (currentGeneration === initializationGeneration) {
        initializationState = 'failed'
        initializationError = error as Error
        // Clear the instance since it's not usable
        lspManagerInstance = undefined

        logError(error as Error)
        logForDebugging(
          `Failed to initialize LSP server manager: ${errorMessage(error)}`,
        )
      }
    })
}

/**
 * Force re-initialization of the LSP server manager, even after a prior
 * successful init. Called from refreshActivePlugins() after plugin caches
 * are cleared, so newly-loaded plugin LSP servers are picked up.
 *
 * Fixes https://github.com/acosmi/crabcode/issues/15521:
 * loadAllPlugins() is memoized and can be called very early in startup
 * (via getCommands prefetch in setup.ts) before marketplaces are reconciled,
 * caching an empty plugin list. initializeLspServerManager() then reads that
 * stale memoized result and initializes with 0 servers. Unlike commands/agents/
 * hooks/MCP, LSP was never re-initialized on plugin refresh.
 *
 * Safe to call when no LSP plugins changed: initialize() is just config
 * parsing (servers are lazy-started on first use). Also safe during pending
 * init: the generation counter invalidates the in-flight promise.
 */
export function reinitializeLspServerManager(): Promise<void> {
  if (
    initializationState === 'not-started' ||
    lspRuntimeShuttingDown
  ) {
    // initializeLspServerManager() was never called (e.g. headless subcommand
    // path). Don't start it now.
    return Promise.resolve()
  }
  if (reinitializationPromise) return reinitializationPromise

  logForDebugging('[LSP MANAGER] reinitializeLspServerManager() called')

  const owned = (async () => {
    // Initialization publishes into the same manager instance. Wait for that
    // producer before trying to settle and replace the generation.
    if (initializationPromise) await initializationPromise
    if (lspRuntimeShuttingDown) return

    const previous = lspManagerInstance
    if (previous) {
      await previous.shutdown()
    }
    if (lspManagerInstance === previous) {
      lspManagerInstance = undefined
      initializationState = 'not-started'
      initializationError = undefined
      initializationPromise = undefined
      initializationGeneration++
    }
    if (lspRuntimeShuttingDown) return

    initializeLspServerManager()
    await waitForInitialization()
  })()
  let tracked!: Promise<void>
  tracked = owned.finally(() => {
    if (reinitializationPromise === tracked) {
      reinitializationPromise = undefined
    }
  })
  reinitializationPromise = tracked
  return tracked
}

export function beginLspRuntimeShutdown(): void {
  lspRuntimeShuttingDown = true
}

/**
 * Shutdown the LSP server manager and clean up resources.
 *
 * This should be called during CrabCode shutdown. Stops all running LSP servers
 * and clears internal state. Safe to call when not initialized (no-op).
 *
 * NOTE: Errors during shutdown are logged for monitoring but NOT propagated to the caller.
 * State is always cleared even if shutdown fails, to prevent resource accumulation.
 * This is acceptable during application exit when recovery is not possible.
 *
 * @returns Promise that resolves when shutdown completes (errors are swallowed)
 */
export async function shutdownLspServerManager(): Promise<'stopped'> {
  beginLspRuntimeShutdown()

  if (reinitializationPromise) {
    try {
      await reinitializationPromise
    } catch (error) {
      // The failed old generation remains authoritative. Continue below and
      // retry its exact shutdown rather than converting the reinit error into
      // a leaked singleton.
      logForDebugging(
        `[LSP MANAGER] reinitialization settlement failed during shutdown: ${errorMessage(error)}`,
      )
    }
  }
  if (initializationPromise) await initializationPromise

  const manager = lspManagerInstance
  if (manager) {
    await manager.shutdown()
    logForDebugging('LSP server manager shut down successfully')
  }

  if (lspManagerInstance === manager) {
    lspManagerInstance = undefined
    initializationState = 'not-started'
    initializationError = undefined
    initializationPromise = undefined
    initializationGeneration++
  }
  return 'stopped'
}
