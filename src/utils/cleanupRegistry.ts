/**
 * Global registry for cleanup functions that should run during graceful shutdown.
 * This module is separate from gracefulShutdown.ts to avoid circular dependencies.
 */

// Global registry for cleanup functions
const cleanupFunctions = new Set<() => Promise<void>>()
const cleanupFinalizers = new Set<() => Promise<void>>()

/**
 * Register a cleanup function to run during graceful shutdown.
 * @param cleanupFn - Function to run during cleanup (can be sync or async)
 * @returns Unregister function that removes the cleanup handler
 */
export function registerCleanup(cleanupFn: () => Promise<void>): () => void {
  cleanupFunctions.add(cleanupFn)
  return () => cleanupFunctions.delete(cleanupFn) // Return unregister function
}

/**
 * Register a provider teardown that must run only after ordinary cleanup
 * consumers have settled. Use this for process-owned runtimes whose
 * capabilities may still be needed by background-task cleanup.
 */
export function registerCleanupFinalizer(
  cleanupFn: () => Promise<void>,
): () => void {
  cleanupFinalizers.add(cleanupFn)
  return () => cleanupFinalizers.delete(cleanupFn)
}

/**
 * Whether shutdown currently owns a provider finalizer. This is intentionally
 * state-based rather than mode/env-based: the direct TUI marker is erased at
 * process entry, and an ordinary model must retain the established 2s
 * best-effort cleanup behavior.
 */
export function hasCleanupFinalizers(): boolean {
  return cleanupFinalizers.size > 0
}

async function settleCleanupWithFinalizers(): Promise<{
  consumerResults: PromiseSettledResult<void>[]
  finalizerResults: PromiseSettledResult<void>[]
}> {
  const consumerResults = await Promise.allSettled(
    Array.from(cleanupFunctions).map(fn => fn()),
  )
  const finalizerResults = await Promise.allSettled(
    Array.from(cleanupFinalizers).map(fn => fn()),
  )
  return { consumerResults, finalizerResults }
}

function rejectedReasons(
  results: PromiseSettledResult<void>[],
): unknown[] {
  return results
    .filter(
      (result): result is PromiseRejectedResult =>
        result.status === 'rejected',
    )
    .map(result => result.reason)
}

function throwFailures(failures: unknown[]): void {
  if (failures.length === 1) throw failures[0]
  if (failures.length > 1) {
    throw new AggregateError(failures, 'Multiple cleanup functions failed')
  }
}

/**
 * Run all registered cleanup functions.
 * Used internally by gracefulShutdown.
 */
export async function runCleanupFunctions(): Promise<void> {
  // Preserve the established cleanup semantics for every process that did
  // not instantiate a provider finalizer (including ordinary
  // print and non-Account-Bridge paths).
  if (cleanupFinalizers.size === 0) {
    await Promise.all(Array.from(cleanupFunctions).map(fn => fn()))
    return
  }

  const { consumerResults, finalizerResults } =
    await settleCleanupWithFinalizers()
  throwFailures(rejectedReasons([...consumerResults, ...finalizerResults]))
}

/**
 * Run ordinary cleanup before provider teardown, but make only provider
 * teardown authoritative for the process result. Existing cleanup consumers
 * have always been best-effort during graceful shutdown; changing their
 * failures into a non-zero exit would alter interactive/print semantics.
 *
 * The caller deliberately supplies the outer liveness ceiling. Cutting this
 * operation off at the ordinary 2s cleanup budget is unsafe because the
 * Account Bridge provider itself grants its child 2s before escalating from
 * TERM to KILL and then requires an exit witness.
 */
export async function runCleanupFunctionsRequiringFinalizers(): Promise<void> {
  const { finalizerResults } = await settleCleanupWithFinalizers()
  throwFailures(rejectedReasons(finalizerResults))
}
