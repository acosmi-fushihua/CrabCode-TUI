import { getIsNonInteractiveSession } from '../../bootstrap/state.js'
import type { Command } from '../../types/command.js'
import { t } from '../../i18n/index.js'

/**
 * Renderer-neutral `/context` command retained for print/SDK callers.
 *
 * The native TUI owns `/context` locally and sends the pre-existing
 * `get_context_usage` StructuredIO control directly. It therefore does not
 * register this adapter or import the removed Ink view.
 */
const contextCommand = {
  type: 'local',
  name: 'context',
  supportsNonInteractive: true,
  get description() {
    return t('cmd_context_desc')
  },
  get isHidden() {
    return !getIsNonInteractiveSession()
  },
  isEnabled() {
    return getIsNonInteractiveSession()
  },
  load: () => import('./context-noninteractive.js'),
} satisfies Command

export default contextCommand
