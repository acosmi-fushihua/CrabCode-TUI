import type { Command } from '../../types/command.js'
import { feature } from '../../utils/featurePolyfill.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import { discoverWorkflows, type DiscoveredWorkflow } from './registry.js'

/**
 * Which discovered workflows may be exposed as slash commands.
 *
 * A slash command is an execution entry point, not just a listing. When only
 * the bundled-workflow flag is on, plugin-authored workflows must stay
 * unreachable — surfacing them here would run third-party scripts through
 * exactly the surface `WORKFLOW_SCRIPTS` is still gating.
 */
export function selectExposedWorkflows(
  workflows: readonly DiscoveredWorkflow[],
): DiscoveredWorkflow[] {
  if (feature('WORKFLOW_SCRIPTS')) return [...workflows]
  return workflows.filter(workflow => workflow.origin === 'bundled')
}

export async function getWorkflowCommands(
  cwd?: string,
): Promise<Command[]> {
  const { workflows } = await discoverWorkflows(cwd)
  return selectExposedWorkflows(workflows).map(workflow => {
    const bundled = workflow.origin === 'bundled'
    const baseInstruction = `Invoke the Workflow tool exactly once with name ${JSON.stringify(workflow.name)}. This is an executable ${bundled ? 'bundled' : 'installed'} workflow: do not reproduce, paraphrase, or simulate its internal steps.`
    return {
      name: workflow.name,
      userFacingName: () => workflow.name,
      description: workflow.meta.description,
      whenToUse: workflow.meta.whenToUse,
      type: 'prompt' as const,
      kind: 'workflow' as const,
      source: bundled ? ('bundled' as const) : ('plugin' as const),
      loadedFrom: bundled ? ('bundled' as const) : ('plugin' as const),
      progressMessage: `Starting ${workflow.name}`,
      contentLength: baseInstruction.length,
      allowedTools: [`Workflow(${workflow.name})`],
      userInvocable: true,
      async getPromptForCommand(args) {
        const workflowArgs = args.trim()
        return [
          {
            type: 'text' as const,
            text:
              `${baseInstruction}\n` +
              `Pass args as the following exact string (an empty string means an empty object):\n` +
              `<workflow-args>${jsonStringify(workflowArgs)}</workflow-args>`,
          },
        ]
      },
    } satisfies Command
  })
}
