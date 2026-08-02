import type { Command } from '../commands.js'
import { t } from '../i18n/index.js'
import { getAttributionTexts } from '../utils/attribution.js'
import { executeShellCommandsInPrompt } from '../utils/promptShellExecution.js'
import { getUndercoverInstructions, isUndercover } from '../utils/undercover.js'
import {
  buildGitWorkflowSpec,
  getGitWorkflowAllowedTools,
  type GitWorkflowPolicy,
} from './git-workflow.js'

const ALLOWED_TOOLS = getGitWorkflowAllowedTools('commit', {
  surface: 'ant',
  undercover: false,
})

function getLegacyTuiPolicy(): GitWorkflowPolicy {
  const undercover = process.env.USER_TYPE === 'ant' && isUndercover()
  return {
    surface: 'ant',
    undercover,
    undercoverInstructions: undercover
      ? getUndercoverInstructions()
      : undefined,
  }
}

type CommitCommandDependencies = {
  getAttributionTexts: typeof getAttributionTexts
  executeShellCommandsInPrompt: typeof executeShellCommandsInPrompt
}

export function createCommitCommand(
  dependencies: CommitCommandDependencies = {
    // Keep imported bindings lazy: this module participates in the command /
    // attribution graph, so eagerly reading them while the graph is still in
    // its ESM TDZ can break worker control-runtime startup.
    getAttributionTexts: () => getAttributionTexts(),
    executeShellCommandsInPrompt: (...args) =>
      executeShellCommandsInPrompt(...args),
  },
) {
  return {
    type: 'prompt',
    name: 'commit',
    get description() {
      return t('cmd_commit_desc')
    },
    allowedTools: ALLOWED_TOOLS,
    contentLength: 0, // Dynamic content
    get progressMessage() {
      return t('cmd_commit_progress')
    },
    source: 'builtin',
    async getPromptForCommand(_args, context) {
      const { commit: commitAttribution } =
        dependencies.getAttributionTexts()
      const spec = buildGitWorkflowSpec({
        kind: 'commit',
        policy: getLegacyTuiPolicy(),
        progressMessage: t('cmd_commit_progress'),
        commitAttribution,
      })
      const finalContent = await dependencies.executeShellCommandsInPrompt(
        spec.userInput,
        {
          ...context,
          getAppState() {
            const appState = context.getAppState()
            return {
              ...appState,
              toolPermissionContext: {
                ...appState.toolPermissionContext,
                alwaysAllowRules: {
                  ...appState.toolPermissionContext.alwaysAllowRules,
                  command: ALLOWED_TOOLS,
                },
              },
            }
          },
        },
        '/commit',
      )

      return [{ type: 'text', text: finalContent }]
    },
  } satisfies Command
}

const command = createCommitCommand()

export default command
