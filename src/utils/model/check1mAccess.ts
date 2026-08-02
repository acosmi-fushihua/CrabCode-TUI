import { is1mContextDisabled } from '../context.js'
import { getCachedDefaultModelId, getCachedEnabledModels, getCachedModelCapabilities } from './modelCapabilities.js'
import { findModelByCapability } from './findModelByCapability.js'
import { getAPIProvider } from './providers.js'

/**
 * Check whether a specific model id (looked up via SDK capability) supports
 * 1M context. Returns false when SDK cache is empty or the capability flag is
 * absent, conservatively gating 1M-only UI paths.
 */
function modelIdSupports1m(modelId: string | null | undefined): boolean {
  if (!modelId) return false
  return getCachedModelCapabilities(modelId)?.supports_1m_context === true
}

/**
 * 1M access for the SDK max-effort model (replaces legacy max-effort 1M gate).
 */
export function checkMaxEffort1mAccess(): boolean {
  if (is1mContextDisabled()) {
    return false
  }
  if (getAPIProvider() === 'acosmi') {
    return modelIdSupports1m(findModelByCapability('supports_max_effort')?.id)
  }
  return getCachedEnabledModels().some(
    m =>
      m.capabilities?.supports_max_effort === true &&
      m.capabilities?.supports_1m_context === true,
  )
}

/**
 * 1M access for the SDK default-tier model (replaces legacy default-tier 1M gate).
 */
export function checkDefault1mAccess(): boolean {
  if (is1mContextDisabled()) {
    return false
  }
  if (getAPIProvider() === 'acosmi') {
    return modelIdSupports1m(getCachedDefaultModelId())
  }
  return getCachedEnabledModels().some(
    m => m.capabilities?.supports_1m_context === true,
  )
}
