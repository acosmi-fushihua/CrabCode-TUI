import memoize from 'lodash-es/memoize.js'
import { isOfficialProvider } from './providers.js'

export type ModelCapabilityOverride =
  | 'effort'
  | 'max_effort'
  | 'thinking'
  | 'adaptive_thinking'
  | 'interleaved_thinking'

const TIERS = [
  {
    modelEnvVar: 'ACOSMI_DEFAULT_MAX_EFFORT_MODEL',
    capabilitiesEnvVar: 'ACOSMI_DEFAULT_MAX_EFFORT_MODEL_SUPPORTED_CAPABILITIES',
  },
  {
    modelEnvVar: 'ACOSMI_DEFAULT_MODEL',
    capabilitiesEnvVar: 'ACOSMI_DEFAULT_MODEL_SUPPORTED_CAPABILITIES',
  },
  {
    modelEnvVar: 'ACOSMI_DEFAULT_FAST_MODE_MODEL',
    capabilitiesEnvVar: 'ACOSMI_DEFAULT_FAST_MODE_MODEL_SUPPORTED_CAPABILITIES',
  },
] as const

/**
 * Check whether a 3p model capability override is set for a model that matches one of
 * the pinned ACOSMI_DEFAULT_*_MODEL env vars.
 */
export const get3PModelCapabilityOverride = memoize(
  (model: string, capability: ModelCapabilityOverride): boolean | undefined => {
    if (isOfficialProvider()) {
      return undefined
    }
    const m = model.toLowerCase()
    for (const tier of TIERS) {
      const pinned = process.env[tier.modelEnvVar]
      const capabilities = process.env[tier.capabilitiesEnvVar]
      if (!pinned || capabilities === undefined) continue
      if (m !== pinned.toLowerCase()) continue
      return capabilities
        .toLowerCase()
        .split(',')
        .map(s => s.trim())
        .includes(capability)
    }
    return undefined
  },
  (model, capability) => `${model.toLowerCase()}:${capability}`,
)
