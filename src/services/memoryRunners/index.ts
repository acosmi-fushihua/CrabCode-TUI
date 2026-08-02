import {
  configureMemoryRunnersForTesting,
  drainPendingMemoryRunner,
  onTurnEnd as onTurnEndImpl,
  startDurableMemoryRunnerRecovery,
  stopDurableMemoryRunnerRecovery,
  wakeDurableMemoryRunnerRecovery,
} from './turnEnd.js'
import { prefetchMemoryUiState } from './memoryUiState.js'
import type {
  MemoryRunnerKind,
  MemoryRunnerStopHookContext,
} from './turnEnd.js'

let initialized = false
let startupDrain: Promise<void> | null = null

function ensureStartupDrain(): Promise<void> {
  startupDrain ??= prefetchMemoryUiState().then(
    () => {},
    () => {},
  )
  return startupDrain
}

export function initMemoryRunners(): void {
  if (initialized) return
  initialized = true
  void ensureStartupDrain()
}

export async function drainOnStartup(): Promise<void> {
  await ensureStartupDrain()
}

export function onTurnEnd(
  context: MemoryRunnerStopHookContext,
  appendSystemMessage:
    | MemoryRunnerStopHookContext['toolUseContext']['appendSystemMessage']
    | undefined,
  requestedKinds: readonly MemoryRunnerKind[],
): Promise<void> {
  return onTurnEndImpl(context, appendSystemMessage, requestedKinds)
}

export {
  configureMemoryRunnersForTesting,
  drainPendingMemoryRunner,
  startDurableMemoryRunnerRecovery,
  stopDurableMemoryRunnerRecovery,
  wakeDurableMemoryRunnerRecovery,
}
export { drainPendingExtraction } from './extract/drain.js'
export {
  getMemoryUiStateSnapshot,
  prefetchMemoryUiState,
  setAutoDreamEnabled,
  setMemoryUiStateForTesting,
  toggleAutoDreamEnabled,
} from './memoryUiState.js'
export type { MemoryUiState } from './memoryUiState.js'
export type {
  MemoryRunnerKind,
  MemoryRunnerLeaderFence,
  MemoryRunnerStopHookContext,
  MemoryTriggerRunnerInput,
  MemoryTurnEndPayload,
  MemoryTurnEndTrigger,
} from './turnEnd.js'
