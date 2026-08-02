import type {
  BetaContentBlock,
  BetaContentBlockParam,
  BetaMessage,
  BetaMessageParam,
  BetaMessageStreamParams,
  BetaRawMessageStreamEvent,
  BetaStopReason,
  BetaToolResultBlockParam,
  BetaToolUnion,
  BetaUsage,
  TextBlockParam,
  ToolUseBlock,
} from '../../types/api-types.js'
import { logForDebugging } from '../debug.js'

export type CustomModelProtocol =
  | 'anthropic-compatible'
  | 'openai-compatible'

export interface OpenAIToolCall {
  id: string
  type: 'function'
  function: {
    name: string
    arguments: string
  }
}

export interface OpenAIChatMessage {
  role: 'system' | 'user' | 'assistant' | 'tool'
  content?: string | null | Array<Record<string, unknown>>
  tool_call_id?: string
  tool_calls?: OpenAIToolCall[]
}

export interface OpenAIChatCompletionRequest {
  model: string
  messages: OpenAIChatMessage[]
  max_tokens: number
  stream?: boolean
  /**
   * W-CUSTOM-MODEL-PLUS-GATE PR-5（审计 F1）：不带 `include_usage` 时多数
   * OpenAI 兼容端点的流式响应不回 usage → 上层 token 记账恒 0。请求侧默认带；
   * 个别 strict 校验器对该字段 400 时由 fetch 层做一次性去参重试。
   */
  stream_options?: { include_usage: boolean }
  temperature?: number
  stop?: string[]
  tools?: Array<Record<string, unknown>>
  tool_choice?: 'auto' | 'none' | 'required' | Record<string, unknown>
  response_format?: unknown
  metadata?: Record<string, unknown>
}

export interface OpenAIChatCompletion {
  id: string
  model: string
  choices: Array<{
    message: {
      role: 'assistant'
      content?: string | null
      tool_calls?: OpenAIToolCall[]
    }
    finish_reason?: string | null
  }>
  usage?: {
    prompt_tokens?: number
    completion_tokens?: number
    total_tokens?: number
  }
}

export interface OpenAIChatCompletionChunk {
  id?: string
  model?: string
  choices: Array<{
    delta?: {
      role?: 'assistant'
      content?: string | null
      /**
       * PR-5（审计 F2 / 裁决③ 响应侧）：推理增量。`reasoning_content` 是
       * DeepSeek 系/多数国产端点的字段名，`reasoning` 是部分聚合器的变体。
       * 无条件解析（接收宽容）；请求侧按裁决③不发送任何 reasoning 开关。
       */
      reasoning_content?: string | null
      reasoning?: string | null
      tool_calls?: Array<{
        index: number
        id?: string
        type?: 'function'
        function?: {
          name?: string
          arguments?: string
        }
      }>
    }
    finish_reason?: string | null
  }>
  usage?: {
    prompt_tokens?: number
    completion_tokens?: number
  } | null
}

export interface AnthropicCompatibleError {
  type: 'error'
  error: {
    type: string
    message: string
  }
}

export function customModelRequestToProviderRequest(
  protocol: CustomModelProtocol,
  params: BetaMessageStreamParams,
): BetaMessageStreamParams | OpenAIChatCompletionRequest {
  if (protocol === 'anthropic-compatible') return params
  return anthropicToOpenAIChatCompletionRequest(params)
}

export function anthropicToOpenAIChatCompletionRequest(
  params: BetaMessageStreamParams,
): OpenAIChatCompletionRequest {
  const messages: OpenAIChatMessage[] = []
  const system = renderSystemPrompt(params.system)
  if (system) messages.push({ role: 'system', content: system })

  for (const message of params.messages) {
    messages.push(...anthropicMessageToOpenAIMessages(message))
  }

  const request: OpenAIChatCompletionRequest = {
    model: params.model,
    messages,
    max_tokens: params.max_tokens,
  }

  if (params.stream !== undefined) request.stream = params.stream
  // F1：流式请求默认索要 usage（custom 链在转换前置 stream:true 即命中；local
  // 链在转换后自设 stream 与 stream_options，两链行为一致）。
  if (params.stream) request.stream_options = { include_usage: true }
  if (params.temperature !== undefined) request.temperature = params.temperature
  if (params.stop_sequences?.length) request.stop = params.stop_sequences
  if (params.metadata) request.metadata = params.metadata
  if (params.tools?.length) {
    // F5：转换后可能为空（全是 server-side 形态）——只在非空时落字段，空数组
    // `tools: []` 会被部分 strict 端点拒绝。
    const tools = params.tools.flatMap(openAIToolFromAnthropicTool)
    if (tools.length > 0) request.tools = tools
  }
  if (params.tool_choice) request.tool_choice = openAIToolChoiceFromAnthropic(params.tool_choice)
  if (params.response_format) request.response_format = params.response_format

  return request
}

export function openAICompletionToAnthropicMessage(
  response: OpenAIChatCompletion,
): BetaMessage {
  const choice = response.choices[0]
  const message = choice?.message
  const content: BetaContentBlock[] = []

  if (message?.content) {
    content.push({ type: 'text', text: message.content })
  }

  for (const toolCall of message?.tool_calls ?? []) {
    content.push(openAIToolCallToAnthropicBlock(toolCall))
  }

  return {
    id: response.id,
    type: 'message',
    role: 'assistant',
    model: response.model,
    content,
    stop_reason: openAIFinishReasonToAnthropic(choice?.finish_reason),
    stop_sequence: null,
    usage: openAIUsageToAnthropicUsage(response.usage),
  }
}

export class OpenAIStreamToAnthropicAdapter {
  #messageStarted = false
  #messageEnded = false
  #nextContentIndex = 0
  #openTextIndex: number | null = null
  /** PR-5（F2）：进行中的 thinking block 索引（reasoning 增量聚合于此）。 */
  #openThinkingIndex: number | null = null
  #openToolIndices = new Map<number, number>()
  #openContentIndices = new Set<number>()
  #messageId = 'openai-compatible-message'
  #model = 'custom-model'
  #lastUsage: OpenAIChatCompletionChunk['usage'] = undefined
  #pendingStopReason: BetaStopReason | undefined = undefined

  accept(chunk: OpenAIChatCompletionChunk): BetaRawMessageStreamEvent[] {
    const events: BetaRawMessageStreamEvent[] = []
    if (this.#messageEnded) return events
    if (!Array.isArray(chunk.choices)) {
      throw new Error(formatOpenAIStreamPayloadError(chunk))
    }
    if (chunk.id) this.#messageId = chunk.id
    if (chunk.model) this.#model = chunk.model
    if (chunk.usage) this.#lastUsage = chunk.usage

    if (chunk.choices.length === 0) {
      if (this.#pendingStopReason !== undefined) {
        this.#emitTerminalEvents(events)
      }
      return events
    }

    if (!this.#messageStarted) {
      this.#messageStarted = true
      events.push({
        type: 'message_start',
        message: {
          id: this.#messageId,
          type: 'message',
          role: 'assistant',
          model: this.#model,
          content: [],
          stop_reason: null,
          stop_sequence: null,
          usage: openAIUsageToAnthropicUsage(chunk.usage ?? undefined, true),
        },
      })
    }

    for (const choice of chunk.choices) {
      const delta = choice.delta
      // PR-5（F2 / 裁决③ 响应侧）：reasoning 增量 → Anthropic thinking block。
      // DeepSeek 系把思考放 `reasoning_content`，部分聚合器用 `reasoning`；两者
      // 无条件解析。thinking block 在首个非 reasoning 增量（正文/工具）或
      // finish 处关闭 —— 与 Anthropic 原生流 thinking-先行 的块序一致。
      const reasoningDelta =
        (typeof delta?.reasoning_content === 'string' && delta.reasoning_content) ||
        (typeof delta?.reasoning === 'string' && delta.reasoning) ||
        ''
      if (reasoningDelta) {
        if (this.#openThinkingIndex === null) {
          this.#openThinkingIndex = this.#openContentBlock(events, {
            type: 'thinking',
            thinking: '',
            signature: '',
          } as unknown as BetaContentBlock)
        }
        events.push({
          type: 'content_block_delta',
          index: this.#openThinkingIndex,
          delta: {
            type: 'thinking_delta',
            thinking: reasoningDelta,
          } as unknown as Extract<BetaRawMessageStreamEvent, { type: 'content_block_delta' }>['delta'],
        })
      }
      if (delta?.content) {
        this.#closeThinkingBlock(events)
        if (this.#openTextIndex === null) {
          this.#openTextIndex = this.#openContentBlock(events, {
            type: 'text',
            text: '',
          })
        }
        events.push({
          type: 'content_block_delta',
          index: this.#openTextIndex,
          delta: { type: 'text_delta', text: delta.content },
        })
      }
      if ((delta?.tool_calls ?? []).length > 0) {
        this.#closeThinkingBlock(events)
      }

      for (const toolDelta of delta?.tool_calls ?? []) {
        let index = this.#openToolIndices.get(toolDelta.index)
        if (index === undefined) {
          index = this.#openContentBlock(events, {
            type: 'tool_use',
            id: toolDelta.id ?? `toolu_${toolDelta.index}`,
            name: toolDelta.function?.name ?? 'tool',
            input: {},
          })
          this.#openToolIndices.set(toolDelta.index, index)
        }
        const partialJson = toolDelta.function?.arguments
        if (partialJson) {
          events.push({
            type: 'content_block_delta',
            index,
            delta: { type: 'input_json_delta', partial_json: partialJson },
          })
        }
      }

      if (choice.finish_reason) {
        this.#pendingStopReason =
          openAIFinishReasonToAnthropic(choice.finish_reason) ?? undefined
        this.#closeOpenContentBlocks(events)
        if (chunk.usage) this.#emitTerminalEvents(events)
      }
    }

    return events
  }

  /**
   * Close out the stream when the SSE body ends without ever sending a
   * `finish_reason` (LOW-6). llama-server normally sends one, but a
   * bring-your-own OpenAI-compatible provider may not — without this the
   * Anthropic raw stream never closes (no `content_block_stop` /
   * `message_stop`) and the consumer hangs.
   *
   * Idempotent: a no-op once `message_stop` has already been emitted, whether
   * by a `finish_reason` chunk or a prior `finalize()` call.
   */
  finalize(): BetaRawMessageStreamEvent[] {
    if (this.#messageEnded) return []
    const events: BetaRawMessageStreamEvent[] = []
    if (!this.#messageStarted) {
      this.#messageStarted = true
      events.push({
        type: 'message_start',
        message: {
          id: this.#messageId,
          type: 'message',
          role: 'assistant',
          model: this.#model,
          content: [],
          stop_reason: null,
          stop_sequence: null,
          usage: openAIUsageToAnthropicUsage(this.#lastUsage ?? undefined, true),
        },
      })
    }
    this.#closeOpenContentBlocks(events)
    this.#emitTerminalEvents(events)
    return events
  }

  #openContentBlock(
    events: BetaRawMessageStreamEvent[],
    contentBlock: BetaContentBlock,
  ): number {
    const index = this.#nextContentIndex++
    this.#openContentIndices.add(index)
    events.push({
      type: 'content_block_start',
      index,
      content_block: contentBlock,
    })
    return index
  }

  /** PR-5（F2）：首个非 reasoning 增量到达时关闭进行中的 thinking block。 */
  #closeThinkingBlock(events: BetaRawMessageStreamEvent[]): void {
    if (this.#openThinkingIndex === null) return
    events.push({ type: 'content_block_stop', index: this.#openThinkingIndex })
    this.#openContentIndices.delete(this.#openThinkingIndex)
    this.#openThinkingIndex = null
  }

  #closeOpenContentBlocks(events: BetaRawMessageStreamEvent[]): void {
    for (const index of [...this.#openContentIndices].sort((a, b) => a - b)) {
      events.push({ type: 'content_block_stop', index })
    }
    this.#openContentIndices.clear()
    this.#openTextIndex = null
    this.#openThinkingIndex = null
    this.#openToolIndices.clear()
  }

  #emitTerminalEvents(events: BetaRawMessageStreamEvent[]): void {
    events.push({
      type: 'message_delta',
      delta: { stop_reason: this.#pendingStopReason ?? 'end_turn' },
      usage: {
        output_tokens: this.#lastUsage?.completion_tokens ?? 0,
        input_tokens: this.#lastUsage?.prompt_tokens ?? null,
        cache_creation_input_tokens: null,
        cache_read_input_tokens: null,
      },
    })
    events.push({ type: 'message_stop' })
    this.#messageEnded = true
  }
}

function formatOpenAIStreamPayloadError(payload: unknown): string {
  const obj = isRecord(payload) ? payload : {}
  const nested = isRecord(obj.error) ? obj.error : obj
  const message =
    typeof nested.message === 'string'
      ? nested.message
      : 'OpenAI-compatible provider returned a stream payload without choices'
  const type =
    typeof nested.type === 'string'
      ? nested.type
      : typeof nested.code === 'string'
        ? nested.code
        : 'invalid_stream_payload'
  return `OpenAI-compatible provider stream error (${type}): ${message}`
}

export function openAIErrorToAnthropicError(error: unknown): AnthropicCompatibleError {
  const obj = isRecord(error) ? error : {}
  const nested = isRecord(obj.error) ? obj.error : obj
  const message =
    typeof nested.message === 'string'
      ? nested.message
      : typeof obj.message === 'string'
        ? obj.message
        : 'Custom model provider error'
  const type =
    typeof nested.type === 'string'
      ? nested.type
      : typeof obj.type === 'string'
        ? obj.type
        : 'api_error'

  return {
    type: 'error',
    error: {
      type,
      message,
    },
  }
}

function anthropicMessageToOpenAIMessages(
  message: BetaMessageParam,
): OpenAIChatMessage[] {
  if (typeof message.content === 'string') {
    return [{ role: message.role, content: message.content }]
  }

  if (message.role === 'assistant') {
    const text = message.content
      .filter(isTextBlock)
      .map(block => block.text)
      .join('')
    const toolCalls = message.content
      .filter(isToolUseBlock)
      .map(block => ({
        id: block.id,
        type: 'function' as const,
        function: {
          name: block.name,
          arguments: stringifyToolInput(block.input),
        },
      }))
    return [
      {
        role: 'assistant',
        content: text || null,
        ...(toolCalls.length ? { tool_calls: toolCalls } : {}),
      },
    ]
  }

  const userMessages: OpenAIChatMessage[] = []
  const userContent = renderOpenAIUserContent(message.content)
  if (userContent !== null) {
    userMessages.push({ role: 'user', content: userContent })
  }
  for (const block of message.content.filter(isToolResultBlock)) {
    // PR-5（F4）：OpenAI 世界的 tool 消息不支持图片。图片在 tool 消息内留
    // `[image]` 文本占位，真图随后以一条 user 消息携 `image_url` 补发（通行
    // 折中——模型能看到图，只是署名从 tool 变 user）。纯文本行为不变。
    const { text, imageUrls } = renderToolResultContentParts(block)
    userMessages.push({
      role: 'tool',
      tool_call_id: block.tool_use_id,
      content: text,
    })
    if (imageUrls.length > 0) {
      userMessages.push({
        role: 'user',
        content: [
          {
            type: 'text',
            text: `[images from tool result ${block.tool_use_id}]`,
          },
          ...imageUrls.map(url => ({
            type: 'image_url',
            image_url: { url },
          })),
        ],
      })
    }
  }
  return userMessages
}

function renderSystemPrompt(
  system: BetaMessageStreamParams['system'],
): string | null {
  if (!system) return null
  if (typeof system === 'string') return system
  return system.filter(isTextBlock).map(block => block.text).join('\n')
}

function renderOpenAIUserContent(
  content: Array<BetaContentBlockParam>,
): string | Array<Record<string, unknown>> | null {
  const parts: Array<Record<string, unknown>> = []
  const text = content.filter(isTextBlock).map(block => block.text).join('')
  if (text) parts.push({ type: 'text', text })
  for (const block of content) {
    if (isImageBlock(block)) {
      const url = imageBlockToUrl(block)
      if (!url) continue
      parts.push({ type: 'image_url', image_url: { url } })
    }
  }
  if (parts.length === 0) return null
  if (parts.length === 1 && parts[0].type === 'text') {
    return String(parts[0].text)
  }
  return parts
}

/** Shared image-source → URL rendering (data: URI for base64 sources). */
function imageBlockToUrl(
  block: Extract<BetaContentBlockParam, { type: 'image' }>,
): string | null {
  return block.source.type === 'url'
    ? block.source.url
    : block.source.type === 'base64'
      ? `data:${block.source.media_type};base64,${block.source.data}`
      : null
}

/**
 * PR-5（F4）：tool_result 内容拆成 tool 消息文本 + 待补发的图片 URL。图片位置
 * 在文本里留 `[image]` 占位（此前是整块 JSON.stringify —— base64 图变一坨
 * 废字节直接塞进 tool 消息）。
 */
function renderToolResultContentParts(block: BetaToolResultBlockParam): {
  text: string
  imageUrls: string[]
} {
  if (typeof block.content === 'string') {
    return { text: block.content, imageUrls: [] }
  }
  if (!block.content) return { text: '', imageUrls: [] }
  const imageUrls: string[] = []
  const text = block.content
    .map(part => {
      if (isTextBlock(part)) return part.text
      if (isImageBlock(part)) {
        const url = imageBlockToUrl(part)
        if (url) imageUrls.push(url)
        return '[image]'
      }
      return JSON.stringify(part)
    })
    .join('\n')
  return { text, imageUrls }
}

function openAIToolFromAnthropicTool(
  tool: BetaToolUnion,
): Array<Record<string, unknown>> {
  // PR-5（F5）：真正无法转换的只有「没有 name」的形态（server-side tool 等）——
  // 丢弃但留一行 debug 日志（此前静默蒸发，模型直接少一件工具且无从排查）。
  if (!isRecord(tool) || typeof tool.name !== 'string') {
    logForDebugging(
      `customProtocolAdapter: dropping untranslatable tool (no name): ${safeToolLabel(tool)}`,
    )
    return []
  }
  // F5：有 name 无 input_schema 的工具（零参数工具的合法形态）补一个空对象
  // schema 保留 —— OpenAI 端 parameters 本就可为空对象。
  const parameters = isRecord(tool.input_schema)
    ? tool.input_schema
    : { type: 'object', properties: {} }
  return [
    {
      type: 'function',
      function: {
        name: tool.name,
        description: typeof tool.description === 'string' ? tool.description : undefined,
        parameters,
      },
    },
  ]
}

function safeToolLabel(tool: unknown): string {
  if (!isRecord(tool)) return String(tool)
  const type = typeof tool.type === 'string' ? tool.type : 'unknown-type'
  const name = typeof tool.name === 'string' ? tool.name : '(no name)'
  return `${type} ${name}`
}

function openAIToolChoiceFromAnthropic(
  toolChoice: NonNullable<BetaMessageStreamParams['tool_choice']>,
): OpenAIChatCompletionRequest['tool_choice'] {
  if (toolChoice.type === 'auto') return 'auto'
  if (toolChoice.type === 'none') return 'none'
  if (toolChoice.type === 'any') return 'required'
  if (toolChoice.type === 'tool') {
    return {
      type: 'function',
      function: { name: toolChoice.name },
    }
  }
  return 'auto'
}

function openAIToolCallToAnthropicBlock(toolCall: OpenAIToolCall): ToolUseBlock {
  return {
    type: 'tool_use',
    id: toolCall.id,
    name: toolCall.function.name,
    input: parseToolInput(toolCall.function.arguments),
  }
}

function openAIFinishReasonToAnthropic(
  finishReason: string | null | undefined,
): BetaStopReason | null {
  switch (finishReason) {
    case 'stop':
      return 'end_turn'
    case 'length':
      return 'max_tokens'
    case 'tool_calls':
    case 'function_call':
      return 'tool_use'
    case 'content_filter':
      return 'refusal'
    default:
      return null
  }
}

function openAIUsageToAnthropicUsage(
  usage: OpenAIChatCompletion['usage'] | OpenAIChatCompletionChunk['usage'] | undefined,
  messageStart = false,
): BetaUsage {
  return {
    input_tokens: usage?.prompt_tokens ?? 0,
    output_tokens: messageStart ? 0 : usage?.completion_tokens ?? 0,
    cache_creation_input_tokens: null,
    cache_read_input_tokens: null,
  }
}

function stringifyToolInput(input: unknown): string {
  try {
    return JSON.stringify(input ?? {})
  } catch {
    return '{}'
  }
}

function parseToolInput(input: string): unknown {
  try {
    return input ? JSON.parse(input) : {}
  } catch {
    return {}
  }
}

function isTextBlock(block: unknown): block is TextBlockParam {
  return isRecord(block) && block.type === 'text' && typeof block.text === 'string'
}

function isImageBlock(block: unknown): block is Extract<BetaContentBlockParam, { type: 'image' }> {
  return isRecord(block) && block.type === 'image' && isRecord(block.source)
}

function isToolUseBlock(block: unknown): block is ToolUseBlock {
  return isRecord(block) && block.type === 'tool_use'
}

function isToolResultBlock(block: unknown): block is BetaToolResultBlockParam {
  return isRecord(block) && block.type === 'tool_result'
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}
