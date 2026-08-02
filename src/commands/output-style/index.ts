import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

const outputStyle = {
  type: 'local-jsx',
  name: 'output-style',
  get description() {
    return t('cmd_output_style_desc')
  },
  isHidden: true,
  load: () => import('./output-style.js'),
} satisfies Command

export default outputStyle
