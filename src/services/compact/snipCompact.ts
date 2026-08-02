// Stubs for snip compact feature (ANT-ONLY)
export const SNIP_NUDGE_TEXT = ''

export function isSnipRuntimeEnabled(): boolean {
  return false
}

export function shouldNudgeForSnips(_messages?: unknown): boolean {
  return false
}

export function isSnipMarkerMessage(_msg: unknown): boolean {
  return false
}

export function snipCompactIfNeeded<T>(messages: T[], _opts?: { force?: boolean }): { messages: T[]; tokensFreed: number; boundaryMessage?: unknown; executed: boolean } {
  return { messages, tokensFreed: 0, executed: false }
}
