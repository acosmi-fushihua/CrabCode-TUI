import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import { getAutoMemPath } from '../../../memdir/paths.js'
import { logForDebugging } from '../../../utils/debug.js'
import { getInitialSettings } from '../../../utils/settings/settings.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../../analytics/growthbook.js'

const STATE_TIMEOUT_MS = 1000

export function isAutoDreamEnabled(): boolean {
  const setting = getInitialSettings().autoDreamEnabled
  if (setting !== undefined) return setting
  const gb = getFeatureValue_CACHED_MAY_BE_STALE<{ enabled?: unknown } | null>(
    'tengu_onyx_plover',
    null,
  )
  // Default on. An explicit GrowthBook object can still force either direction
  // as a kill switch, matching the Rust DreamConfig default.
  if (gb !== null && gb !== undefined && typeof gb.enabled === 'boolean') {
    return gb.enabled
  }
  return true
}

export async function setAutoDreamEnabled(enabled: boolean): Promise<void> {
  try {
    await memoryBridgeIpc.send(
      'memory.dream.set_enabled',
      { enabled, memory_dir: getAutoMemPath() },
      { timeout_ms: STATE_TIMEOUT_MS },
    )
  } catch (error) {
    logForDebugging(`[autoDream] set enabled IPC failed: ${error}`)
  }
}

export async function readLastConsolidatedAt(): Promise<number> {
  try {
    const response = await memoryBridgeIpc.send(
      'memory.lock.last_consolidated_at',
      { memory_dir: getAutoMemPath() },
      { timeout_ms: STATE_TIMEOUT_MS },
    )
    if (
      response !== null &&
      typeof response === 'object' &&
      typeof (response as { mtime_ms?: unknown }).mtime_ms === 'number'
    ) {
      return (response as { mtime_ms: number }).mtime_ms
    }
  } catch (error) {
    logForDebugging(`[autoDream] read last consolidated IPC failed: ${error}`)
  }
  return 0
}

export async function rollbackConsolidationLock(
  priorMtime: number,
): Promise<void> {
  try {
    await memoryBridgeIpc.send(
      'memory.lock.rollback',
      { memory_dir: getAutoMemPath(), prior_mtime_ms: priorMtime },
      { timeout_ms: STATE_TIMEOUT_MS },
    )
  } catch (error) {
    logForDebugging(
      `[autoDream] rollback failed: ${error} — next trigger delayed to minHours`,
    )
  }
}
