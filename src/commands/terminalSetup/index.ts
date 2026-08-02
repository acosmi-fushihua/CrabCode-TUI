import type { Command } from '../../commands.js'
import { env } from '../../utils/env.js'
import { t } from '../../i18n/index.js'

// Terminals that natively support CSI u / Kitty keyboard protocol
const NATIVE_CSIU_TERMINALS: Record<string, string> = {
  ghostty: 'Ghostty',
  kitty: 'Kitty',
  'iTerm.app': 'iTerm2',
  WezTerm: 'WezTerm',
}

const terminalSetup = {
  type: 'local-jsx',
  name: 'terminal-setup',
  get description() {
    return env.terminal === 'Apple_Terminal'
      ? t('cmd_terminal_setup_desc_apple')
      : t('cmd_terminal_setup_desc_default')
  },
  isHidden: env.terminal !== null && env.terminal in NATIVE_CSIU_TERMINALS,
  load: () => import('./terminalSetup.js'),
} satisfies Command

export default terminalSetup
