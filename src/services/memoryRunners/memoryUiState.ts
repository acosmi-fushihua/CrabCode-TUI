import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import { getAutoMemPath } from '../../memdir/paths.js'

export type MemoryUiState = {
  autoDreamEnabled: boolean
  lastConsolidatedAt: number
}

const DEFAULT_STATE: MemoryUiState = {
  autoDreamEnabled: false,
  lastConsolidatedAt: 0,
}

let memoryUiState: MemoryUiState = DEFAULT_STATE
const listeners = new Set<() => void>()

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function emitMemoryUiState(): void {
  for (const listener of listeners) listener()
}

function setMemoryUiState(next: MemoryUiState): MemoryUiState {
  if (
    memoryUiState.autoDreamEnabled === next.autoDreamEnabled &&
    memoryUiState.lastConsolidatedAt === next.lastConsolidatedAt
  ) {
    return memoryUiState
  }
  memoryUiState = next
  emitMemoryUiState()
  return memoryUiState
}

function patchMemoryUiState(patch: Partial<MemoryUiState>): MemoryUiState {
  return setMemoryUiState({ ...memoryUiState, ...patch })
}

function readEnabledResponse(response: unknown): boolean | undefined {
  return isRecord(response) && typeof response.enabled === 'boolean'
    ? response.enabled
    : undefined
}

function readMtimeResponse(response: unknown): number | undefined {
  return isRecord(response) &&
    typeof response.mtime_ms === 'number' &&
    Number.isFinite(response.mtime_ms)
    ? response.mtime_ms
    : undefined
}

export function subscribeMemoryUiState(
  listener: () => void,
): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

export function getMemoryUiStateSnapshot(): MemoryUiState {
  return memoryUiState
}

export async function prefetchMemoryUiState(
  timeoutMs = 500,
): Promise<MemoryUiState> {
  const [enabled, mtime] = await Promise.all([
    memoryBridgeIpc
      .send(
        'memory.dream.is_enabled',
        { memory_dir: getAutoMemPath() },
        { timeout_ms: timeoutMs },
      )
      .then(readEnabledResponse)
      .catch(() => undefined),
    memoryBridgeIpc
      .send(
        'memory.lock.last_consolidated_at',
        { memory_dir: getAutoMemPath() },
        { timeout_ms: timeoutMs },
      )
      .then(readMtimeResponse)
      .catch(() => undefined),
  ])
  return patchMemoryUiState({
    autoDreamEnabled: enabled ?? memoryUiState.autoDreamEnabled,
    lastConsolidatedAt: mtime ?? memoryUiState.lastConsolidatedAt,
  })
}

export async function setAutoDreamEnabled(
  enabled: boolean,
): Promise<void> {
  const previous = memoryUiState.autoDreamEnabled
  patchMemoryUiState({ autoDreamEnabled: enabled })
  try {
    const response = await memoryBridgeIpc.send(
      'memory.dream.set_enabled',
      { enabled, memory_dir: getAutoMemPath() },
      { timeout_ms: 500 },
    )
    patchMemoryUiState({
      autoDreamEnabled: readEnabledResponse(response) ?? enabled,
    })
  } catch (error) {
    patchMemoryUiState({ autoDreamEnabled: previous })
    throw error
  }
}

export async function toggleAutoDreamEnabled(): Promise<void> {
  await setAutoDreamEnabled(!memoryUiState.autoDreamEnabled)
}

export function setMemoryUiStateForTesting(
  state?: Partial<MemoryUiState>,
): void {
  memoryUiState = { ...DEFAULT_STATE, ...state }
  emitMemoryUiState()
}
