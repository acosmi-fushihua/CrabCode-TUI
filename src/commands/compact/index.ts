import type { Command } from '../../commands.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import { t } from '../../i18n/index.js'

const compact = {
  type: 'local',
  name: 'compact',
  // Explicitly support the short form used by the native command surface.
  // Prefix completion remains presentation-only; the alias makes `/com`
  // resolve identically even when palette completion is disabled or bypassed.
  aliases: ['com'],
  get description() {
    return t('cmd_compact_desc')
  },
  isEnabled: () => !isEnvTruthy(process.env.DISABLE_COMPACT),
  supportsNonInteractive: true,
  argumentHint: '<optional custom summarization instructions>',
  load: () => import('./compact.js'),
} satisfies Command

export default compact
