// Auto-generated stub

export type CollapseHealth = {
  totalSpawns: number
  totalErrors: number
  lastError: string | null
  totalEmptySpawns: number
  emptySpawnWarningEmitted: boolean
}

export type CollapseStats = {
  collapsedSpans: number
  collapsedMessages: number
  stagedSpans: number
  health: CollapseHealth
}

export function getStats(): CollapseStats {
  return {
    collapsedSpans: 0,
    collapsedMessages: 0,
    stagedSpans: 0,
    health: {
      totalSpawns: 0,
      totalErrors: 0,
      lastError: null,
      totalEmptySpawns: 0,
      emptySpawnWarningEmitted: false,
    },
  }
}

export function isContextCollapseEnabled(): boolean {
  return false
}

export async function applyCollapsesIfNeeded<T>(messages: T[], _toolUseContext?: unknown, _querySource?: unknown): Promise<{ messages: T[] }> {
  return { messages }
}

export function isWithheldPromptTooLong(_error: unknown, _check?: unknown, _querySource?: unknown): boolean {
  return false
}

export function recoverFromOverflow<T>(messages: T[], _querySource?: unknown): { committed: number; messages: T[] } {
  return { committed: 0, messages }
}

export function projectView(_options: unknown): unknown {
  return null
}

export function initContextCollapse(_options?: unknown): void {}
export function resetContextCollapse(): void {}
export function subscribe(_callback: unknown): () => void { return () => {} }
