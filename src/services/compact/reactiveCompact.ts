// Stubs for reactive compact feature (ANT-ONLY)
export function isReactiveOnlyMode(): boolean {
  return false
}

type CompactOutcome =
  | { ok: true; result: import('./compact.js').CompactionResult }
  | { ok: false; reason: 'too_few_groups' | 'aborted' | 'exhausted' | 'error' | 'media_unstrippable' }

export async function reactiveCompactOnPromptTooLong(_messages: unknown, _params?: unknown, _opts?: unknown): Promise<CompactOutcome> {
  return { ok: false, reason: 'error' }
}

export function isReactiveCompactEnabled(): boolean {
  return false
}

export function isWithheldPromptTooLong(_error: unknown): boolean {
  return false
}

export function isWithheldMediaSizeError(_error: unknown): boolean {
  return false
}

export async function tryReactiveCompact(_options: unknown): Promise<import('./compact.js').CompactionResult | null> {
  return null
}
