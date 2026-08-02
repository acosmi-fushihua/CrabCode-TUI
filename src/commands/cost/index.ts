/**
 * Cost command - minimal metadata only.
 * Implementation is lazy-loaded from cost.ts to reduce startup time.
 */
import type { Command } from '../../commands.js'
import { isAcosmiSubscriber } from '../../utils/auth.js'
import { t } from '../../i18n/index.js'

const cost = {
  type: 'local',
  name: 'cost',
  get description() {
    return t('cmd_cost_desc')
  },
  get isHidden() {
    // Keep visible for Ants even if they're subscribers (they see cost breakdowns)
    if (process.env.USER_TYPE === 'ant') {
      return false
    }
    return isAcosmiSubscriber()
  },
  supportsNonInteractive: true,
  load: () => import('./cost.js'),
} satisfies Command

export default cost
