import type { Command } from '../../types/command.js'
import { isOverageProvisioningAllowed } from '../../utils/auth.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import { t } from '../../i18n/index.js'

function isExtraUsageAllowed(): boolean {
  if (isEnvTruthy(process.env.DISABLE_EXTRA_USAGE_COMMAND)) return false
  return isOverageProvisioningAllowed()
}

/**
 * Direct-TUI registration of the historical renderer-neutral command body.
 * It returns text (including the fallback URL) instead of importing the
 * retired Ink dialog.
 */
const extraUsageCommand = {
  type: 'local',
  name: 'extra-usage',
  supportsNonInteractive: true,
  get description() {
    return t('cmd_extra_usage_desc')
  },
  isEnabled: isExtraUsageAllowed,
  load: () => import('./extra-usage-noninteractive.js'),
} satisfies Command

export default extraUsageCommand
