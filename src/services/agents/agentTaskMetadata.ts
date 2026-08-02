import { AsyncLocalStorage } from 'async_hooks'

/** Ownership recorded before a background task enters the shared scheduler. */
export interface BackgroundAgentOwnerMetadata {
  taskId: string
  threadId: string
}

type BackgroundTaskOwnerContext = Readonly<{ threadId: string }>

const activeOwners = new Map<string, BackgroundAgentOwnerMetadata>()
const terminalOwners = new Map<string, string>()
const terminalOwnerLimit = 32
const ownerStorage = new AsyncLocalStorage<BackgroundTaskOwnerContext>()

/** Bind one complete turn to the session that owns its background tasks. */
export function runWithBackgroundTaskOwner<T>(
  threadId: string,
  fn: () => T,
): T {
  if (threadId.length === 0) {
    throw new Error('background task owner threadId must be non-empty')
  }
  return ownerStorage.run({ threadId }, fn)
}

/** Read the current turn's explicit background-task owner. */
export function getActiveBackgroundTaskOwner(): string | null {
  return ownerStorage.getStore()?.threadId ?? null
}

/** Fail closed when a background lifecycle has no provable owning turn. */
export function requireActiveBackgroundTaskOwner(operation: string): string {
  const threadId = getActiveBackgroundTaskOwner()
  if (!threadId) {
    throw new Error(`${operation} requires an explicit background task owner`)
  }
  return threadId
}

/** Record immutable owner metadata before scheduler admission. */
export function recordBackgroundAgentOwner(
  owner: BackgroundAgentOwnerMetadata,
): void {
  activeOwners.set(owner.taskId, owner)
}

/** Remove ownership for a foreground task that never entered the background. */
export function removeBackgroundAgentOwner(taskId: string): boolean {
  const activeRemoved = activeOwners.delete(taskId)
  const terminalRemoved = terminalOwners.delete(taskId)
  return activeRemoved || terminalRemoved
}

/** Return complete owner metadata or fail before scheduler admission. */
export function requireBackgroundAgentOwnerMetadata(
  taskId: string,
): BackgroundAgentOwnerMetadata {
  const owner = activeOwners.get(taskId)
  if (!owner) {
    throw new Error(`background task ${taskId} has no explicit owner metadata`)
  }
  return owner
}

/** Preserve the owner until the terminal task notification has been enqueued. */
export function recordBackgroundAgentTerminal(taskId: string): void {
  const owner = activeOwners.get(taskId)
  activeOwners.delete(taskId)
  if (!owner) return

  terminalOwners.delete(taskId)
  terminalOwners.set(taskId, owner.threadId)
  while (terminalOwners.size > terminalOwnerLimit) {
    const oldest = terminalOwners.keys().next().value
    if (oldest === undefined) break
    terminalOwners.delete(oldest)
  }
}

/** Resolve a task owner during either its active or terminal-notification phase. */
export function getBackgroundAgentOwnerThreadId(taskId: string): string | null {
  return activeOwners.get(taskId)?.threadId ?? terminalOwners.get(taskId) ?? null
}

/** Reset ownership state between isolated tests. */
export function _resetBackgroundAgentOwnersForTest(): void {
  activeOwners.clear()
  terminalOwners.clear()
}
