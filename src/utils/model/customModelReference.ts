/**
 * Custom-model reference syntax for `mainLoopModel` / the `/model` command.
 *
 * The `custom:` prefix keeps bring-your-own custom model entries in a
 * namespace of their own so the routing key never collides with managed
 * catalog slugs, predefined alias names, or the free-form `modelId` string
 * the user sends to their own third-party endpoint (which is allowed to
 * equal any gateway slug — see W-CUSTOM-MODEL-NAMESPACE-ISOLATION
 * 2026-07-01). The bare id inside the reference is the registry entry's
 * stable `entry.id` (a `randomUUID()`), never the wire-level `modelId`.
 *
 * Mirrors `localModelReference.ts` exactly. Pure functions only: no renderer,
 * App Server or worker dependencies, so the TUI `/model` command and any
 * future caller can reuse the same parse/render pair.
 */

/** Prefix that marks a `/model` argument / `mainLoopModel` value as a custom-model reference. */
export const CUSTOM_MODEL_REFERENCE_PREFIX = 'custom:'

/**
 * Parse a reference into a bare `entry.id`.
 *
 * Returns the trimmed id when `input` is a well-formed `custom:<entryId>`
 * reference; returns `null` for anything else: a plain model name, an alias,
 * an empty string, or a bare `custom:` with no id.
 *
 * Surrounding whitespace and whitespace right after the prefix are trimmed so
 * `  custom: abc-123  ` and `custom:abc-123` resolve to the same id. The
 * prefix match is case-sensitive: `Custom:` / `CUSTOM:` fall through to
 * `null` and let the normal alias / catalog / legacy-modelId path report it.
 */
export function parseCustomModelReference(
  input: string | null | undefined,
): string | null {
  if (typeof input !== 'string') return null
  const trimmed = input.trim()
  if (!trimmed.startsWith(CUSTOM_MODEL_REFERENCE_PREFIX)) return null
  const id = trimmed.slice(CUSTOM_MODEL_REFERENCE_PREFIX.length).trim()
  return id.length > 0 ? id : null
}

/** True when `input` is a well-formed `custom:<entryId>` reference. */
export function isCustomModelReferenceId(
  input: string | null | undefined,
): boolean {
  return parseCustomModelReference(input) !== null
}

/**
 * Render a registry entry's stable `id` back into its canonical
 * `custom:<entryId>` reference form. Round-trips with
 * {@link parseCustomModelReference}.
 */
export function renderCustomModelReference(entryId: string): string {
  return `${CUSTOM_MODEL_REFERENCE_PREFIX}${entryId.trim()}`
}
