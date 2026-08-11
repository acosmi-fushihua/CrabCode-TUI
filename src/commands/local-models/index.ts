import type { Command } from '../../commands.js'

// English constant — this command intentionally does not add i18n locale
// entries (PR9B scope is the command entry point only).
const DESCRIPTION =
  'Check local model status, or install/remove curated local models'

const localModels = {
  type: 'local',
  name: 'local-models',
  description: DESCRIPTION,
  argumentHint:
    '[status|install <id>|add <path> [name]|remove <id>|remove-byo <id>|help]',
  supportsNonInteractive: true,
  load: () => import('./local-models.js'),
} satisfies Command

export default localModels
