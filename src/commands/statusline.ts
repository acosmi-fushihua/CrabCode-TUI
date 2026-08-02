import type { ContentBlockParam } from '../types/api-types.js'
import type { Command } from '../types/command.js'
import { AGENT_TOOL_NAME } from '../tools/AgentTool/constants.js'
import { t } from '../i18n/index.js'

function createStatuslineCommand(
  disableNonInteractive: boolean,
): Command {
  return {
    type: 'prompt',
    get description() {
      return t('cmd_statusline_desc')
    },
    contentLength: 0,
    aliases: [],
    name: 'statusline',
    get progressMessage() {
      return t('cmd_statusline_progress')
    },
    allowedTools: [
      AGENT_TOOL_NAME,
      'Read(~/**)',
      'Edit(~/.crabcode/settings.json)',
    ],
    source: 'builtin',
    ...(disableNonInteractive ? { disableNonInteractive: true } : {}),
    async getPromptForCommand(
      args: string,
    ): Promise<ContentBlockParam[]> {
      const prompt =
        args.trim() ||
        'Configure my statusLine from my shell PS1 configuration'
      return [
        {
          type: 'text',
          text: `Create an ${AGENT_TOOL_NAME} with subagent_type "statusline-setup" and the prompt "${prompt}"`,
        },
      ]
    },
  }
}

/**
 * Fixed-source metadata for real print/SDK non-interactive sessions.
 */
const statusline = createStatuslineCommand(true)

/**
 * Renderer-neutral prompt body projected only into the interactive direct TUI.
 * Keeping this as a separate object avoids widening the print/SDK command
 * surface merely because the Rust renderer talks to its host over StructuredIO.
 */
export const directTuiStatusline = createStatuslineCommand(false)

export default statusline
