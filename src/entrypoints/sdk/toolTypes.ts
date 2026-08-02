/**
 * SDK Tool Types - Types for tool definitions and tool execution.
 *
 * @internal All types in this file are internal until the SDK API stabilizes.
 */

import type { CallToolResult, ToolAnnotations } from '@modelcontextprotocol/sdk/types.js'
import type { AnyZodRawShape, InferShape, SdkMcpToolDefinition } from './runtimeTypes.js'

// Re-export MCP protocol tool types for SDK consumers
export type { CallToolResult, ToolAnnotations }

// Re-export tool-related runtime types
export type { AnyZodRawShape, InferShape, SdkMcpToolDefinition }

// ============================================================================
// Tool Handler Types
// ============================================================================

/** Handler function for an SDK MCP tool. */
export type SdkToolHandler<Schema extends AnyZodRawShape = AnyZodRawShape> = (
  args: InferShape<Schema>,
  extra: unknown,
) => Promise<CallToolResult>

/** Extra metadata for tool definitions. */
export type SdkToolExtras = {
  annotations?: ToolAnnotations
  searchHint?: string
  alwaysLoad?: boolean
}

/** Input type for a tool defined with the `tool()` helper. */
export type SdkToolInput<Schema extends AnyZodRawShape = AnyZodRawShape> = InferShape<Schema>

/** Content item within a CallToolResult. */
export type ToolResultContent =
  | { type: 'text'; text: string }
  | { type: 'image'; data: string; mimeType: string }
  | { type: 'resource'; resource: { uri: string; text?: string; mimeType?: string } }
