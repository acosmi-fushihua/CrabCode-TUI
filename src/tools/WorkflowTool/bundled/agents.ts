/**
 * Agents that exist only to serve bundled workflows.
 *
 * These are deliberately NOT registered in `getBuiltInAgents()`. That list is
 * the model-facing catalog: everything in it is rendered into the Agent tool's
 * description and is spawnable by the main loop. A research agent that only
 * ever makes sense inside `deep-research` would be pure prompt weight there,
 * and every session would pay for it.
 *
 * Keeping them out of `activeAgents` makes the invisibility structural rather
 * than a filter that some future render site could forget to apply. The
 * workflow runtime resolves this table only for workflows whose `origin` is
 * `bundled`, so plugin workflows cannot reach these definitions either.
 *
 * User deny rules still apply: the runtime runs `filterDeniedAgents` over the
 * merged candidate list, so `Agent(web-researcher)` in a deny rule removes it
 * exactly like any other agent.
 */

import type { BuiltInAgentDefinition } from '../../AgentTool/loadAgentsDir.js'
import { WEB_FETCH_TOOL_NAME } from '../../WebFetchTool/prompt.js'
import { WEB_SEARCH_TOOL_NAME } from '../../WebSearchTool/prompt.js'

export const WEB_RESEARCHER_AGENT_TYPE = 'web-researcher'

function getWebResearcherSystemPrompt(): string {
  return `You are a web research agent for CrabCode. You gather and assess evidence from the open web; you do not write code and you do not modify anything.

Your only tools are ${WEB_SEARCH_TOOL_NAME} and ${WEB_FETCH_TOOL_NAME}. Use them — do not answer from memory. If a task asks you to search or fetch and the tool is unavailable or every attempt fails, say so plainly in your result instead of substituting recalled knowledge.

Evidence standards:
- Prefer primary sources (the original paper, spec, filing, vendor documentation) over reporting about them, and reporting over blogs and forum posts.
- Quote what the source actually says. Never paraphrase a quote into something stronger than the original.
- Record the publication date when the page shows one. For fast-moving topics an undated or old page is weak evidence.
- Treat marketing pages, press releases, vendor benchmarks and SEO content farms as low quality even when they are well written.
- If a page is paywalled, empty, or unrelated to the question, report that outcome rather than inventing content for it.

When you are asked to judge a claim, argue against it first. A claim survives only on evidence, not on plausibility. Being unable to find contradicting evidence is not the same as finding supporting evidence — say which one happened.

Report only what the sources support. Distinguish what a source states, what you inferred, and what remains unknown.`
}

/**
 * Web research agent used by the bundled `deep-research` workflow for all five
 * of its roles (scoping, searching, extracting, adversarial verification and
 * synthesis). The per-role instructions live in the workflow's prompts; this
 * definition supplies the shared evidence discipline and the tool floor.
 *
 * `model` is intentionally omitted so the agent follows
 * `getDefaultSubagentModel()`. Pinning a model here would both hardcode a
 * brand literal and be rejected by the runtime, which refuses per-call model
 * overrides on the grounds that the agent definition is authoritative.
 */
export const WEB_RESEARCHER_AGENT: BuiltInAgentDefinition = {
  agentType: WEB_RESEARCHER_AGENT_TYPE,
  whenToUse:
    'Gathers and adversarially assesses evidence from the open web. Used by the bundled deep-research workflow; not intended for direct invocation.',
  // Least privilege: research needs to read the web and nothing else. The
  // structured-output tool is appended by cloneAgentForWorkflow when the
  // workflow passes a schema.
  tools: [WEB_SEARCH_TOOL_NAME, WEB_FETCH_TOOL_NAME],
  // Bound unattended workflow depth independently from the total wall clock.
  maxTurns: 30,
  source: 'built-in',
  baseDir: 'built-in',
  getSystemPrompt: getWebResearcherSystemPrompt,
}

const BUNDLED_WORKFLOW_AGENTS: readonly BuiltInAgentDefinition[] = [
  WEB_RESEARCHER_AGENT,
]

export function getBundledWorkflowAgents(): BuiltInAgentDefinition[] {
  return [...BUNDLED_WORKFLOW_AGENTS]
}
