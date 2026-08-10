// A workflow agent must not start without the tools it declares —
// W-WORKFLOW-TURN-BUDGET PR-6 (2026-08-02).
//
// `runAgent` resolves an agent's declared tools and silently drops whatever is
// missing. For a user-authored agent that is right: a smaller pool still does
// useful work. For a workflow it is the difference between a clear failure and
// a 30-minute run that produces nothing — `deep-research` declares
// `[WebSearch, WebFetch]`, and with WebSearch absent its five search agents
// are told in their own system prompt that they can search, discover they
// cannot, and keep going. §16's no-progress watchdog does not save it either:
// it only fires when every tool_result is an error, and a model that answers
// in prose instead of erroring never trips that.
//
// The original plan for this called it a "pre-flight probe inside the script".
// Validating the resolved pool on the host is strictly better: it covers every
// workflow rather than one script, and it reuses `resolveAgentTools` so the
// check cannot drift from what `runAgent` will actually hand over.

import { describe, expect, test } from 'bun:test'
import { assertWorkflowAgentToolsAreAvailable } from '../../src/tools/WorkflowTool/runtime.js'
import { WEB_RESEARCHER_AGENT } from '../../src/tools/WorkflowTool/bundled/agents.js'
import type { AgentDefinition } from '../../src/tools/AgentTool/loadAgentsDir.js'
import type { Tool } from '../../src/Tool.js'

function fakeTool(name: string): Tool {
  return {
    name,
    isReadOnly: () => true,
    isConcurrencySafe: () => true,
    isEnabled: async () => true,
  } as unknown as Tool
}

const agent = WEB_RESEARCHER_AGENT as unknown as AgentDefinition

describe('workflow agent tool availability', () => {
  test('passes when every declared tool is in the pool', () => {
    expect(() =>
      assertWorkflowAgentToolsAreAvailable(agent, [
        fakeTool('WebSearch'),
        fakeTool('WebFetch'),
        fakeTool('Read'),
      ]),
    ).not.toThrow()
  })

  test('names the missing tool instead of running a research agent that cannot search', () => {
    expect(() =>
      assertWorkflowAgentToolsAreAvailable(agent, [fakeTool('WebFetch')]),
    ).toThrow(/WebSearch/)
  })

  test('reports every missing tool at once', () => {
    let message = ''
    try {
      assertWorkflowAgentToolsAreAvailable(agent, [fakeTool('Read')])
    } catch (error) {
      message = error instanceof Error ? error.message : String(error)
    }
    expect(message).toContain('WebSearch')
    expect(message).toContain('WebFetch')
    expect(message).toContain(WEB_RESEARCHER_AGENT.agentType)
  })

  test('an agent that declares no tools asks for whatever exists and is never unsatisfiable', () => {
    const wildcard = { ...agent, tools: undefined } as AgentDefinition
    expect(() =>
      assertWorkflowAgentToolsAreAvailable(wildcard, []),
    ).not.toThrow()
    const explicitWildcard = { ...agent, tools: ['*'] } as AgentDefinition
    expect(() =>
      assertWorkflowAgentToolsAreAvailable(explicitWildcard, []),
    ).not.toThrow()
  })
})
