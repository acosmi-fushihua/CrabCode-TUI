/**
 * Single source of truth for custom-model endpoint construction.
 *
 * Both the runtime chat adapter and the connectivity probe must build
 * their URL through this function — a divergence between the two produced the
 * "connectivity test passes, chat 404s" class of bug (2026-07-01 audit,
 * finding 1): the probe dialed `${base}/v1/messages` while the runtime dialed
 * `${base}/messages`, and every anthropic-compatible catalog preset ships a
 * baseUrl WITHOUT the `/v1` segment.
 *
 * Conventions (aligned with the provider catalog + official SDK behavior):
 * - `anthropic-compatible`: baseUrl excludes the version segment
 *   (`ANTHROPIC_BASE_URL` convention); requests POST to `{base}/v1/messages`.
 * - `openai-compatible`: baseUrl includes its version segment
 *   (`OPENAI_BASE_URL` convention, e.g. `.../v1`); requests POST to
 *   `{base}/chat/completions`.
 *
 * Hand-filled values are tolerated: a trailing slash, an already-complete
 * endpoint path, or (anthropic) a baseUrl that already ends with `/v1` all
 * resolve to the same canonical URL instead of doubling path segments.
 */

export type CustomModelEndpointProtocol =
  | 'anthropic-compatible'
  | 'openai-compatible'

export function resolveCustomModelEndpoint(
  protocol: CustomModelEndpointProtocol,
  baseUrl: string,
): string {
  const base = baseUrl.trim().replace(/\/+$/, '')
  if (protocol === 'openai-compatible') {
    if (base.endsWith('/chat/completions')) return base
    return `${base}/chat/completions`
  }
  // anthropic-compatible: `/messages` also covers a pasted full
  // `/v1/messages` endpoint; a bare `/v1` suffix only needs `/messages`.
  if (base.endsWith('/messages')) return base
  if (base.endsWith('/v1')) return `${base}/messages`
  return `${base}/v1/messages`
}
