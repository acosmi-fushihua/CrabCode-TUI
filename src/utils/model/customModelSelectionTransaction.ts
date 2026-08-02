import { withCrossProcessResourceLockSync } from '../crossProcessResourceLock.js'
import {
  CUSTOM_MODEL_REFERENCE_PREFIX,
  parseCustomModelReference,
} from './customModelReference.js'

export interface ModelSelectionPersistenceDeps {
  readUserSettings: () => unknown
  resetUserSettingsCache: () => void
  writeUserModel: (model: string | null) => { error?: Error | null }
}

export type ModelSelectionPersistenceResult =
  | { ok: true }
  | { ok: false; reason: string }

function isCustomReferenceLike(model: string | null): boolean {
  if (model === null) return false
  return (
    parseCustomModelReference(model) !== null ||
    model.trim().startsWith(CUSTOM_MODEL_REFERENCE_PREFIX)
  )
}

export function requiresCustomModelSelectionLock(
  previousModel: string | null,
  nextModel: string | null,
): boolean {
  return (
    isCustomReferenceLike(previousModel) || isCustomReferenceLike(nextModel)
  )
}

function hasEnabledCustomEntry(settings: unknown, model: string): boolean {
  const entryId = parseCustomModelReference(model)
  if (entryId === null || !settings || typeof settings !== 'object') {
    return false
  }
  const record = settings as Record<string, unknown>
  const registry = record.customModels
  if (Array.isArray(registry) && registry.length > 0) {
    return registry.some(
      entry =>
        entry !== null &&
        typeof entry === 'object' &&
        (entry as Record<string, unknown>).id === entryId &&
        (entry as Record<string, unknown>).enabled !== false,
    )
  }

  // Legacy customModel rows are projected as custom:legacy:<alias>. Keep that
  // stable reference usable until migration materializes the flat registry.
  if (!entryId.startsWith('legacy:')) return false
  const alias = entryId.slice('legacy:'.length)
  if (!alias) return false
  const legacy = record.customModel
  if (!legacy || typeof legacy !== 'object') return false
  const models = (legacy as Record<string, unknown>).models
  return Boolean(
    models &&
      typeof models === 'object' &&
      Object.prototype.hasOwnProperty.call(models, alias),
  )
}

/**
 * Persist a TUI model selection without reopening a registry/delete race.
 * Custom transitions share the same synchronous cross-process lock used by
 * concurrent copy-on-write settings updates. The new custom reference is re-read and validated
 * only after that lock is held; a concurrent remove/disable therefore fails
 * closed instead of recreating a dangling persisted selection.
 */
export function persistModelSelectionTransaction(
  previousModel: string | null,
  nextModel: string | null,
  deps: ModelSelectionPersistenceDeps,
): ModelSelectionPersistenceResult {
  const persist = (): ModelSelectionPersistenceResult => {
    if (isCustomReferenceLike(nextModel)) {
      deps.resetUserSettingsCache()
      let settings: unknown
      try {
        settings = deps.readUserSettings()
      } catch (error) {
        return {
          ok: false,
          reason: `custom model registry read failed: ${
            error instanceof Error ? error.message : String(error)
          }`,
        }
      }
      if (nextModel === null || !hasEnabledCustomEntry(settings, nextModel)) {
        return {
          ok: false,
          reason: 'custom model selection is missing, malformed, or disabled',
        }
      }
    }

    const result = deps.writeUserModel(nextModel)
    if (result.error) {
      return { ok: false, reason: result.error.message }
    }
    return { ok: true }
  }

  if (!requiresCustomModelSelectionLock(previousModel, nextModel)) {
    return persist()
  }
  try {
    return withCrossProcessResourceLockSync('custom-model-config', persist)
  } catch (error) {
    return {
      ok: false,
      reason: `custom model selection lock unavailable: ${
        error instanceof Error ? error.message : String(error)
      }`,
    }
  }
}
