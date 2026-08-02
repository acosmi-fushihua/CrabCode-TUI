import { logout as acosmiLogout } from '../acosmi/client.js'
import { logForDebugging } from '../../utils/debug.js'
import { stopLocalModelServerOnLogoutDirect } from '../localModel/directClient.js'
import { clearLocalAuthState } from './localAuthState.js'

/**
 * Renderer-independent implementation of the established CrabCode logout
 * authority. Presentation and process-exit timing remain command-adapter
 * concerns, so the same operation is used by both interactive renderers.
 *
 * Presentation is intentionally absent. The native TUI owns only the
 * in-process direct runtime. Direct cleanup is
 * best-effort, while credential removal stays authoritative and shared.
 */
export async function performLogout({
  clearOnboarding = false,
}: {
  clearOnboarding?: boolean
} = {}): Promise<void> {
  const { flushTelemetry } = await import(
    '../../utils/telemetry/instrumentation.js'
  )
  await flushTelemetry()

  try {
    await acosmiLogout()
  } catch (error) {
    logForDebugging(
      `[logout] SDK acosmiLogout failed (continuing): ${
        error instanceof Error ? error.message : 'unknown'
      }`,
    )
  }

  // Preserve the established logout safety boundary: a signed-out user must
  // not leave the in-process
  // local-model server running. The direct helper retains the original
  // best-effort behavior, so cleanup failure never blocks credential removal.
  await stopLocalModelServerOnLogoutDirect()

  await clearLocalAuthState({ clearOnboarding })
}
