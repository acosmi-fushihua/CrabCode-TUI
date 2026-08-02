import type { Command } from '../commands.js'
import { t } from '../i18n/index.js'
import {
  getAttributionTexts,
  getEnhancedPRAttribution,
} from '../utils/attribution.js'
import { getDefaultBranch } from '../utils/git.js'
import { executeShellCommandsInPrompt } from '../utils/promptShellExecution.js'
import { getUndercoverInstructions, isUndercover } from '../utils/undercover.js'
import {
  buildGitWorkflowSpec,
  getGitWorkflowAllowedTools,
  type GitWorkflowPolicy,
} from './git-workflow.js'

const ALLOWED_TOOLS = getGitWorkflowAllowedTools('commitPushPr', {
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
    safeUser: process.env.SAFEUSER || '',
    username: process.env.USER || '',
  }
}

function buildLegacyTuiSpec(
  defaultBranch: string,
  prAttribution?: string,
  instructions?: string,
  attributionTexts = getAttributionTexts(),
) {
  return buildGitWorkflowSpec({
    kind: 'commitPushPr',
    policy: getLegacyTuiPolicy(),
    progressMessage: t('cmd_commit_push_pr_progress'),
    defaultBranch,
    instructions,
    commitAttribution: attributionTexts.commit,
    prAttribution: prAttribution ?? attributionTexts.pr,
  })
}

type CommitPushPrCommandDependencies = {
  getAttributionTexts: typeof getAttributionTexts
  getEnhancedPRAttribution: typeof getEnhancedPRAttribution
  getDefaultBranch: typeof getDefaultBranch
  executeShellCommandsInPrompt: typeof executeShellCommandsInPrompt
}

export function createCommitPushPrCommand(
  dependencies: CommitPushPrCommandDependencies = {
    // Lazily dereference imports. Eager dependency snapshots here create an
    // ESM TDZ when the long-lived worker loads attribution -> command modules.
    getAttributionTexts: () => getAttributionTexts(),
    getEnhancedPRAttribution: (...args) =>
      getEnhancedPRAttribution(...args),
    getDefaultBranch: () => getDefaultBranch(),
    executeShellCommandsInPrompt: (...args) =>
      executeShellCommandsInPrompt(...args),
  },
) {
  return {
    type: 'prompt',
    name: 'commit-push-pr',
    get description() {
      return t('cmd_commit_push_pr_desc')
    },
    allowedTools: ALLOWED_TOOLS,
    get contentLength() {
      // Use 'main' as estimate for content length calculation.
      return buildLegacyTuiSpec(
        'main',
        undefined,
        undefined,
        dependencies.getAttributionTexts(),
      ).userInput.length
    },
    get progressMessage() {
      return t('cmd_commit_push_pr_progress')
    },
    source: 'builtin',
    async getPromptForCommand(args, context) {
      const [defaultBranch, prAttribution] = await Promise.all([
        dependencies.getDefaultBranch(),
        dependencies.getEnhancedPRAttribution(context.getAppState),
      ])
      const spec = buildLegacyTuiSpec(
        defaultBranch,
        prAttribution,
        args?.trim() || undefined,
        dependencies.getAttributionTexts(),
      )

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
        '/commit-push-pr',
      )

      return [{ type: 'text', text: finalContent }]
    },
  } satisfies Command
}

const command = createCommitPushPrCommand()

export default command
