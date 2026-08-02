import { getCachedDefaultModelId, getCachedEnabledModels } from '../model/modelCapabilities.js'

/**
 * When the user has never set teammateDefaultModel in /config, new teammates
 * use the SDK default model from upstream. Falls back to the first enabled
 * model if no default is marked.
 */
export function getHardcodedTeammateModelFallback(): string | null {
  // SDK dynamic: use upstream default model
  const sdkDefault = getCachedDefaultModelId()
  if (sdkDefault) return sdkDefault

  // Fallback: first enabled model from SDK cache
  const models = getCachedEnabledModels()
  if (models.length > 0) return models[0].id

  // No models available — return null so callers can degrade gracefully
  // instead of crashing (Config.tsx UI, spawnMultiAgent all call without try/catch)
  return null
}
