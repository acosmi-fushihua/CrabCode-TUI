import { getSettings_DEPRECATED } from '../settings/settings.js'
import { isModelAlias } from './aliases.js'
import { parseUserSpecifiedModel, resolveOverriddenModel } from './model.js'

/**
 * Check if a model name starts with a prefix at a segment boundary.
 * The prefix must match up to the end of the name or a "-" separator.
 */
function prefixMatchesModel(modelName: string, prefix: string): boolean {
  if (!modelName.startsWith(prefix)) {
    return false
  }
  return modelName.length === prefix.length || modelName[prefix.length] === '-'
}

/**
 * Check if a model matches a version-prefix entry in the allowlist.
 * Resolves input aliases before matching.
 */
function modelMatchesVersionPrefix(model: string, entry: string): boolean {
  const resolvedModel = isModelAlias(model)
    ? parseUserSpecifiedModel(model).toLowerCase()
    : model
  return prefixMatchesModel(resolvedModel, entry)
}

/**
 * Check if a model is allowed by the availableModels allowlist in settings.
 * If availableModels is not set, all models are allowed.
 *
 * Matching tiers (post P4-Z3 — family wildcard semantics retired):
 * 1. Direct match (alias-to-alias or full-id-to-full-id)
 * 2. Alias resolves to listed full id (or vice versa)
 * 3. Version prefix — listed prefix matches input full id at segment boundary
 */
export function isModelAllowed(model: string): boolean {
  const settings = getSettings_DEPRECATED() || {}
  const { availableModels } = settings
  if (!availableModels) {
    return true // No restrictions
  }
  if (availableModels.length === 0) {
    return false // Empty allowlist blocks all user-specified models
  }

  const resolvedModel = resolveOverriddenModel(model)
  const normalizedModel = resolvedModel.trim().toLowerCase()
  const normalizedAllowlist = availableModels.map(m => m.trim().toLowerCase())

  // Direct match (alias-to-alias or full-name-to-full-name)
  if (normalizedAllowlist.includes(normalizedModel)) {
    return true
  }

  // If model is an alias, resolve it and check if the resolved name is in the list
  if (isModelAlias(normalizedModel)) {
    const resolved = parseUserSpecifiedModel(normalizedModel).toLowerCase()
    if (normalizedAllowlist.includes(resolved)) {
      return true
    }
  }

  // If any alias in the allowlist resolves to the input model
  for (const entry of normalizedAllowlist) {
    if (isModelAlias(entry)) {
      const resolved = parseUserSpecifiedModel(entry).toLowerCase()
      if (resolved === normalizedModel) {
        return true
      }
    }
  }

  // Version-prefix matching at segment boundary
  for (const entry of normalizedAllowlist) {
    if (!isModelAlias(entry)) {
      if (modelMatchesVersionPrefix(normalizedModel, entry)) {
        return true
      }
    }
  }

  return false
}
