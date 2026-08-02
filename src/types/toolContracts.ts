/**
 * Cross-language ABI contract types extracted from Tool.ts.
 *
 * This file is the single source of truth for type shapes that must be
 * consistent across the three-language (TypeScript / Go / Rust) boundary.
 * Future codegen pipelines (ts-to-go, ts-to-rust) read from here.
 *
 * Rules:
 *   - Only plain, JSON-serializable shapes. No functions, no React, no Zod.
 *   - No `DeepImmutable` wrapper -- use explicit `readonly` modifiers instead.
 *   - `Set<string>` becomes `string[]` (JSON-safe).
 *   - Keep in sync with Tool.ts via re-import (Tool.ts imports from here and
 *     re-exports for backwards compatibility).
 *
 * Extracted: 2026-04-03
 */

import type { UUID } from 'crypto'

// ============================================================================
// Re-exports from sibling type files (part of the ABI surface)
// ============================================================================

export type {
  PermissionMode,
  PermissionBehavior,
  PermissionResult,
} from './permissions.js'

export type { ToolProgressData } from './tools.js'

export type { Message } from './message.js'

// ============================================================================
// JSON Schema representation (Tool.ts L15-21)
// ============================================================================

/**
 * Minimal JSON Schema object type used for tool input declarations.
 * Mirrors the subset of JSON Schema that the API accepts.
 */
export type ToolInputJSONSchema = {
  [x: string]: unknown
  type: 'object'
  properties?: {
    [x: string]: unknown
  }
}

// ============================================================================
// Validation (Tool.ts L95-101)
// ============================================================================

/**
 * Result of `Tool.validateInput`. Discriminated on `result`.
 */
export type ValidationResult =
  | { result: true }
  | {
      result: false
      message: string
      errorCode: number
    }

// ============================================================================
// Query chain tracking (Tool.ts L90-93)
// ============================================================================

/**
 * Tracks the position of a query within a chain of delegated queries
 * (e.g., agent -> sub-agent).
 */
export type QueryChainTracking = {
  chainId: string
  depth: number
}

// ============================================================================
// Compact progress event (Tool.ts L150-156)
// ============================================================================

/**
 * Events emitted during the compaction lifecycle so callers can render
 * progress spinners or hook into pre/post phases.
 */
export type CompactProgressEvent =
  | {
      type: 'hooks_start'
      hookType: 'pre_compact' | 'post_compact' | 'session_start'
    }
  | { type: 'compact_start' }
  | { type: 'compact_end' }

// ============================================================================
// ToolContractFields -- ABI subset of the Tool type
// ============================================================================

/**
 * The data-only, cross-language subset of `Tool`.
 *
 * Excludes all methods (`call`, `description`, `prompt`, `render*`,
 * `checkPermissions`, etc.) and only retains fields that are meaningful
 * across the IPC / FFI boundary.
 */
export type ToolContractFields = {
  /** Primary tool name (unique within a session). */
  readonly name: string
  /** Backwards-compatible aliases for renamed tools. */
  aliases?: string[]
  /** JSON Schema for the tool's input, when specified directly (not via Zod). */
  readonly inputJSONSchema?: ToolInputJSONSchema
  /** Max chars before tool result is persisted to disk. */
  maxResultSizeChars: number
  /** Enables strict mode for this tool. */
  readonly strict?: boolean
  /** True for tools originating from an MCP server. */
  isMcp?: boolean
  /** True for tools originating from an LSP server. */
  isLsp?: boolean
  /** When true, the tool is deferred and requires ToolSearch before use. */
  readonly shouldDefer?: boolean
  /** When true, the tool is never deferred. */
  readonly alwaysLoad?: boolean
  /** MCP server and tool names (unnormalized). */
  mcpInfo?: { serverName: string; toolName: string }
  /** Keyword hint for ToolSearch matching. */
  searchHint?: string
}

// ============================================================================
// ToolUseContextContract -- IPC-serializable subset of ToolUseContext
// ============================================================================

/**
 * IPC-serializable subset of `ToolUseContext`.
 *
 * Strips every callback / setter / React / AbortController field so the
 * shape can cross a process or FFI boundary as plain JSON. `Set<string>`
 * fields are converted to `string[]`.
 */
export type ToolUseContextContract = {
  options: {
    debug: boolean
    mainLoopModel: string
    verbose: boolean
    isNonInteractiveSession: boolean
    maxBudgetUsd?: number
    customSystemPrompt?: string
    appendSystemPrompt?: string
    querySource?: string
  }

  /** User modified the tool input via the permission prompt. */
  userModified?: boolean

  /** Current conversation messages (serialized). */
  messages: unknown[]

  /** File reading limits. */
  fileReadingLimits?: {
    maxTokens?: number
    maxSizeBytes?: number
  }

  /** Glob result limits. */
  globLimits?: {
    maxResults?: number
  }

  /** Per-tool decisions (serialized from Map). */
  toolDecisions?: Array<{
    toolName: string
    source: string
    decision: 'accept' | 'reject'
    timestamp: number
  }>

  /** Query chain position. */
  queryTracking?: QueryChainTracking

  /** Tool use ID for the current invocation. */
  toolUseId?: string

  /** Agent ID (only set for sub-agents). */
  agentId?: string

  /** Sub-agent type name. */
  agentType?: string

  /** Critical system reminder (experimental). */
  criticalSystemReminder_EXPERIMENTAL?: string

  /** Preserve tool use results for in-process teammates. */
  preserveToolUseResults?: boolean

  /**
   * Conversation ID.
   * 注意：不在 ToolUseContext 中定义，源自 REPL.tsx useState。
   * 此字段为合约扩展，由序列化边界从 session 状态填充。
   */
  conversationId?: UUID

  // -- Set<string> fields converted to string[] --

  /** Memory attachment triggers. */
  nestedMemoryAttachmentTriggers?: string[]

  /** Already-loaded nested memory paths. */
  loadedNestedMemoryPaths?: string[]

  /** Dynamic skill directory triggers. */
  dynamicSkillDirTriggers?: string[]

  /** Skill names surfaced via skill_discovery. */
  discoveredSkillNames?: string[]

  /** Whether canUseTool must always be called. */
  requireCanUseTool?: boolean
}
