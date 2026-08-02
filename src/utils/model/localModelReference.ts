/**
 * Local-model reference syntax for the TUI `/model` command.
 *
 * `/model local:<modelId>` selects a locally-hosted model. The `local:` prefix
 * keeps local runtimes in a namespace of their own so they never collide with
 * managed catalog ids, predefined alias names, or custom bring-your-own model
 * strings; only this exact prefix routes through the TUI local-model catalog
 * and inference runtime.
 *
 * Pure functions only, so every TUI model-selection path can reuse the same
 * parse/render pair.
 */

/** Prefix that marks a `/model` argument as a local-model reference. */
export const LOCAL_MODEL_REFERENCE_PREFIX = 'local:'

/**
 * Parse a `/model` argument into a bare local-model id.
 *
 * Returns the trimmed model id when `input` is a well-formed
 * `local:<modelId>` reference; returns `null` for anything else: a plain
 * model name, an alias, an empty string, or a bare `local:` with no id.
 *
 * Surrounding whitespace and whitespace right after the prefix are trimmed so
 * `  local: qwen-7b  ` and `local:qwen-7b` resolve to the same id. The prefix
 * match is case-sensitive: `Local:` / `LOCAL:` fall through to `null` and let
 * the normal alias / catalog / validate path report the error.
 */
export function parseLocalModelReference(
  input: string | null | undefined,
): string | null {
  if (typeof input !== 'string') return null
  const trimmed = input.trim()
  if (!trimmed.startsWith(LOCAL_MODEL_REFERENCE_PREFIX)) return null
  const id = trimmed.slice(LOCAL_MODEL_REFERENCE_PREFIX.length).trim()
  return id.length > 0 ? id : null
}

/** True when `input` is a well-formed `local:<modelId>` reference. */
export function isLocalModelReference(
  input: string | null | undefined,
): boolean {
  return parseLocalModelReference(input) !== null
}

/**
 * Render a bare local-model id back into its canonical `local:<modelId>`
 * reference form. Round-trips with {@link parseLocalModelReference}.
 */
export function renderLocalModelReference(modelId: string): string {
  return `${LOCAL_MODEL_REFERENCE_PREFIX}${modelId.trim()}`
}

/**
 * Build the picker label / description for a `local:<id>` reference surfaced
 * in the `/model` option list.
 *
 * A local-model runtime is owned by the TUI local-model authority, not the
 * managed catalog or a bring-your-own custom model string, so a `local:<id>`
 * reference must never be rendered as a generic "Custom model". The
 * synchronous `getModelOptions()` cannot read live inference-server status;
 * the description points the user at `/local-models` for management.
 *
 * Pure function: takes a bare model id (as returned by
 * {@link parseLocalModelReference}) and returns label/description text only.
 */
export function describeLocalModelReference(modelId: string): {
  label: string
  description: string
} {
  const id = modelId.trim()
  return {
    label: `Local model: ${id}`,
    description:
      'TUI-managed local model - runtime status unavailable here - ' +
      'use /local-models to manage',
  }
}
