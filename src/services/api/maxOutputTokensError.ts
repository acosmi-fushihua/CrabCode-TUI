import { API_ERROR_MESSAGE_PREFIX } from './errors.js'

/**
 * Below this fraction of the requested max_tokens, a stop_reason==='max_tokens'
 * terminal is attributed to an upstream-imposed cut rather than the requested
 * budget being genuinely exhausted.
 *
 * 2026-07-17 audit RC-1: DashScope capped glm-5.2 at min(max_tokens, 2000)+1
 * while the client had requested 64000. The old copy quoted the client's own
 * 64000 as the cause and told users to set CRABCODE_MAX_OUTPUT_TOKENS — a
 * misattribution (the real cap was never in the client's hands).
 */
export const UPSTREAM_TRUNCATION_RATIO = 0.5

export function isUpstreamTruncatedOutput(
  maxOutputTokens: number,
  outputTokens: number | null | undefined,
): boolean {
  return (
    typeof outputTokens === 'number' &&
    Number.isFinite(outputTokens) &&
    outputTokens > 0 &&
    maxOutputTokens > 0 &&
    outputTokens < maxOutputTokens * UPSTREAM_TRUNCATION_RATIO
  )
}

/**
 * Build the user-visible max-tokens error copy from the real terminal usage.
 * The genuine-exhaustion branch keeps the legacy wording verbatim (external
 * docs/tests reference it); only the upstream-cut branch changes the story —
 * and stops recommending an environment variable that cannot help.
 */
export function formatMaxOutputTokensError(
  maxOutputTokens: number,
  outputTokens: number | null | undefined,
): { content: string; upstreamTruncated: boolean } {
  if (isUpstreamTruncatedOutput(maxOutputTokens, outputTokens)) {
    return {
      upstreamTruncated: true,
      content:
        `${API_ERROR_MESSAGE_PREFIX}: The upstream model provider cut this response at ` +
        `${outputTokens} output tokens — far below the ${maxOutputTokens} maximum CrabCode requested. ` +
        `This limit is imposed upstream; changing CRABCODE_MAX_OUTPUT_TOKENS will not raise it.`,
    }
  }
  return {
    upstreamTruncated: false,
    content: `${API_ERROR_MESSAGE_PREFIX}: CrabCode's response exceeded the ${maxOutputTokens} output token maximum. To configure this behavior, set the CRABCODE_MAX_OUTPUT_TOKENS environment variable.`,
  }
}
