import { formatTotalCost } from '../../cost-tracker.js'
import { currentLimits } from '../../services/acosmiLimits.js'
import type { LocalCommandCall } from '../../types/command.js'
import { isAcosmiSubscriber } from '../../utils/auth.js'

export const call: LocalCommandCall = async () => {
  if (isAcosmiSubscriber()) {
    let value: string

    if (currentLimits.status === 'rejected') {
      value =
        'Your token quota is exhausted. Usage will resume when you purchase more tokens or your entitlements renew'
    } else if (currentLimits.status === 'allowed_warning') {
      value =
        'You are running low on token quota. Consider purchasing additional tokens'
    } else {
      value =
        'You are currently using your subscription to power your CrabCode usage'
    }

    if (process.env.USER_TYPE === 'ant') {
      value += `\n\n[ANT-ONLY] Showing cost anyway:\n ${formatTotalCost()}`
    }
    return { type: 'text', value }
  }
  return { type: 'text', value: formatTotalCost() }
}
