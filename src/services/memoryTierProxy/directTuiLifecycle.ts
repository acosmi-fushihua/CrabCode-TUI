import {
  getIsInteractive,
  getLastInteractionTime,
} from '../../bootstrap/state.js'
import { startDirectTuiDeferredPrefetches } from '../../main/directTuiPrefetch.js'
import { cleanupDirectTuiUserDataInBackground } from '../../utils/cleanup.js'
import { registerCleanup } from '../../utils/cleanupRegistry.js'
import {
  countConcurrentSessions,
  registerSession,
  updateSessionName,
} from '../../utils/concurrentSessions.js'
import { initSkillImprovement } from '../../utils/hooks/skillImprovement.js'
import { autoUpdateMarketplacesAndPluginsInBackground } from '../../utils/plugins/pluginAutoupdate.js'
import { initMagicDocs } from '../MagicDocs/magicDocs.js'
import { logEvent } from '../analytics/index.js'
import { prefetchMemoryUiState } from '../memoryRunners/memoryUiState.js'
import {
  bootstrapMemoryBridgeAndMaybeRunners,
  releaseLeaderIfHeld,
} from './leaderBootstrap.js'

let started = false
const DELAY_VERY_SLOW_OPERATIONS_THAT_HAPPEN_EVERY_SESSION =
  10 * 60 * 1000

/**
 * Start the backend-only lifecycle owned by the direct native TUI.
 *
 * This mirrors the non-presentation work of backgroundHousekeeping plus the
 * deferred REPL prefetches. The OS deep-link registration surface is
 * deliberately absent; everything retained here is an existing TUI backend
 * capability or process-lifecycle responsibility.
 */
export function startDirectTuiBackendLifecycle(sessionName?: string): void {
  if (started) return
  started = true

  startDirectTuiDeferredPrefetches()
  void initMagicDocs()
  initSkillImprovement()
  void prefetchMemoryUiState().catch(() => {
    // Fail-soft UI snapshot: durable capability admission is owned below.
  })
  void bootstrapMemoryBridgeAndMaybeRunners().catch(error => {
    process.stderr.write(
      `[memory-runtime] bootstrap failed (session continues without background memory runners): ${
        error instanceof Error ? error.message : String(error)
      }\n`,
    )
  })
  registerCleanup(releaseLeaderIfHeld)
  autoUpdateMarketplacesAndPluginsInBackground()
  void registerSession().then(registered => {
    if (!registered) return
    if (sessionName) void updateSessionName(sessionName)
    void countConcurrentSessions().then(count => {
      if (count >= 2) {
        logEvent('tengu_concurrent_sessions', {
          num_sessions: count,
        })
      }
    })
  })

  async function runVerySlowOps(): Promise<void> {
    if (
      getIsInteractive() &&
      getLastInteractionTime() > Date.now() - 60_000
    ) {
      setTimeout(
        runVerySlowOps,
        DELAY_VERY_SLOW_OPERATIONS_THAT_HAPPEN_EVERY_SESSION,
      ).unref()
      return
    }
    await cleanupDirectTuiUserDataInBackground()
  }

  setTimeout(
    runVerySlowOps,
    DELAY_VERY_SLOW_OPERATIONS_THAT_HAPPEN_EVERY_SESSION,
  ).unref()
}

export function _resetDirectTuiMemoryLifecycleForTesting(): void {
  started = false
}
