// Phase -1 stub: 待后续实现
// /proactive slash command — toggles proactive (autonomous) mode.
import { t } from '../i18n/index.js'

import type { Command } from '../types/command.js'

const proactiveCommand: Command = {
  type: 'local',
  name: 'proactive',
  get description() {
    return t('cmd_proactive_desc')
  },
  supportsNonInteractive: false,
  async load() {
    return {
      async call(_args) {
        return {
          type: 'text' as const,
          value: 'Proactive mode is not yet implemented (Phase -1 stub).',
        }
      },
    }
  },
}

export default proactiveCommand
