import type { Tools, ToolUseContext } from '../../Tool.js'

export type AutomationTurnSurface = 'builtin'

/** Pure TUI exposes no host-owned Chrome or desktop-control tool family. */
export function isInteractiveAutomationToolName(_toolName: string): boolean {
  return false
}

export function removeInteractiveAutomationTools(tools: Tools): Tools {
  return tools
}

type AutomationTurnMcpMetadata = Pick<
  ToolUseContext['options'],
  'mcpClients' | 'mcpResources'
>

export function selectAutomationTurnMcpMetadata(
  metadata: AutomationTurnMcpMetadata,
  _surface: AutomationTurnSurface | undefined,
  _turnTools: Tools,
): AutomationTurnMcpMetadata {
  return metadata
}

export function removeInteractiveAutomationToolsFromOptions(
  options: ToolUseContext['options'],
): ToolUseContext['options'] {
  return options
}

export function removeInteractiveAutomationToolsFromContext(
  context: ToolUseContext,
): ToolUseContext {
  return context
}

export function selectAutomationTurnTools(
  tools: Tools,
  surface: AutomationTurnSurface | undefined,
  allowedTools: readonly string[] | undefined,
  capability: {
    localAutomationRoutingAllowed: boolean
  },
): Tools {
  if (surface === undefined) return tools
  if (!capability.localAutomationRoutingAllowed || allowedTools === undefined) {
    return []
  }
  const allowed = new Set(allowedTools)
  return tools.filter(tool => tool.name === 'Bash' && allowed.has(tool.name))
}
