import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

const releaseNotes: Command = {
  get description() {
    return t('cmd_release_notes_desc')
  },
  name: 'release-notes',
  type: 'local',
  supportsNonInteractive: true,
  load: () => import('./release-notes.js'),
}

export default releaseNotes
