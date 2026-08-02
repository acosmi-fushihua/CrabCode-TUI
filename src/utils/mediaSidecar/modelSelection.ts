import type { ModelCapability } from '../model/modelCapabilities.js'

type ImageModelCapability = ModelCapability & {
  inputModalities?: readonly string[]
}

export type MediaSidecarSelectionDiagnostic = {
  eligible: number
  totalEnabled: number
  forceModelIdSatisfied: boolean | null
  fallbackReason:
    | 'feature_off'
    | 'no_enabled_models'
    | 'no_image_capable'
    | 'no_chat_capable'
    | null
}

export type MediaSidecarSelection = {
  modelId: string | null
  provider: string | null
  diagnostic: MediaSidecarSelectionDiagnostic
}

export function selectMediaSidecarModel(
  enabledModels: readonly ModelCapability[],
  options: {
    forceModelId?: string | null
    featureEnabled?: boolean
  } = {},
): MediaSidecarSelection {
  const diagnostic: MediaSidecarSelectionDiagnostic = {
    eligible: 0,
    totalEnabled: enabledModels.length,
    forceModelIdSatisfied: options.forceModelId == null ? null : false,
    fallbackReason: null,
  }

  if (options.featureEnabled === false) {
    diagnostic.fallbackReason = 'feature_off'
    return { modelId: null, provider: null, diagnostic }
  }
  if (enabledModels.length === 0) {
    diagnostic.fallbackReason = 'no_enabled_models'
    return { modelId: null, provider: null, diagnostic }
  }

  const imageCapable = (enabledModels as readonly ImageModelCapability[]).filter(
    model => model.inputModalities?.includes('image'),
  )
  if (imageCapable.length === 0) {
    diagnostic.fallbackReason = 'no_image_capable'
    return { modelId: null, provider: null, diagnostic }
  }
  const candidates = imageCapable.filter(
    model => model.chatRuntimeSupported !== false,
  )
  if (candidates.length === 0) {
    diagnostic.fallbackReason = 'no_chat_capable'
    return { modelId: null, provider: null, diagnostic }
  }
  diagnostic.eligible = candidates.length

  let selected: ImageModelCapability | undefined
  if (options.forceModelId != null) {
    selected = candidates.find(model => model.id === options.forceModelId)
    diagnostic.forceModelIdSatisfied = selected !== undefined
  }
  selected ??= candidates.find(model => model.isDefault === true)
  selected ??= candidates[0]

  return {
    modelId: selected?.id ?? null,
    provider: selected?.provider?.trim() || null,
    diagnostic,
  }
}
