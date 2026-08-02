import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

const installSlackApp = {
  type: 'local',
  name: 'install-slack-app',
  get description() {
    return t('cmd_install_slack_app_desc')
  },
  availability: ['crabcode-ai'],
  supportsNonInteractive: false,
  load: () => import('./install-slack-app.js'),
} satisfies Command

export default installSlackApp
