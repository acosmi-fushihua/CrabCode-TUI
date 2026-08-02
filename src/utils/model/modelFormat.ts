// Managed-model request-format compatibility helper.
//
// The managed runtime path
// (TS → `@acosmi/sdk-ts` → Acosmi gateway) is Anthropic-compatible. The SDK
// `ManagedModel` type carries `supported_formats?: string[]` and
// `preferred_format?: string` (values `"anthropic" | "openai"`), but the
// model-capabilities disk cache used to drop these fields, so OpenAI-format
// managed models (e.g. `qwen3.7`) leaked into the TUI picker even though
// the managed runtime cannot drive them.
//
// This module centralises the Anthropic-compatibility predicate so the worker
// `model/list` path (`getCachedModels`) and the TUI picker path
// (`getCachedEnabledModels`) agree on a single rule.
//
// Do not pattern-match model names
// strings. `'anthropic'` / `'openai'` are SDK-declared *format* enum values
// from the `ManagedModel.supported_formats` / `preferred_format` type
// contract — not a vendor-name blacklist — so referencing them is allowed.

/** SDK-declared format token meaning "Anthropic Messages API compatible". */
const ANTHROPIC_FORMAT = 'anthropic'

/**
 * Minimal structural view of a model capability for format compatibility.
 * Kept loose (no import of `ModelCapability`) so this stays a leaf module
 * with zero import cycle into `modelCapabilities.ts`.
 */
export interface FormatCompatInput {
  preferred_format?: string
  supported_formats?: string[]
}

/**
 * Returns whether a managed model can be driven through the Anthropic-
 * compatible managed runtime path.
 *
 * Rules (per Phase B plan §3, mirrors SDK 1.0.2 type-contract semantics):
 *   1. `preferred_format` is a non-empty string → exact match against
 *      `'anthropic'`. The gateway's explicit per-model preference wins.
 *   2. `preferred_format` empty, `supported_formats` non-empty → the model
 *      is compatible iff the array contains `'anthropic'`.
 *   3. Both empty (legacy upstream that has not declared formats) →
 *      conservatively returns `true`. Historically every managed model is
 *      Anthropic-routed through the gateway; SDK 1.0.2 does not populate
 *      these fields at runtime, so this branch is the zero-regression
 *      default and prevents the filter from emptying the picker.
 */
export function isAnthropicCompatibleManagedModel(
  cap: FormatCompatInput,
): boolean {
  const preferred = cap.preferred_format
  if (typeof preferred === 'string' && preferred.length > 0) {
    return preferred === ANTHROPIC_FORMAT
  }
  const supported = cap.supported_formats
  if (Array.isArray(supported) && supported.length > 0) {
    return supported.includes(ANTHROPIC_FORMAT)
  }
  // Legacy upstream: format metadata absent → conservative keep.
  return true
}
