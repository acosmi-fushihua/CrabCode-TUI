import { checkMaxEffort1mAccess, checkDefault1mAccess } from './check1mAccess.js'
import { getUserSpecifiedModelSetting } from './model.js'
import { getCachedDefaultModelId, getCachedModelDisplayName } from './modelCapabilities.js'
import { findModelByCapability } from './findModelByCapability.js'

/**
 * Get available model upgrade for more context.
 * Returns null if no upgrade available or user already has max context.
 *
 * Resolves the user-pinned model setting against SDK capabilities and
 * surfaces the matching 1M-context variant when one exists.
 */
function getAvailableUpgrade(): {
  alias: string
  name: string
  multiplier: number
} | null {
  const currentModelSetting = getUserSpecifiedModelSetting()
  if (typeof currentModelSetting !== 'string') return null
  const maxEffortId = findModelByCapability('supports_max_effort')?.id
  const defaultId = getCachedDefaultModelId()
  if (maxEffortId && currentModelSetting === maxEffortId && checkMaxEffort1mAccess()) {
    const name = getCachedModelDisplayName(maxEffortId)
    return {
      alias: `${maxEffortId}[1m]`,
      name: `${name} 1M`,
      multiplier: 5,
    }
  }
  if (defaultId && currentModelSetting === defaultId && checkDefault1mAccess()) {
    const name = getCachedModelDisplayName(defaultId)
    return {
      alias: `${defaultId}[1m]`,
      name: `${name} 1M`,
      multiplier: 5,
    }
  }
  return null
}

/**
 * Get upgrade message for different contexts
 */
export function getUpgradeMessage(context: 'warning' | 'tip'): string | null {
  const upgrade = getAvailableUpgrade()
  if (!upgrade) return null

  switch (context) {
    case 'warning':
      return `/model ${upgrade.alias}`
    case 'tip':
      return `Tip: You have access to ${upgrade.name} with ${upgrade.multiplier}x more context`
    default:
      return null
  }
}
