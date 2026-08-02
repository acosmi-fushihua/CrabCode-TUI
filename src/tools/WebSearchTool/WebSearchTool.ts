
import type { BetaContentBlock, BetaWebSearchTool20250305 } from '../../types/api-types.js'
import { getAPIProvider } from 'src/utils/model/providers.js'
import { getCachedCapabilityWithDefaultFallback } from 'src/utils/model/modelCapabilities.js'
import type { PermissionResult } from 'src/utils/permissions/PermissionResult.js'
import { z } from 'zod/v4'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../../services/analytics/growthbook.js'
import { queryModelWithStreaming } from '../../services/api/crabcode.js'
import {
  executeSearch,
  isSearchProviderConfigured,
} from '../../services/search/search.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { logError } from '../../utils/log.js'
import { createUserMessage } from '../../utils/messages.js'
import { getMainLoopModel, getSmallFastModel } from '../../utils/model/model.js'
import { isNonGatewayModelReference } from '../../utils/model/nonGatewayModelReference.js'
import { jsonParse, jsonStringify } from '../../utils/slowOperations.js'
import { asSystemPrompt } from '../../utils/systemPromptType.js'
import { getWebSearchPrompt, WEB_SEARCH_TOOL_NAME } from './prompt.js'
import { webSearchToolUseSummary as getToolUseSummary } from '../toolMetadata.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'
import { getCachedModelCapabilities } from '../../utils/model/modelCapabilities.js'

const inputSchema = lazySchema(() =>
  z.strictObject({
    query: z.string().min(2).describe('The search query to use'),
    allowed_domains: z
      .array(z.string())
      .optional()
      .describe('Only include search results from these domains'),
    blocked_domains: z
      .array(z.string())
      .optional()
      .describe('Never include search results from these domains'),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

type Input = z.infer<InputSchema>

const searchResultSchema = lazySchema(() => {
  const searchHitSchema = z.object({
    title: z.string().describe('The title of the search result'),
    url: z.string().describe('The URL of the search result'),
  })

  return z.object({
    tool_use_id: z.string().describe('ID of the tool use'),
    content: z.array(searchHitSchema).describe('Array of search hits'),
  })
})

export type SearchResult = z.infer<ReturnType<typeof searchResultSchema>>

const outputSchema = lazySchema(() =>
  z.object({
    query: z.string().describe('The search query that was executed'),
    results: z
      .array(z.union([searchResultSchema(), z.string()]))
      .describe('Search results and/or text commentary from the model'),
    durationSeconds: z
      .number()
      .describe('Time taken to complete the search operation'),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>

export type Output = z.infer<OutputSchema>

// Re-export WebSearchProgress from centralized types to break import cycles
export type { WebSearchProgress } from '../../types/tools.js'

import type { WebSearchProgress } from '../../types/tools.js'

function makeToolSchema(input: Input): BetaWebSearchTool20250305 {
  return {
    type: 'web_search_20250305',
    name: 'web_search',
    allowed_domains: input.allowed_domains,
    blocked_domains: input.blocked_domains,
    max_uses: 8, // Hardcoded to 8 searches maximum
  }
}

function makeOutputFromSearchResponse(
  result: BetaContentBlock[],
  query: string,
  durationSeconds: number,
): Output {
  // The result is a sequence of these blocks:
  // - text to start -- always?
  // [
  //    - server_tool_use
  //    - web_search_tool_result
  //    - text and citation blocks intermingled
  //  ]+  (this block repeated for each search)

  const results: (SearchResult | string)[] = []
  let textAcc = ''
  let inText = true

  for (const block of result) {
    // Acosmi 2026-04-28 网关 fallback：非 server-tool-capable 模型（DeepSeek/Qwen/...）
    // 的 web_search server tool 被改写为 client function tool；网关合并的
    // SSE 流里出现 tool_use(name='web_search') + 上游模型整合后的 text，
    // 没有 server_tool_use / web_search_tool_result 块。把这种形状当作
    // 一次有效搜索计数（hits 留空——内容已被模型整合进随后的 text 块）。
    const isGatewayFallbackSearch =
      block.type === 'tool_use' &&
      (block as { name?: string }).name === 'web_search'

    if (block.type === 'server_tool_use' || isGatewayFallbackSearch) {
      if (inText) {
        inText = false
        if (textAcc.trim().length > 0) {
          results.push(textAcc.trim())
        }
        textAcc = ''
      }
      if (isGatewayFallbackSearch) {
        results.push({
          tool_use_id: (block as { id: string }).id,
          content: [],
        })
      }
      continue
    }

    if (block.type === 'web_search_tool_result') {
      // Handle error case - content is a WebSearchToolResultError
      if (!Array.isArray(block.content)) {
        const errorMessage = `Web search error: ${(block.content as any).error_code}`
        logError(new Error(errorMessage))
        results.push(errorMessage)
        continue
      }
      // Success case - add results to our collection
      const hits = block.content.map(r => ({ title: r.title, url: r.url }))
      results.push({
        tool_use_id: block.tool_use_id,
        content: hits,
      })
    }

    if (block.type === 'text') {
      if (inText) {
        textAcc += block.text
      } else {
        inText = true
        textAcc = block.text
      }
    }
  }

  if (textAcc.length) {
    results.push(textAcc.trim())
  }

  return {
    query,
    results,
    durationSeconds,
  }
}

/**
 * Execute search via the in-process search service (Mode B: local search
 * providers). Calls Ali (DashScope) or Bocha directly — no Go IPC, no hub.
 */
async function callLocalSearch(
  input: Input,
  startTime: number,
  onProgress?: (data: { toolUseID: string; data: WebSearchProgress }) => void,
): Promise<{ data: Output }> {
  const { query } = input

  // Progress: query starting
  onProgress?.({
    toolUseID: 'local-search-1',
    data: { type: 'query_update', query },
  })

  const response = await executeSearch(query, {
    maxResults: 8,
    allowedDomains: input.allowed_domains,
    blockedDomains: input.blocked_domains,
  })

  const durationSeconds = (performance.now() - startTime) / 1000

  if (!response || !response.results) {
    return {
      data: {
        query,
        results: ['No search results available. Local search provider may not be configured.'],
        durationSeconds,
      },
    }
  }

  // Progress: results received
  onProgress?.({
    toolUseID: 'local-search-2',
    data: {
      type: 'search_results_received',
      resultCount: response.results.length,
      query,
    },
  })

  // Format results to match upstream output format
  const results: (SearchResult | string)[] = []

  if (response.results.length > 0) {
    // Add structured search hits
    results.push({
      tool_use_id: 'local-search',
      content: response.results.map((r) => ({
        title: r.title,
        url: r.url,
      })),
    })

    // Add text summary from snippets
    const snippetText = response.results
      .map((r) => `${r.title}\n${r.url}\n${r.snippet || r.content || ''}`)
      .join('\n\n')
    if (snippetText) {
      results.push(snippetText)
    }
  }

  return {
    data: {
      query,
      results,
      durationSeconds,
    },
  }
}

export const WebSearchTool = buildTool({
  name: WEB_SEARCH_TOOL_NAME,
  // OpenAI/DeepSeek/Qwen-style snake_case alias — non-server-tool upstreams emit
  // `web_search` from training regardless of the declared PascalCase name,
  // so the dispatcher must accept both.
  aliases: ['web_search'],
  searchHint: 'search the web for current information',
  maxResultSizeChars: 100_000,
  shouldDefer: true,
  async description(input) {
    return `CrabCode wants to search the web for: ${input.query}`
  },
  userFacingName() {
    return 'Web Search'
  },
  getToolUseSummary,
  getActivityDescription(input) {
    const summary = getToolUseSummary(input)
    return summary ? `Searching for ${summary}` : 'Searching the web'
  },
  isEnabled() {
    const provider = getAPIProvider()
    const model = getMainLoopModel()

    // firstParty: direct API → always Mode A
    if (provider === 'firstParty') {
      return true
    }

    // acosmi: gateway-routed. SDK capability `supports_web_search` decides
    // dispatch — true → Mode A (server_tool_use protocol), false/undefined
    // → Mode B (in-process Ali/Bocha) because upstreams without server-tool
    // support silently 0-result on Mode A.
    if (provider === 'acosmi') {
      if (getCachedModelCapabilities(model)?.supports_web_search === true) {
        return true
      }
      return isSearchProviderConfigured()
    }

    if (provider === 'vertex') {
      return (
        getCachedCapabilityWithDefaultFallback(model, 'supports_web_search') ??
        false
      )
    }

    // Foundry only ships models that already support Web Search
    if (provider === 'foundry') {
      return true
    }

    // Custom model: enable if local search provider is configured
    // (search executes in-process via src/services/search).
    if (provider === 'custom') {
      return isSearchProviderConfigured()
    }

    return false
  },
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  isConcurrencySafe() {
    return true
  },
  isReadOnly() {
    return true
  },
  toAutoClassifierInput(input) {
    return input.query
  },
  async checkPermissions(_input): Promise<PermissionResult> {
    return {
      behavior: 'passthrough',
      message: 'WebSearchTool requires permission.',
      suggestions: [
        {
          type: 'addRules',
          rules: [{ toolName: WEB_SEARCH_TOOL_NAME }],
          behavior: 'allow',
          destination: 'localSettings',
        },
      ],
    }
  },
  async prompt() {
    return getWebSearchPrompt()
  },
  ...createToolPresentationDelegates(WEB_SEARCH_TOOL_NAME, [
    'renderToolUseMessage',
    'renderToolUseProgressMessage',
    'renderToolResultMessage',
  ]),
  extractSearchText() {
    // renderToolResultMessage shows only "Did N searches in Xs" chrome —
    // the results[] content never appears on screen. Heuristic would index
    // string entries in results[] (phantom match). Nothing to search.
    return ''
  },
  async validateInput(input) {
    const { query, allowed_domains, blocked_domains } = input
    if (!query.length) {
      return {
        result: false,
        message: 'Error: Missing query',
        errorCode: 1,
      }
    }
    if (allowed_domains?.length && blocked_domains?.length) {
      return {
        result: false,
        message:
          'Error: Cannot specify both allowed_domains and blocked_domains in the same request',
        errorCode: 2,
      }
    }
    return { result: true }
  },
  async call(input, context, _canUseTool, _parentMessage, onProgress) {
    const startTime = performance.now()
    const { query } = input
    const provider = getAPIProvider()
    const model = getMainLoopModel()

    // ── Mode B: Local search via in-process Ali/Bocha ──
    // Triggered when:
    //   - provider === 'custom' (user-supplied non-server-tool API), OR
    //   - provider === 'acosmi' AND model lacks SDK supports_web_search caps
    //     because those upstreams silently 0-result on server-tool web_search.
    // Both paths require a search provider configured (env + API key).
    if (isSearchProviderConfigured()) {
      if (provider === 'custom') {
        return callLocalSearch(input, startTime, onProgress)
      }
      if (
        provider === 'acosmi' &&
        getCachedModelCapabilities(model)?.supports_web_search !== true
      ) {
        return callLocalSearch(input, startTime, onProgress)
      }
    }

    // ── Mode A: Upstream server tool (existing logic) ──
    const userMessage = createUserMessage({
      content: 'Perform a web search for the query: ' + query,
    })
    const toolSchema = makeToolSchema(input)

    // Privacy invariant (`sessionAuxiliaryRouting.ts`): when the session runs
    // on a non-gateway namespace (`account:` / `custom:` / `local:`), its
    // transcript-derived work must stay on that route and must never be
    // substituted onto a managed small/fast model. Fast mode does exactly
    // that substitution, so it is unavailable for those sessions — the same
    // route the session already uses when the flag is off.
    const sessionRunsOnGateway = !isNonGatewayModelReference(
      context.options.mainLoopModel,
    )
    const useFastMode =
      sessionRunsOnGateway &&
      getFeatureValue_CACHED_MAY_BE_STALE('tengu_plum_vx3', false)

    const appState = context.getAppState()
    const queryStream = queryModelWithStreaming({
      messages: [userMessage],
      systemPrompt: asSystemPrompt([
        'You are an assistant for performing a web search tool use',
      ]),
      thinkingConfig: useFastMode
        ? { type: 'disabled' as const }
        : context.options.thinkingConfig,
      tools: [],
      signal: context.abortController.signal,
      options: {
        getToolPermissionContext: async () => appState.toolPermissionContext,
        model: useFastMode ? getSmallFastModel() : context.options.mainLoopModel,
        toolChoice: useFastMode ? { type: 'tool', name: 'web_search' } : undefined,
        isNonInteractiveSession: context.options.isNonInteractiveSession,
        hasAppendSystemPrompt: !!context.options.appendSystemPrompt,
        extraToolSchemas: [toolSchema],
        querySource: 'web_search_tool',
        agents: context.options.agentDefinitions.activeAgents,
        mcpTools: [],
        agentId: context.agentId,
        effortValue: appState.effortValue,
      },
    })

    const allContentBlocks: BetaContentBlock[] = []
    let currentToolUseId: string | null = null
    let currentToolUseJson = ''
    let progressCounter = 0
    const toolUseQueries = new Map() // Map of tool_use_id to query

    for await (const event of queryStream) {
      if (event.type === 'assistant') {
        allContentBlocks.push(...event.message.content)
        continue
      }

      // Track tool use ID when server_tool_use OR gateway-fallback
      // tool_use(name='web_search') starts. See note in
      // makeOutputFromSearchResponse for the fallback shape.
      if (
        event.type === 'stream_event' &&
        event.event?.type === 'content_block_start'
      ) {
        const contentBlock = event.event.content_block
        const isFallbackToolUse =
          contentBlock?.type === 'tool_use' &&
          (contentBlock as { name?: string }).name === 'web_search'
        if (
          contentBlock &&
          (contentBlock.type === 'server_tool_use' || isFallbackToolUse)
        ) {
          currentToolUseId = (contentBlock as { id: string }).id
          currentToolUseJson = ''
          continue
        }
      }

      // Accumulate JSON for current tool use
      if (
        currentToolUseId &&
        event.type === 'stream_event' &&
        event.event?.type === 'content_block_delta'
      ) {
        const delta = event.event.delta
        if (delta?.type === 'input_json_delta' && delta.partial_json) {
          currentToolUseJson += delta.partial_json

          // Try to extract query from partial JSON for progress updates
          try {
            // Look for a complete query field
            const queryMatch = currentToolUseJson.match(
              /"query"\s*:\s*"((?:[^"\\]|\\.)*)"/,
            )
            if (queryMatch && queryMatch[1]) {
              // The regex properly handles escaped characters
              const query = jsonParse('"' + queryMatch[1] + '"')

              if (
                !toolUseQueries.has(currentToolUseId) ||
                toolUseQueries.get(currentToolUseId) !== query
              ) {
                toolUseQueries.set(currentToolUseId, query)
                progressCounter++
                if (onProgress) {
                  onProgress({
                    toolUseID: `search-progress-${progressCounter}`,
                    data: {
                      type: 'query_update',
                      query,
                    },
                  })
                }
              }
            }
          } catch {
            // Ignore parsing errors for partial JSON
          }
        }
      }

      // Yield progress when search results come in
      if (
        event.type === 'stream_event' &&
        event.event?.type === 'content_block_start'
      ) {
        const contentBlock = event.event.content_block
        if (contentBlock && contentBlock.type === 'web_search_tool_result') {
          // Get the actual query that was used for this search
          const toolUseId = contentBlock.tool_use_id
          const actualQuery = toolUseQueries.get(toolUseId) || query
          const content = contentBlock.content

          progressCounter++
          if (onProgress) {
            onProgress({
              toolUseID: toolUseId || `search-progress-${progressCounter}`,
              data: {
                type: 'search_results_received',
                resultCount: Array.isArray(content) ? content.length : 0,
                query: actualQuery,
              },
            })
          }
        }
      }
    }

    // Process the final result
    const endTime = performance.now()
    const durationSeconds = (endTime - startTime) / 1000

    const data = makeOutputFromSearchResponse(
      allContentBlocks,
      query,
      durationSeconds,
    )
    return { data }
  },
  mapToolResultToToolResultBlockParam(output, toolUseID) {
    const { query, results } = output

    let formattedOutput = `Web search results for query: "${query}"\n\n`

    // Process the results array - it can contain both string summaries and search result objects.
    // Guard against null/undefined entries that can appear after JSON round-tripping
    // (e.g., from compaction or transcript deserialization).
    ;(results ?? []).forEach(result => {
      if (result == null) {
        return
      }
      if (typeof result === 'string') {
        // Text summary
        formattedOutput += result + '\n\n'
      } else {
        // Search result with links
        if (result.content?.length > 0) {
          formattedOutput += `Links: ${jsonStringify(result.content)}\n\n`
        } else {
          formattedOutput += 'No links found.\n\n'
        }
      }
    })

    formattedOutput +=
      '\nREMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.'

    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: formattedOutput.trim(),
    }
  },
} satisfies ToolDef<InputSchema, Output, WebSearchProgress>)
