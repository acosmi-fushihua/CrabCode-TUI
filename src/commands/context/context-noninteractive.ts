import { feature } from '../../utils/featurePolyfill.js'
import { microcompactMessagesForAnalysis } from '../../services/compact/microCompact.js'
import type { AppState } from '../../state/AppStateStore.js'
import type { Tools, ToolUseContext } from '../../Tool.js'
import type { AgentDefinitionsResult } from '../../tools/AgentTool/loadAgentsDir.js'
import type {
  SDKControlGetContextUsageResponse,
  SDKControlResponse,
} from '../../entrypoints/sdk/controlTypes.js'
import type { Message } from '../../types/message.js'
import {
  analyzeContextUsage,
  type ContextData,
} from '../../utils/analyzeContext.js'
import { errorMessage } from '../../utils/errors.js'
import { formatTokens } from '../../utils/format.js'
import { getMessagesAfterCompactBoundary } from '../../utils/messages.js'
import { getSourceDisplayName } from '../../utils/settings/constants.js'
import { plural } from '../../utils/stringUtils.js'
import { SDKControlGetContextUsageResponseSchema } from '../../entrypoints/sdk/controlSchemas.js'

/**
 * Shared data-collection path for `/context` (slash command) and the SDK
 * `get_context_usage` control request. Mirrors query.ts's pre-API transforms
 * (compact boundary, existing snip projection, microcompact, projectView) so
 * the token count reflects
 * what the model actually sees.
 *
 * This read-only command deliberately does not run query.ts's *new* snip or
 * aggregate tool-result budget decisions. Both stages mutate/persist query
 * lifecycle state; replaying them from an inspection command could double
 * apply a replacement or create a boundary that was never sent to the model.
 * `getMessagesAfterCompactBoundary` still projects already-recorded snips, so
 * committed history state is represented without manufacturing new state.
 * Microcompact is likewise projected through its explicit analysis-only API;
 * cached tool registration, cache edits, warnings, and cache-break bookkeeping
 * remain owned exclusively by the real query lifecycle.
 */
export type CollectContextDataInput = {
  messages: Message[]
  getAppState: () => AppState
  options: {
    mainLoopModel: string
    tools: Tools
    agentDefinitions: AgentDefinitionsResult
    customSystemPrompt?: string
    appendSystemPrompt?: string
  }
}

/**
 * Injectable seams for the context collection pipeline. Production callers use
 * the default implementation below; tests replace the expensive analyzer and
 * microcompact stages without network, filesystem, feature-cache, or timer
 * state. Keeping the seams here also makes the transform order executable as a
 * contract instead of relying on source-text assertions.
 */
export type CollectContextDataDependencies = {
  getMessagesAfterCompactBoundary: (messages: Message[]) => Message[]
  isContextCollapseEnabled: () => boolean
  projectContextView: (messages: Message[]) => Message[]
  microcompactMessages: (
    messages: Message[],
    mainLoopModel: string,
  ) => Promise<{ messages: Message[] }>
  analyzeContextUsage: typeof analyzeContextUsage
}

const defaultCollectContextDataDependencies: CollectContextDataDependencies = {
  getMessagesAfterCompactBoundary,
  isContextCollapseEnabled: () => feature('CONTEXT_COLLAPSE'),
  projectContextView: messages => {
    /* eslint-disable @typescript-eslint/no-require-imports */
    const { projectView } =
      require('../../services/contextCollapse/operations.js') as typeof import('../../services/contextCollapse/operations.js')
    /* eslint-enable @typescript-eslint/no-require-imports */
    return projectView(messages)
  },
  microcompactMessages: (messages, mainLoopModel) =>
    microcompactMessagesForAnalysis(messages, {
      options: { mainLoopModel },
    } as ToolUseContext),
  analyzeContextUsage,
}

export async function collectContextData(
  context: CollectContextDataInput,
  dependencies: CollectContextDataDependencies = defaultCollectContextDataDependencies,
): Promise<ContextData> {
  const {
    messages,
    getAppState,
    options: {
      mainLoopModel,
      tools,
      agentDefinitions,
      customSystemPrompt,
      appendSystemPrompt,
    },
  } = context

  // Keep the exact pre-microcompact boundary/snip view for API-usage
  // extraction. `analyzeContextUsage` documents its last argument as the
  // original pre-microcompact messages.
  const originalMessages = dependencies.getMessagesAfterCompactBoundary(messages)
  const { messages: microcompactedMessages } =
    await dependencies.microcompactMessages(originalMessages, mainLoopModel)
  // query.ts runs context-collapse projection after microcompact. Projecting
  // before it can make cached microcompact inspect a synthetic collapsed view
  // that the actual model request never uses.
  const modelMessages = dependencies.isContextCollapseEnabled()
    ? dependencies.projectContextView(microcompactedMessages)
    : microcompactedMessages
  const appState = getAppState()

  return dependencies.analyzeContextUsage(
    modelMessages,
    mainLoopModel,
    async () => appState.toolPermissionContext,
    tools,
    agentDefinitions,
    undefined, // terminalWidth
    // analyzeContextUsage only reads options.{customSystemPrompt,appendSystemPrompt}
    // but its signature declares the full Pick<ToolUseContext, 'options'>.
    { options: { customSystemPrompt, appendSystemPrompt } } as Pick<
      ToolUseContext,
      'options'
    >,
    undefined, // mainThreadAgentDefinition
    originalMessages, // original messages for API usage extraction
  )
}

type ContextUsageControlRequest = {
  request_id: string
}

export type GetContextUsageControlInput = {
  message: ContextUsageControlRequest
  messages: Message[]
  getAppState: () => AppState
  getMainLoopModel: () => string
  buildTools: (appState: AppState) => Tools
  customSystemPrompt?: string
  appendSystemPrompt?: string
}

export type GetContextUsageControlDependencies = {
  collectContextData: typeof collectContextData
}

const defaultGetContextUsageControlDependencies: GetContextUsageControlDependencies = {
  collectContextData,
}

/**
 * Execute one correlated `get_context_usage` control request.
 *
 * The app state is snapped exactly once per request so the permission context,
 * dynamic tool pool, and dynamic agent catalog all describe the same runtime
 * instant. A later request obtains a fresh snapshot. Successful data is
 * validated against the public response schema before it can cross stdout;
 * collection or validation failures become an error response carrying the
 * original request id and do not mutate the transcript or app state.
 */
export async function handleGetContextUsageControlRequest(
  input: GetContextUsageControlInput,
  dependencies: GetContextUsageControlDependencies = defaultGetContextUsageControlDependencies,
): Promise<SDKControlResponse> {
  const { message } = input
  try {
    const appState = input.getAppState()
    const data = await dependencies.collectContextData({
      messages: input.messages,
      // Keep all inputs for this response on the same dynamic-state snapshot.
      getAppState: () => appState,
      options: {
        mainLoopModel: input.getMainLoopModel(),
        tools: input.buildTools(appState),
        agentDefinitions: appState.agentDefinitions,
        customSystemPrompt: input.customSystemPrompt,
        appendSystemPrompt: input.appendSystemPrompt,
      },
    })
    const response: SDKControlGetContextUsageResponse =
      SDKControlGetContextUsageResponseSchema().parse({ ...data })
    return {
      type: 'control_response',
      response: {
        subtype: 'success',
        request_id: message.request_id,
        response,
      },
    }
  } catch (error) {
    return {
      type: 'control_response',
      response: {
        subtype: 'error',
        request_id: message.request_id,
        error: errorMessage(error),
      },
    }
  }
}

export async function call(
  _args: string,
  context: ToolUseContext,
): Promise<{ type: 'text'; value: string }> {
  const data = await collectContextData(context)
  return {
    type: 'text' as const,
    value: formatContextAsMarkdownTable(data),
  }
}

function formatContextAsMarkdownTable(data: ContextData): string {
  const {
    categories,
    totalTokens,
    rawMaxTokens,
    percentage,
    model,
    memoryFiles,
    mcpTools,
    agents,
    skills,
    messageBreakdown,
    systemTools,
    systemPromptSections,
  } = data

  let output = `## Context Usage\n\n`
  output += `**Model:** ${model}  \n`
  output += `**Tokens:** ${formatTokens(totalTokens)} / ${formatTokens(rawMaxTokens)} (${percentage}%)\n`

  // Context-collapse status. Always show when the runtime gate is on —
  // the user needs to know which strategy is managing their context
  // even before anything has fired.
  if (feature('CONTEXT_COLLAPSE')) {
    /* eslint-disable @typescript-eslint/no-require-imports */
    const { getStats, isContextCollapseEnabled } =
      require('../../services/contextCollapse/index.js') as typeof import('../../services/contextCollapse/index.js')
    /* eslint-enable @typescript-eslint/no-require-imports */
    if (isContextCollapseEnabled()) {
      const s = getStats()
      const { health: h } = s

      const parts: string[] = []
      if (s.collapsedSpans > 0) {
        parts.push(
          `${s.collapsedSpans} ${plural(s.collapsedSpans, 'span')} summarized (${s.collapsedMessages} messages)`,
        )
      }
      if (s.stagedSpans > 0) parts.push(`${s.stagedSpans} staged`)
      const summary =
        parts.length > 0
          ? parts.join(', ')
          : h.totalSpawns > 0
            ? `${h.totalSpawns} ${plural(h.totalSpawns, 'spawn')}, nothing staged yet`
            : 'waiting for first trigger'
      output += `**Context strategy:** collapse (${summary})\n`

      if (h.totalErrors > 0) {
        output += `**Collapse errors:** ${h.totalErrors}/${h.totalSpawns} spawns failed`
        if (h.lastError) {
          output += ` (last: ${h.lastError.slice(0, 80)})`
        }
        output += '\n'
      } else if (h.emptySpawnWarningEmitted) {
        output += `**Collapse idle:** ${h.totalEmptySpawns} consecutive empty runs\n`
      }
    }
  }
  output += '\n'

  // Main categories table
  const visibleCategories = categories.filter(
    cat =>
      cat.tokens > 0 &&
      cat.name !== 'Free space' &&
      cat.name !== 'Autocompact buffer',
  )

  if (visibleCategories.length > 0) {
    output += `### Estimated usage by category\n\n`
    output += `| Category | Tokens | Percentage |\n`
    output += `|----------|--------|------------|\n`

    for (const cat of visibleCategories) {
      const percentDisplay = ((cat.tokens / rawMaxTokens) * 100).toFixed(1)
      output += `| ${cat.name} | ${formatTokens(cat.tokens)} | ${percentDisplay}% |\n`
    }

    const freeSpaceCategory = categories.find(c => c.name === 'Free space')
    if (freeSpaceCategory && freeSpaceCategory.tokens > 0) {
      const percentDisplay = (
        (freeSpaceCategory.tokens / rawMaxTokens) *
        100
      ).toFixed(1)
      output += `| Free space | ${formatTokens(freeSpaceCategory.tokens)} | ${percentDisplay}% |\n`
    }

    const autocompactCategory = categories.find(
      c => c.name === 'Autocompact buffer',
    )
    if (autocompactCategory && autocompactCategory.tokens > 0) {
      const percentDisplay = (
        (autocompactCategory.tokens / rawMaxTokens) *
        100
      ).toFixed(1)
      output += `| Autocompact buffer | ${formatTokens(autocompactCategory.tokens)} | ${percentDisplay}% |\n`
    }

    output += `\n`
  }

  // MCP tools
  if (mcpTools.length > 0) {
    output += `### MCP Tools\n\n`
    output += `| Tool | Server | Tokens |\n`
    output += `|------|--------|--------|\n`
    for (const tool of mcpTools) {
      output += `| ${tool.name} | ${tool.serverName} | ${formatTokens(tool.tokens)} |\n`
    }
    output += `\n`
  }

  // System tools (ant-only)
  if (
    systemTools &&
    systemTools.length > 0 &&
    process.env.USER_TYPE === 'ant'
  ) {
    output += `### [ANT-ONLY] System Tools\n\n`
    output += `| Tool | Tokens |\n`
    output += `|------|--------|\n`
    for (const tool of systemTools) {
      output += `| ${tool.name} | ${formatTokens(tool.tokens)} |\n`
    }
    output += `\n`
  }

  // System prompt sections (ant-only)
  if (
    systemPromptSections &&
    systemPromptSections.length > 0 &&
    process.env.USER_TYPE === 'ant'
  ) {
    output += `### [ANT-ONLY] System Prompt Sections\n\n`
    output += `| Section | Tokens |\n`
    output += `|---------|--------|\n`
    for (const section of systemPromptSections) {
      output += `| ${section.name} | ${formatTokens(section.tokens)} |\n`
    }
    output += `\n`
  }

  // Custom agents
  if (agents.length > 0) {
    output += `### Custom Agents\n\n`
    output += `| Agent Type | Source | Tokens |\n`
    output += `|------------|--------|--------|\n`
    for (const agent of agents) {
      let sourceDisplay: string
      switch (agent.source) {
        case 'projectSettings':
          sourceDisplay = 'Project'
          break
        case 'userSettings':
          sourceDisplay = 'User'
          break
        case 'localSettings':
          sourceDisplay = 'Local'
          break
        case 'flagSettings':
          sourceDisplay = 'Flag'
          break
        case 'policySettings':
          sourceDisplay = 'Policy'
          break
        case 'plugin':
          sourceDisplay = 'Plugin'
          break
        case 'built-in':
          sourceDisplay = 'Built-in'
          break
        default:
          sourceDisplay = String(agent.source)
      }
      output += `| ${agent.agentType} | ${sourceDisplay} | ${formatTokens(agent.tokens)} |\n`
    }
    output += `\n`
  }

  // Memory files
  if (memoryFiles.length > 0) {
    output += `### Memory Files\n\n`
    output += `| Type | Path | Tokens |\n`
    output += `|------|------|--------|\n`
    for (const file of memoryFiles) {
      output += `| ${file.type} | ${file.path} | ${formatTokens(file.tokens)} |\n`
    }
    output += `\n`
  }

  // Skills
  if (skills && skills.tokens > 0 && skills.skillFrontmatter.length > 0) {
    output += `### Skills\n\n`
    output += `| Skill | Source | Tokens |\n`
    output += `|-------|--------|--------|\n`
    for (const skill of skills.skillFrontmatter) {
      output += `| ${skill.name} | ${getSourceDisplayName(skill.source)} | ${formatTokens(skill.tokens)} |\n`
    }
    output += `\n`
  }

  // Message breakdown (ant-only)
  if (messageBreakdown && process.env.USER_TYPE === 'ant') {
    output += `### [ANT-ONLY] Message Breakdown\n\n`
    output += `| Category | Tokens |\n`
    output += `|----------|--------|\n`
    output += `| Tool calls | ${formatTokens(messageBreakdown.toolCallTokens)} |\n`
    output += `| Tool results | ${formatTokens(messageBreakdown.toolResultTokens)} |\n`
    output += `| Attachments | ${formatTokens(messageBreakdown.attachmentTokens)} |\n`
    output += `| Assistant messages (non-tool) | ${formatTokens(messageBreakdown.assistantMessageTokens)} |\n`
    output += `| User messages (non-tool-result) | ${formatTokens(messageBreakdown.userMessageTokens)} |\n`
    output += `\n`

    if (messageBreakdown.toolCallsByType.length > 0) {
      output += `#### Top Tools\n\n`
      output += `| Tool | Call Tokens | Result Tokens |\n`
      output += `|------|-------------|---------------|\n`
      for (const tool of messageBreakdown.toolCallsByType) {
        output += `| ${tool.name} | ${formatTokens(tool.callTokens)} | ${formatTokens(tool.resultTokens)} |\n`
      }
      output += `\n`
    }

    if (messageBreakdown.attachmentsByType.length > 0) {
      output += `#### Top Attachments\n\n`
      output += `| Attachment | Tokens |\n`
      output += `|------------|--------|\n`
      for (const attachment of messageBreakdown.attachmentsByType) {
        output += `| ${attachment.name} | ${formatTokens(attachment.tokens)} |\n`
      }
      output += `\n`
    }
  }

  return output
}
