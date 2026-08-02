/**
 * Capability-based model lookup helpers.
 *
 * The single source of truth for model metadata is the SDK catalog
 * (`getCachedEnabledModels()` returning `ModelCapability[]`).
 *
 * Returns `null` when no matching model exists — callers must handle the
 * cold-start (pre-OAuth) state with a clear "no catalog" UX.
 */

import {
  getCachedEnabledModels,
  type ModelCapability,
} from './modelCapabilities.js'

export type CapabilityKey =
  | 'supports_fast_mode'
  | 'supports_max_effort'
  | 'supports_1m_context'
  | 'supports_thinking'
  | 'supports_web_search'
  | 'supports_prompt_cache'

/**
 * Find the first model matching a capability. Priority:
 *   1. `isDefault === true`
 *   2. `isEnabled !== false`
 *   3. First match
 *
 * Returns `null` when the catalog is empty or no model has the capability.
 */
export function findModelByCapability(cap: CapabilityKey): ModelCapability | null {
  const all = getCachedEnabledModels()
  const matches = all.filter(m => m.capabilities?.[cap] === true)
  if (matches.length === 0) return null
  return (
    matches.find(m => m.isDefault === true) ??
    matches.find(m => m.isEnabled !== false) ??
    matches[0] ??
    null
  )
}

/**
 * Pick any enabled model from the catalog. Used by template generators and
 * doc rendering when only "some valid model id" is needed, not a specific
 * capability. Returns `null` when the catalog is empty.
 */
export function pickAnyEnabledModel(): ModelCapability | null {
  const all = getCachedEnabledModels()
  return (
    all.find(m => m.isDefault === true) ??
    all.find(m => m.isEnabled !== false) ??
    all[0] ??
    null
  )
}
