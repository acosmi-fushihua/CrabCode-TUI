// Stubs for cached microcompact feature (ANT-ONLY)
// These are feature-gated by feature('CACHED_MICROCOMPACT') in production.

export interface CacheEditsBlock {
  type: 'cache_edits'
  edits: unknown[]
}

export interface PinnedCacheEdits {
  userMessageIndex: number
  block: CacheEditsBlock
}

export interface CachedMCState {
  registeredTools: Set<string>
  toolOrder: string[]
  deletedRefs: Set<string>
  pinnedEdits: PinnedCacheEdits[]
}

export function isCachedMicrocompactEnabled(): boolean {
  return false
}

export function isModelSupportedForCacheEditing(_model: string): boolean {
  return false
}

export function getCachedMCConfig(): { supportedModels: string[]; triggerThreshold?: number; keepRecent?: number } {
  return { supportedModels: [], triggerThreshold: 10, keepRecent: 3 }
}

export function createCachedMCState(): CachedMCState {
  return {
    registeredTools: new Set(),
    toolOrder: [],
    deletedRefs: new Set(),
    pinnedEdits: [],
  }
}

export function markToolsSentToAPI(_state: CachedMCState): void {}

export function resetCachedMCState(state: CachedMCState): void {
  state.registeredTools.clear()
  state.toolOrder = []
  state.deletedRefs.clear()
  state.pinnedEdits = []
}

export function registerToolResult(_state: CachedMCState, _toolUseId: string): void {}

export function registerToolMessage(_state: CachedMCState, _groupIds: string[]): void {}

export function getToolResultsToDelete(_state: CachedMCState): string[] {
  return []
}

export function createCacheEditsBlock(_state: CachedMCState, _toolIds: string[]): CacheEditsBlock | null {
  return null
}
