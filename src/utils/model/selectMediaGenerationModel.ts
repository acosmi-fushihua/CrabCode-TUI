/**
 * Media-generation model selectors.
 *
 * Picks a catalog model that advertises image- or video-generation capability
 * so the `MediaGenerationTool` can call `client.generateImage()` /
 * `client.generateVideo()` against the right managed model. The selection
 * chain mirrors `selectVisualSidecar` (sidecar.ts) but is simpler — a single
 * boolean capability filter plus default→enabled→first priority:
 *
 *   1. Caller-supplied override (`forceModelId`) — must itself be capable;
 *      otherwise the override is ignored and we fall through to the chain.
 *   2. Catalog default (`isDefault === true`) when it is also capability-
 *      capable.
 *   3. First capable model by stable catalog order.
 *   4. None capable → `modelId: null` with a diagnostic `reason`.
 *
 * **No hardcoded model ids**: the filter uses only
 * SDK catalog capability flag (`supports_image_generation` /
 * `supports_video_generation`), surfaced by `@acosmi/sdk-ts` ≥ 2.2.1. When the
 * catalog advertises no capable model every candidate fails the filter and the
 * selector returns `null` cleanly (the tool then reports "no model available").
 *
 * Pure functions: the caller passes the endpoint-specific catalog projection
 * from `getCachedImageGenerationModels()` / `getCachedVideoGenerationModels()`.
 * Those accessors read the normalized cache before the Anthropic-chat format
 * filter, so OpenAI-only generation rows remain eligible. DI-testable like
 * `selectVisualSidecar`.
 */

import type { ModelCapability } from './modelCapabilities.js'

export type MediaGenerationSelectionReason =
  | 'ok'
  | 'no_models'
  | 'none_capable'
  | 'forced_not_capable'

export type MediaGenerationSelection = {
  modelId: string | null
  reason: MediaGenerationSelectionReason
}

type GenerationCapability =
  | 'supports_image_generation'
  | 'supports_video_generation'

type ModelWithGenerationCaps = ModelCapability & {
  capabilities?: ModelCapability['capabilities'] & {
    supports_image_generation?: boolean
    supports_video_generation?: boolean
  }
}

/**
 * Shared pure selection chain. `capabilityKey` chooses image vs video.
 *
 * The model list MUST already be the matching image/video projection. We keep
 * an additional `isEnabled !== false` check as defense in depth for injected
 * callers and pure-function tests.
 */
function selectGenerationModel(
  models: readonly ModelCapability[],
  capabilityKey: GenerationCapability,
  options: { forceModelId?: string | null } = {},
): MediaGenerationSelection {
  if (models.length === 0) {
    return { modelId: null, reason: 'no_models' }
  }

  const enabled = models.filter(
    m => m.isEnabled !== false,
  ) as readonly ModelWithGenerationCaps[]

  const capable = enabled.filter(m => m.capabilities?.[capabilityKey] === true)

  if (capable.length === 0) {
    return { modelId: null, reason: 'none_capable' }
  }

  // 1. Honor user override only when the forced model is itself capable.
  if (options.forceModelId != null) {
    const found = capable.find(m => m.id === options.forceModelId)
    if (found) {
      return { modelId: found.id, reason: 'ok' }
    }
    // Forced model is not capable → explicit diagnostic; do NOT silently
    // fall back, so the caller can surface why the override was rejected.
    return { modelId: null, reason: 'forced_not_capable' }
  }

  // 2. Catalog default if capable.
  const def = capable.find(m => m.isDefault === true)
  if (def) {
    return { modelId: def.id, reason: 'ok' }
  }

  // 3. First capable model by stable catalog order.
  return { modelId: capable[0]!.id, reason: 'ok' }
}

/**
 * Select a catalog model capable of image generation
 * (`capabilities.supports_image_generation === true`).
 */
export function selectImageGenerationModel(
  models: readonly ModelCapability[],
  options: { forceModelId?: string | null } = {},
): MediaGenerationSelection {
  return selectGenerationModel(models, 'supports_image_generation', options)
}

/**
 * Select a catalog model capable of video generation
 * (`capabilities.supports_video_generation === true`).
 */
export function selectVideoGenerationModel(
  models: readonly ModelCapability[],
  options: { forceModelId?: string | null } = {},
): MediaGenerationSelection {
  return selectGenerationModel(models, 'supports_video_generation', options)
}
