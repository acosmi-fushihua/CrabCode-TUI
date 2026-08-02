/**
 * Centralized API type definitions.
 *
 * Phase 4: All types defined locally — zero @acosmi-ai/sdk dependency.
 * These mirror the Acosmi API wire format shapes that the codebase uses.
 *
 * Consumer files import from this module: `import type { X } from 'src/types/api-types.js'`
 */

// ── Supporting types ────────────────────────────────────────────────────────

export interface CacheControlEphemeral {
  type: 'ephemeral'
  ttl?: '5m' | '1h'
}

// ── Non-beta content blocks ─────────────────────────────────────────────────

export interface TextBlockParam {
  text: string
  type: 'text'
  cache_control?: CacheControlEphemeral | null
  citations?: unknown[] | null
}

export interface ImageBlockParam {
  source: Base64ImageSource | { type: 'url'; url: string }
  type: 'image'
  cache_control?: CacheControlEphemeral | null
}

export interface Base64ImageSource {
  data: string
  media_type: 'image/jpeg' | 'image/png' | 'image/gif' | 'image/webp'
  type: 'base64'
}

export interface ThinkingBlock {
  signature: string
  thinking: string
  type: 'thinking'
}

export interface ThinkingBlockParam {
  signature: string
  thinking: string
  type: 'thinking'
}

export interface RedactedThinkingBlock {
  data: string
  type: 'redacted_thinking'
}

export interface RedactedThinkingBlockParam {
  data: string
  type: 'redacted_thinking'
}

export interface ToolUseBlock {
  id: string
  input: unknown
  name: string
  type: 'tool_use'
  caller?: unknown
}

export interface ToolUseBlockParam {
  id: string
  input: unknown
  name: string
  type: 'tool_use'
  cache_control?: CacheControlEphemeral | null
  caller?: unknown
}

export interface ToolResultBlockParam {
  tool_use_id: string
  type: 'tool_result'
  cache_control?: CacheControlEphemeral | null
  content?: string | Array<TextBlockParam | ImageBlockParam>
  is_error?: boolean
}

/** Union of all response content block types. */
export type ContentBlock =
  | BetaTextBlock
  | ThinkingBlock
  | RedactedThinkingBlock
  | ToolUseBlock
  | BetaServerToolUseBlock
  | BetaWebSearchToolResultBlock
  | BetaWebFetchToolResultBlock
  | BetaCodeExecutionToolResultBlock
  | BetaBashCodeExecutionToolResultBlock
  | BetaTextEditorCodeExecutionToolResultBlock
  | BetaToolSearchToolResultBlock
  | BetaMCPToolUseBlock
  | BetaMCPToolResultBlock
  | BetaContainerUploadBlock
  | BetaCompactionBlock

export interface DocumentBlockParam {
  type: 'document'
  source: { type: 'base64'; media_type: string; data: string } | { type: 'url'; url: string }
  cache_control?: CacheControlEphemeral | null
}

/** Union of all request content block param types. */
export type ContentBlockParam =
  | TextBlockParam
  | ImageBlockParam
  | DocumentBlockParam
  | ThinkingBlockParam
  | RedactedThinkingBlockParam
  | ToolUseBlockParam
  | ToolResultBlockParam
  | ContentBlock

export interface MessageParam {
  content: string | Array<ContentBlockParam>
  role: 'user' | 'assistant'
}

// ── Beta content blocks ─────────────────────────────────────────────────────

export interface BetaThinkingBlock {
  signature: string
  thinking: string
  type: 'thinking'
}

export interface BetaRedactedThinkingBlock {
  data: string
  type: 'redacted_thinking'
}

export interface BetaToolUseBlock {
  id: string
  input: unknown
  name: string
  type: 'tool_use'
  caller?: unknown
}

export interface BetaToolResultBlockParam {
  tool_use_id: string
  type: 'tool_result'
  cache_control?: CacheControlEphemeral | null
  content?: string | Array<unknown>
  is_error?: boolean
}

export interface BetaImageBlockParam {
  source: Base64ImageSource | { type: 'url'; url: string } | { type: 'file'; file_id: string }
  type: 'image'
  cache_control?: CacheControlEphemeral | null
}

export interface BetaTextBlock {
  type: 'text'
  text: string
  citations?: unknown[] | null
}

export interface BetaServerToolUseBlock {
  type: 'server_tool_use'
  id: string
  name: string
  input: Record<string, unknown>
  caller?: unknown
}

export interface BetaWebSearchToolResultBlock {
  type: 'web_search_tool_result'
  tool_use_id: string
  content: unknown
  caller?: unknown
}

export interface BetaWebFetchToolResultBlock {
  type: 'web_fetch_tool_result'
  tool_use_id: string
  content: unknown
  caller?: unknown
}

export interface BetaCodeExecutionToolResultBlock {
  type: 'code_execution_tool_result'
  tool_use_id: string
  content: unknown
}

export interface BetaBashCodeExecutionToolResultBlock {
  type: 'bash_code_execution_tool_result'
  tool_use_id: string
  content: unknown
}

export interface BetaTextEditorCodeExecutionToolResultBlock {
  type: 'text_editor_code_execution_tool_result'
  tool_use_id: string
  content: unknown
}

export interface BetaToolSearchToolResultBlock {
  type: 'tool_search_tool_result'
  tool_use_id: string
  content: unknown
}

export interface BetaMCPToolUseBlock {
  type: 'mcp_tool_use'
  id: string
  name: string
  input: unknown
  server_name: string
}

export interface BetaMCPToolResultBlock {
  type: 'mcp_tool_result'
  tool_use_id: string
  content: string | Array<BetaTextBlock>
  is_error: boolean
}

export interface BetaContainerUploadBlock {
  type: 'container_upload'
  file_id: string
}

export interface BetaCompactionBlock {
  type: 'compaction'
  content: string | null
}

/** Union of beta response content block types. */
export type BetaContentBlock =
  | BetaTextBlock
  | BetaThinkingBlock
  | BetaRedactedThinkingBlock
  | BetaToolUseBlock
  | BetaServerToolUseBlock
  | BetaWebSearchToolResultBlock
  | BetaWebFetchToolResultBlock
  | BetaCodeExecutionToolResultBlock
  | BetaBashCodeExecutionToolResultBlock
  | BetaTextEditorCodeExecutionToolResultBlock
  | BetaToolSearchToolResultBlock
  | BetaMCPToolUseBlock
  | BetaMCPToolResultBlock
  | BetaContainerUploadBlock
  | BetaCompactionBlock

/** Union of beta request content block param types. */
export type BetaContentBlockParam =
  | TextBlockParam
  | BetaImageBlockParam
  | DocumentBlockParam
  | ThinkingBlockParam
  | RedactedThinkingBlockParam
  | ToolUseBlockParam
  | BetaToolResultBlockParam
  | BetaContentBlock

// ── Beta messages ───────────────────────────────────────────────────────────

export interface ServerToolUseUsage {
  web_search_requests?: number | null
  web_fetch_requests?: number | null
}

export interface CacheCreationUsage {
  ephemeral_1h_input_tokens?: number | null
  ephemeral_5m_input_tokens?: number | null
}

export interface BetaUsage {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number | null
  cache_read_input_tokens: number | null
  cache_creation?: CacheCreationUsage | null
  inference_geo?: string | null
  iterations?: unknown | null
  server_tool_use?: ServerToolUseUsage | null
  service_tier?: string | null
  speed?: string | null
}

export interface BetaMessageDeltaUsage {
  output_tokens: number
  input_tokens?: number | null
  cache_creation_input_tokens?: number | null
  cache_read_input_tokens?: number | null
  cache_creation?: CacheCreationUsage | null
  iterations?: unknown | null
  server_tool_use?: ServerToolUseUsage | null
}

export type BetaStopReason =
  | 'end_turn'
  | 'max_tokens'
  | 'stop_sequence'
  | 'tool_use'
  | 'pause_turn'
  | 'compaction'
  | 'refusal'
  | 'model_context_window_exceeded'

export interface BetaMessage {
  id: string
  content: Array<BetaContentBlock>
  model: string
  role: 'assistant'
  stop_reason: BetaStopReason | null
  stop_sequence: string | null
  type: 'message'
  usage: BetaUsage
  container?: unknown | null
  context_management?: unknown | null
  stop_details?: unknown | null
}

export interface BetaMessageParam {
  content: string | Array<BetaContentBlockParam>
  role: 'user' | 'assistant'
}

/** Parameters for creating a (streaming) message. */
export interface BetaMessageStreamParams {
  model: string
  messages: Array<BetaMessageParam>
  max_tokens: number
  system?: string | Array<TextBlockParam>
  tools?: Array<BetaToolUnion>
  tool_choice?: BetaToolChoiceAuto | BetaToolChoiceTool | { type: 'any' } | { type: 'none' }
  betas?: string[]
  metadata?: Record<string, unknown>
  temperature?: number
  thinking?: { type: string; budget_tokens?: number; display?: string | null }
  stop_sequences?: string[]
  stream?: boolean
  [key: string]: unknown // extensible for extra body params
}

/** SSE stream event union — the events yielded by a streaming message. */
export type BetaRawMessageStreamEvent =
  | { type: 'message_start'; message: BetaMessage }
  | { type: 'message_delta'; delta: { stop_reason?: BetaStopReason; stop_sequence?: string | null }; usage: BetaMessageDeltaUsage }
  | { type: 'message_stop' }
  | { type: 'content_block_start'; index: number; content_block: BetaContentBlock }
  | { type: 'content_block_delta'; index: number; delta: Record<string, unknown> }
  | { type: 'content_block_stop'; index: number }
  | { type: 'ping' }
  // V116.1 P1-4 (2026-07-24): anthropic 协议流内错误帧(官方协议自带;此前
  // 本地联合缺失,queryModel switch 无从匹配)。顶层 errorCode 为 nexus 网关
  // V116.1 扩展字段,上游原生帧无。
  | {
      type: 'error'
      error?: { type?: string; message?: string }
      errorCode?: string
    }

// ── Beta tools ──────────────────────────────────────────────────────────────

export interface BetaTool {
  input_schema: BetaTool.InputSchema
  name: string
  allowed_callers?: string[]
  cache_control?: CacheControlEphemeral | null
  defer_loading?: boolean
  description?: string
  eager_input_streaming?: boolean | null
  input_examples?: Array<Record<string, unknown>>
  strict?: boolean
  type?: 'custom' | null
}

export namespace BetaTool {
  export interface InputSchema {
    type: 'object'
    properties?: unknown | null
    required?: string[] | readonly string[] | null
    [key: string]: unknown
  }
}

export interface BetaToolChoiceAuto {
  type: 'auto'
  disable_parallel_tool_use?: boolean
}

export interface BetaToolChoiceTool {
  name: string
  type: 'tool'
  disable_parallel_tool_use?: boolean
}

export interface BetaWebSearchTool20250305 {
  name: 'web_search'
  type: 'web_search_20250305'
  allowed_callers?: string[]
  allowed_domains?: string[] | null
  blocked_domains?: string[] | null
  cache_control?: CacheControlEphemeral | null
  defer_loading?: boolean
  max_uses?: number | null
  strict?: boolean
  user_location?: unknown | null
}

/** Union of all tool types accepted in a beta messages request. */
export type BetaToolUnion =
  | BetaTool
  | BetaWebSearchTool20250305
  | Record<string, unknown> // extensible for bash, computer_use, text_editor, mcp, etc.

// ── Stream ──────────────────────────────────────────────────────────────────

/**
 * Async iterable stream with an abort controller.
 * Replaces `import type { Stream } from '@acosmi-ai/sdk/streaming.mjs'`.
 */
export interface Stream<Item> {
  controller: AbortController
  [Symbol.asyncIterator](): AsyncIterator<Item>
}

// ── Client types ────────────────────────────────────────────────────────────

export interface ClientOptions {
  apiKey?: string | null
  authToken?: string | null
  baseURL?: string | null
  timeout?: number
  fetch?: unknown
  maxRetries?: number
  defaultHeaders?: Record<string, string>
  defaultQuery?: Record<string, string | undefined>
  [key: string]: unknown
}

/**
 * Client instance type — allows `Acosmi` to be used as a type (e.g. `client: Acosmi`).
 * Merges with the namespace below via declaration merging.
 */
export interface Acosmi {
  [key: string]: unknown
}

/**
 * Namespace type for backward compatibility with `Acosmi.X.Y.Z` usage
 * across the codebase. Replaces `import type { default as Acosmi } from '@acosmi-ai/sdk'`.
 */
export namespace Acosmi {
  // Top-level types
  export type MessageParam = import('./api-types.js').MessageParam
  export type TextBlockParam = import('./api-types.js').TextBlockParam
  export type ImageBlockParam = import('./api-types.js').ImageBlockParam
  export type ContentBlock = import('./api-types.js').ContentBlock
  export type ContentBlockParam = import('./api-types.js').ContentBlockParam

  // Tool types
  export interface Tool {
    input_schema: Tool.InputSchema
    name: string
    allowed_callers?: string[]
    cache_control?: CacheControlEphemeral | null
    defer_loading?: boolean
    description?: string
    strict?: boolean
    type?: 'custom' | null
  }
  export namespace Tool {
    export interface InputSchema {
      type: 'object'
      properties?: unknown | null
      required?: string[] | readonly string[] | null
      [key: string]: unknown
    }
  }

  export type ToolChoice =
    | { type: 'auto'; disable_parallel_tool_use?: boolean }
    | { type: 'any'; disable_parallel_tool_use?: boolean }
    | { type: 'tool'; name: string; disable_parallel_tool_use?: boolean }
    | { type: 'none' }

  // Beta namespace
  export namespace Beta {
    export namespace Messages {
      export type BetaMessage = import('./api-types.js').BetaMessage
      export type BetaMessageParam = import('./api-types.js').BetaMessageParam
      export type BetaContentBlock = import('./api-types.js').BetaContentBlock
      export type BetaContentBlockParam = import('./api-types.js').BetaContentBlockParam
      export type BetaToolUnion = import('./api-types.js').BetaToolUnion
      export type BetaToolUseBlock = import('./api-types.js').BetaToolUseBlock
      export type BetaToolUseBlockParam = import('./api-types.js').ToolUseBlockParam
      export type BetaToolResultBlockParam = import('./api-types.js').BetaToolResultBlockParam
      export type BetaUsage = import('./api-types.js').BetaUsage
      export type BetaStopReason = import('./api-types.js').BetaStopReason
      export type BetaThinkingConfigParam =
        | { type: 'enabled'; budget_tokens: number; display?: string | null }
        | { type: 'disabled' }
        | { type: 'adaptive'; display?: string | null }
    }
  }
}

// V116.1 P1-3 (2026-07-24):本地 APIError 分类族已删除(全仓零构造点死代码,见
// errors/api-errors.ts 模块头)。遗留消费面(SystemAPIErrorMessage.error /
// formatAPIError 等)改以结构鸭子形态接错误对象 —— 运行时实际是 SDK HTTPError、
// 本地传输哨兵类(APIConnectionError/APIUserAbortError)或普通 Error。
export type APIError = Error & {
  status?: number
  /** 解析后的响应体(旧 APIError 形态;HTTPError 上不存在,消费方须空容忍) */
  error?: unknown
  headers?: Headers
  requestID?: string | null
}
