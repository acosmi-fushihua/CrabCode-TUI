
import type { BetaContentBlock, BetaWebSearchTool20250305 } from '../../types/api-types.js'
import { APIUserAbortError } from '../../errors/api-errors.js'
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
import { buildTool, type ToolDef, type ToolUseContext } from '../../Tool.js'
import { createCombinedAbortSignal } from '../../utils/combinedAbortSignal.js'
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

/**
 * WebSearch 单次工具调用的显式墙钟预算。
 *
 * 本工具的 Mode A 是一条**嵌套模型流**: 工具内部再调 `queryModelWithStreaming`, 让主
 * 循环模型带着 `web_search` server tool 跑最多 8 次搜索。在本常量落地之前, 整个
 * `src/tools/WebSearchTool/` 里唯一与时间有关的东西只有一句
 * `signal: context.abortController.signal` —— 也就是说这条嵌套模型流**没有任何属于
 * 自己的上界**, 完全隐式继承别人的默认值。每条裸调 chat 的路径都必须声明自己的
 * 预算；隐式继承控制面的默认值正是这里要避免的事故形态。同门范本:
 * `sideQuery.ts::SIDE_QUERY_REQUEST_TIMEOUT_MS` /
 * `tokenEstimation.ts::TOKEN_COUNT_REQUEST_TIMEOUT_MS`(均 60s)。
 *
 * 为什么不照抄那两处的 60s: 它们是「一问一答的辅助小请求」, 而本工具是一条会跑多次
 * 搜索、带 thinking、可能触发非流式降级的完整模型流, 量级不同。取 120s。
 *
 * 与内层预算的次序（内层必须小于外层）:
 *   - `queryModel.ts` 的流空闲看门狗 90s(零 chunk 才计时, 慢而活的流会不断重置) < 120s
 *   - Mode B 的 `ali.ts::ALI_TIMEOUT_MS` / `bocha.ts::BOCHA_TIMEOUT_MS` 均 30s < 120s
 * 因此正常情况下先开火的永远是内层那个**可解释、可重试**的错误; 本预算只兜住内层照不
 * 到的那一类 —— 一条永远在滴答出帧、因而永远不空闲、也就永远不会被任何内层看门狗判死
 * 的流(它同时还会持续喂饱宿主的轮次活性时钟, 把整个回合一起吊住)。
 */
export const WEB_SEARCH_REQUEST_TIMEOUT_MS = 120_000

/**
 * `Error.name` stamped on the budget-expiry error, so callers / logs can tell
 * 「我们自己的预算到点了」from a transport failure or a user cancel without
 * string-matching the message. Same convention as
 * `sideQuery.ts::SIDE_QUERY_DESTINATION_ERROR_NAME`.
 */
export const WEB_SEARCH_BUDGET_ERROR_NAME = 'WebSearchBudgetExhaustedError'

/**
 * 预算到期 ≠ 调用方取消。二者都会让派生 signal 变成 aborted, 但对用户是完全不同的
 * 两件事: 前者必须报错(否则就是把一个被腰斩的搜索结果冒充成完整结果), 后者是用户按了
 * 停止, 既有的取消路径原样保留。判据与 sideQuery 同形。
 */
export function isWebSearchBudgetExpiry(
  budgetSignal: AbortSignal,
  callerSignal: AbortSignal,
): boolean {
  return budgetSignal.aborted && !callerSignal.aborted
}

/** 预算到期时抛出的可辨识错误(消息里带工具名与预算值, 方便日志与用户定位)。 */
export function webSearchBudgetExhaustedError(cause?: unknown): Error {
  const error = new Error(
    `WebSearch: no response within ${WEB_SEARCH_REQUEST_TIMEOUT_MS}ms budget ` +
      `(WEB_SEARCH_REQUEST_TIMEOUT_MS); the search was cancelled with no usable ` +
      `result. Retry with a narrower query if the information is still needed.`,
    cause === undefined ? undefined : { cause },
  )
  error.name = WEB_SEARCH_BUDGET_ERROR_NAME
  return error
}

/**
 * `queryModelWithStreaming` intentionally turns APIUserAbortError into a clean
 * end-of-stream. Restore the two distinct terminal meanings before any partial
 * blocks are assembled into a successful WebSearch result.
 */
export function assertWebSearchStreamCompleted(
  budgetSignal: AbortSignal,
  callerSignal: AbortSignal,
): void {
  // Caller cancellation wins even if the outer budget happened to expire in
  // the same turn. Treating this as success is the dangerous case; treating it
  // as a budget failure would also misreport an explicit user action.
  if (callerSignal.aborted) throw new APIUserAbortError()
  if (isWebSearchBudgetExpiry(budgetSignal, callerSignal)) {
    throw webSearchBudgetExhaustedError()
  }
}

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
  budgetSignal: AbortSignal,
  onProgress?: (data: { toolUseID: string; data: WebSearchProgress }) => void,
): Promise<{ data: Output }> {
  const { query } = input

  // Progress: query starting
  onProgress?.({
    toolUseID: 'local-search-1',
    data: { type: 'query_update', query },
  })

  // `executeSearch` 一直有 signal 形参, 但此前这里一个都没传 —— 于是 Mode B 既收不到
  // 本工具的预算, 也收不到用户的取消。provider 自己的 30s 内层预算是更紧的那一层,
  // 本预算只是它的外层兜底(见 WEB_SEARCH_REQUEST_TIMEOUT_MS 的次序说明)。
  const response = await executeSearch(
    query,
    {
      maxResults: 8,
      allowedDomains: input.allowed_domains,
      blockedDomains: input.blocked_domains,
    },
    budgetSignal,
  )

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

/**
 * Mode A: 上游 server tool 搜索 —— 工具内部再跑一条完整的模型流。
 *
 * 从 `call` 里抽出来只是为了让 `call` 能干净地当预算所有者(创建 / 判定 / cleanup 各
 * 一处); 流消费逻辑逐行未变, 唯一的实质改动是 `signal` 收的是**预算派生 signal** 而
 * 不再是裸的 `context.abortController.signal`。
 */
async function runUpstreamServerToolSearch(
  input: Input,
  context: ToolUseContext,
  startTime: number,
  budgetSignal: AbortSignal,
  onProgress?: (data: { toolUseID: string; data: WebSearchProgress }) => void,
): Promise<{ data: Output }> {
  const { query } = input
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
    signal: budgetSignal,
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

  // queryModel turns APIUserAbortError into a clean end-of-stream. Reassert
  // caller cancellation first, then the tool's own budget, before partial or
  // empty blocks can be presented as a successful search.
  assertWebSearchStreamCompleted(
    budgetSignal,
    context.abortController.signal,
  )

  // Process the final result
  const endTime = performance.now()
  const durationSeconds = (endTime - startTime) / 1000

  const data = makeOutputFromSearchResponse(
    allContentBlocks,
    query,
    durationSeconds,
  )
  return { data }
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
    const provider = getAPIProvider()
    const model = getMainLoopModel()

    // 本工具唯一的预算创建点 —— 两个 Mode 共用同一份, 出口只有一个 cleanup。
    // 派生自调用方 signal(取二者先到者), 所以用户按停止依旧立刻生效, 语义不变。
    const callerSignal = context.abortController.signal
    const budget = createCombinedAbortSignal(callerSignal, {
      timeoutMs: WEB_SEARCH_REQUEST_TIMEOUT_MS,
    })

    try {
      // ── Mode B: Local search via in-process Ali/Bocha ──
      // Triggered when:
      //   - provider === 'custom' (user-supplied non-server-tool API), OR
      //   - provider === 'acosmi' AND model lacks SDK supports_web_search caps
      //     because those upstreams silently 0-result on server-tool web_search.
      // Both paths require a search provider configured (env + API key).
      //
      // `return await` 而不是 `return`: 少了 await, finally 里的 cleanup 会在
      // Promise 落定**之前**就把定时器清掉, 预算等于没生效。
      if (isSearchProviderConfigured()) {
        if (provider === 'custom') {
          return await callLocalSearch(
            input,
            startTime,
            budget.signal,
            onProgress,
          )
        }
        if (
          provider === 'acosmi' &&
          getCachedModelCapabilities(model)?.supports_web_search !== true
        ) {
          return await callLocalSearch(
            input,
            startTime,
            budget.signal,
            onProgress,
          )
        }
      }

      // ── Mode A: Upstream server tool (existing logic) ──
      return await runUpstreamServerToolSearch(
        input,
        context,
        startTime,
        budget.signal,
        onProgress,
      )
    } catch (err) {
      // Mode A 的静默腰斩已在 runUpstreamServerToolSearch 内判定并抛出, 原样放行。
      if (err instanceof Error && err.name === WEB_SEARCH_BUDGET_ERROR_NAME) {
        throw err
      }
      // Mode B 走的是 fetch, 预算到期时抛的是裸 AbortError —— 翻译成可辨识错误。
      // 注意 provider 自己那 30s 内层预算 abort 的是另一条 signal, 因此不会被误判。
      if (isWebSearchBudgetExpiry(budget.signal, callerSignal)) {
        throw webSearchBudgetExhaustedError(err)
      }
      throw err
    } finally {
      // return / throw 两条出口都经过这里, 定时器与监听器不泄漏。
      budget.cleanup()
    }
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
