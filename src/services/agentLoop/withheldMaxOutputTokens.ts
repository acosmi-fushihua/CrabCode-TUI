import type { AssistantMessage } from '../../types/message.js'

/**
 * Is this a max_output_tokens error message? If so, the streaming loop should
 * withhold it from SDK callers until we know whether the recovery loop can
 * continue. Yielding early leaks an intermediate error to SDK callers (e.g.
 * cowork/desktop) that terminate the session on any `error` field — the
 * recovery loop keeps running but nobody is listening.
 *
 * 2026-07-17 audit RC-2: the producer (`queryModel` stop_reason==='max_tokens'
 * and the context-window path that reuses the same recovery) marks the message
 * via the SDK-facing `error` field. The historical predicate read only the
 * `apiError` sentinel — which has zero producers repo-wide — so the entire
 * withhold/recovery chain was dead and every max-tokens terminal leaked
 * straight to an interactive runtime as a turn-killing error. Both fields are accepted
 * here; producer ↔ predicate drift is pinned by
 * tests/unit/max-output-tokens-withheld-recovery.test.ts (§16 producer-import
 * bridge style).
 */
export function isWithheldMaxOutputTokens(
  msg: { type?: unknown; error?: unknown; apiError?: unknown } | undefined,
): msg is AssistantMessage {
  if (!msg || msg.type !== 'assistant') return false
  return (
    msg.error === 'max_output_tokens' || msg.apiError === 'max_output_tokens'
  )
}
