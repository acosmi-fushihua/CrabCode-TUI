import type { Command } from '../../types/command.js'
import { t } from '../../i18n/index.js'
import { isEnvTruthy } from '../../utils/envUtils.js'

const logoutHeadless = {
  type: 'local',
  name: 'logout',
  get description() {
    return t('cmd_logout_desc')
  },
  isEnabled: () => !isEnvTruthy(process.env.DISABLE_LOGOUT_COMMAND),
  supportsNonInteractive: true,
  load: () => import('./headlessCall.js'),
} satisfies Command

export default logoutHeadless
