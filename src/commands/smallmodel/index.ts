import type { Command } from '../../commands.js'
import { getSmallFastModel } from '../../utils/model/model.js'
import { t } from '../../i18n/index.js'

/**
 * The historical command was local-jsx only to run a React effect. Its direct
 * action is now renderer-neutral, but remains unavailable to generic
 * print/SDK callers; the direct TUI catalog projects the reviewed action.
 */
const smallModel = {
  type: 'local',
  name: 'smallmodel',
  get description() {
    const current = getSmallFastModel()
    return current
      ? t('cmd_smallmodel_desc_with_current', { model: current })
      : t('cmd_smallmodel_desc')
  },
  argumentHint: '[model]',
  isEnabled: () => true,
  supportsNonInteractive: false,
  load: () => import('./smallmodel.js'),
} satisfies Command

export default smallModel
