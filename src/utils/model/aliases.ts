/**
 * Client-side semantic aliases — kept after P4-Z3 ModelAlias deletion
 * (plan ext §15.8.2 K3 + K15 + K19).
 *
 * These are NOT family-name aliases — they are capability-driven semantic
 * aliases that resolve via SDK at parseUserSpecifiedModel time:
 *  - 'best'     → findModelByCapability('supports_max_effort')?.id
 *  - 'planmode' → max-effort in plan mode + default otherwise (compound)
 *
 * Legacy family-name aliases have been deleted. Old user input falls through
 * to SDK ModelNotFoundError → getCachedDefaultModelId() fallback (D7=a: no
 * settings migration).
 */
export const MODEL_ALIASES = ['best', 'planmode'] as const
export type ModelAlias = (typeof MODEL_ALIASES)[number]

export function isModelAlias(modelInput: string): modelInput is ModelAlias {
  return (MODEL_ALIASES as readonly string[]).includes(modelInput)
}

/**
 * Bare model family aliases — emptied in P4-Z3 (plan ext §15.8.2 K3).
 * Kept as an empty const for type/caller stability; family wildcard semantics
 * in availableModels allowlist are intentionally retired (caller must specify
 * a concrete SDK ManagedModel.id or one of MODEL_ALIASES).
 */
export const MODEL_FAMILY_ALIASES = [] as const

export function isModelFamilyAlias(_model: string): boolean {
  return false
}
