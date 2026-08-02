import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

const heapDump = {
  type: 'local',
  name: 'heapdump',
  get description() {
    return t('cmd_heapdump_desc')
  },
  isHidden: true,
  supportsNonInteractive: true,
  load: () => import('./heapdump.js'),
} satisfies Command

export default heapDump
