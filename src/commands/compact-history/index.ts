import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

const compactHistory = {
  type: 'local',
  name: 'compact-history',
  get description() {
    return t('cmd_compact_history_desc')
  },
  supportsNonInteractive: true,
  load: () => import('./compact-history.js'),
} satisfies Command

export default compactHistory
