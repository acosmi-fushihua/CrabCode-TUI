import type { Command } from '../../types/command.js'
import { getCwd } from '../../utils/cwd.js'
import { discoverWorkflows } from '../../tools/WorkflowTool/registry.js'

const workflowsCommand: Command = {
  name: 'workflows',
  description: 'List executable workflows from enabled plugins',
  type: 'local',
  supportsNonInteractive: true,
  async load() {
    return {
      async call(args) {
        const query = args.trim().toLowerCase()
        const { workflows, errors } = await discoverWorkflows(getCwd())
        const visible = query
          ? workflows.filter(
              workflow =>
                workflow.name.toLowerCase().includes(query) ||
                workflow.meta.description.toLowerCase().includes(query),
            )
          : workflows
        const lines = visible.map(
          workflow =>
            `- /${workflow.name} — ${workflow.meta.description}`,
        )
        if (errors.length > 0) {
          lines.push(
            '',
            `${errors.length} workflow file(s) could not be loaded. Run with debug logging for details.`,
          )
        }
        return {
          type: 'text',
          value:
            lines.join('\n') ||
            (query
              ? `No workflows match "${args.trim()}".`
              : 'No enabled plugin workflows were found.'),
        }
      },
    }
  },
}

export default workflowsCommand
