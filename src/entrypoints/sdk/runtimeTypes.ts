/**
 * SDK Runtime Types - Non-serializable types for the SDK public API.
 *
 * This file contains types for values that cannot be expressed as Zod schemas:
 * callbacks, class instances, generics, and other runtime-only constructs.
 */

import type { z } from 'zod/v4'
import type { SDKMessage, SDKResultMessage, SDKSessionInfo, SDKUserMessage } from './coreTypes.js'

// ============================================================================
// Effort Types
// ============================================================================

/** Named effort levels supported by the API. */
export type EffortLevel = 'low' | 'medium' | 'high' | 'max'

// ============================================================================
// Zod Utility Types
// ============================================================================

/** Any Zod raw shape (used for tool input schema definitions). */
export type AnyZodRawShape = z.ZodRawShape

/** Infer the TypeScript type from a Zod raw shape. */
export type InferShape<T extends AnyZodRawShape> = z.infer<z.ZodObject<T>>

// ============================================================================
// MCP Server Config With Instance
// ============================================================================

/** MCP server config that includes a live SDK server instance (non-serializable). */
export type McpSdkServerConfigWithInstance = {
  type: 'sdk'
  name: string
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  server: any
}

// ============================================================================
// MCP Tool Definition
// ============================================================================

/** An MCP tool definition created by the `tool()` helper. */
export type SdkMcpToolDefinition<Schema extends AnyZodRawShape = AnyZodRawShape> = {
  name: string
  description: string
  inputSchema: Schema
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  handler: (args: InferShape<Schema>, extra: any) => Promise<any>
  extras?: {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    annotations?: any
    searchHint?: string
    alwaysLoad?: boolean
  }
}

// ============================================================================
// Query / Options Types (V1 API)
// ============================================================================

/** Options accepted by the `query()` function (public API). */
export type Options = {
  /** Absolute path to the working directory */
  cwd?: string
  /** Model to use */
  model?: string
  /** Abort signal */
  signal?: AbortSignal
  /** Maximum number of agentic turns */
  maxTurns?: number
  /** System prompt override */
  systemPrompt?: string
  /** Append to system prompt */
  appendSystemPrompt?: string
  /** Permission mode */
  permissionMode?: string
  /** API key */
  apiKey?: string
  /** Additional custom options */
  [key: string]: unknown
}

/** @internal Internal options accepted by query(). */
export type InternalOptions = Options & {
  /** @internal */
  _internal?: unknown
}

/** Async iterable stream of SDK messages returned by query(). */
export type Query = AsyncIterable<SDKMessage>

/** @internal Internal query type. */
export type InternalQuery = AsyncIterable<SDKMessage>

// ============================================================================
// V2 Session API Types
// ============================================================================

/** Options for creating a new SDK session. */
export type SDKSessionOptions = {
  /** Absolute path to the working directory */
  cwd?: string
  /** Model to use */
  model?: string
  /** Abort signal */
  signal?: AbortSignal
  /** Maximum number of agentic turns */
  maxTurns?: number
  /** System prompt override */
  systemPrompt?: string
  /** Append to system prompt */
  appendSystemPrompt?: string
  /** Permission mode */
  permissionMode?: string
  /** API key */
  apiKey?: string
  /** Additional custom options */
  [key: string]: unknown
}

/**
 * A persistent V2 SDK session for multi-turn conversations.
 * @alpha
 */
export type SDKSession = {
  /** Send a message and receive a stream of SDK messages in return. */
  query(
    prompt: string | AsyncIterable<SDKUserMessage>,
    options?: Partial<SDKSessionOptions>,
  ): Query
  /** Interrupt the currently running conversation turn. */
  interrupt(): void
  /** Abort the session entirely. */
  abort(): void
}

// ============================================================================
// Session Management Types
// ============================================================================

/** Options for listSessions(). */
export type ListSessionsOptions = {
  /** Project directory to filter sessions by. Omit for all projects. */
  dir?: string
  /** Maximum number of sessions to return. */
  limit?: number
  /** Number of sessions to skip (for pagination). */
  offset?: number
}

/** Options for getSessionInfo(). */
export type GetSessionInfoOptions = {
  /** Project directory to search. Omit to search all projects. */
  dir?: string
}

/** Options for getSessionMessages(). */
export type GetSessionMessagesOptions = {
  /** Project directory to search. Omit to search all projects. */
  dir?: string
  /** Maximum number of messages to return. */
  limit?: number
  /** Number of messages to skip (for pagination). */
  offset?: number
  /** Whether to include system messages. Default: false. */
  includeSystemMessages?: boolean
}

/** Options for session mutation operations (rename, tag). */
export type SessionMutationOptions = {
  /** Project directory to search. Omit to search all projects. */
  dir?: string
}

/** Options for forkSession(). */
export type ForkSessionOptions = {
  /** Project directory. Omit to search all projects. */
  dir?: string
  /** Fork up to (and including) this message UUID. */
  upToMessageId?: string
  /** Title for the forked session. */
  title?: string
}

/** Result of forkSession(). */
export type ForkSessionResult = {
  /** UUID of the new forked session. */
  sessionId: string
}

/** A single message entry from a session transcript. */
export type SessionMessage = {
  /** Message UUID. */
  uuid: string
  /** The raw SDK message payload. */
  message: SDKMessage
}
