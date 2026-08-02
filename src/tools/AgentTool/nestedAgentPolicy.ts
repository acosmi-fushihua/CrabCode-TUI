import { permissionRuleValueFromString } from '../../utils/permissions/permissionRuleParser.js'
import { AGENT_TOOL_NAME } from './constants.js'

export type NestedAgentPolicy = {
  allowTool: boolean
  allowedAgentTypes: string[]
}

export type AgentAsyncLifecycleSources = {
  explicitBackground: boolean
  definitionBackground: boolean
  coordinator: boolean
  forkSubagent: boolean
  assistant: boolean
  proactive: boolean
  backgroundTasksDisabled: boolean
}

/**
 * Resolve every source that can detach an AgentTool lifecycle. Nested agents
 * must never detach: their parent owns cancellation and, for Workflow roots,
 * the scheduler lease. Keeping the sources explicit prevents a newly added
 * force-async gate from silently bypassing the input/definition guards.
 */
export function resolveAgentAsyncLifecycle(
  sources: AgentAsyncLifecycleSources,
  isNested: boolean,
): boolean {
  const activeSources = [
    sources.explicitBackground && 'run_in_background',
    sources.definitionBackground && 'agent definition',
    sources.coordinator && 'coordinator mode',
    sources.forkSubagent && 'fork-subagent mode',
    sources.assistant && 'assistant mode',
    sources.proactive && 'proactive mode',
  ].filter((source): source is string => Boolean(source))
  const shouldRunAsync =
    !sources.backgroundTasksDisabled && activeSources.length > 0

  if (isNested && shouldRunAsync) {
    throw new Error(
      `Nested agents cannot enter an asynchronous lifecycle (${activeSources.join(
        ', ',
      )}); the parent must retain cancellation and scheduler ownership.`,
    )
  }
  return shouldRunAsync
}

export function mayBackgroundForegroundAgent(
  isNested: boolean,
  backgroundTasksDisabled: boolean,
): boolean {
  return !isNested && !backgroundTasksDisabled
}

/**
 * Plugin workers receive Agent only through one or more non-empty Agent(...)
 * declarations. Bare Agent never means wildcard access, and repeated
 * declarations compose without widening beyond their exact targets.
 */
export function resolveNestedAgentPolicy(
  agentTools: readonly string[] | undefined,
): NestedAgentPolicy {
  const allowedAgentTypes = [
    ...new Set(
      (agentTools ?? []).flatMap(toolSpec => {
        const { toolName, ruleContent } =
          permissionRuleValueFromString(toolSpec)
        if (toolName !== AGENT_TOOL_NAME || !ruleContent?.trim()) return []
        return ruleContent
          .split(',')
          .map(agentType => agentType.trim())
          .filter(Boolean)
      }),
    ),
  ]
  return {
    allowTool: allowedAgentTypes.length > 0,
    allowedAgentTypes,
  }
}
