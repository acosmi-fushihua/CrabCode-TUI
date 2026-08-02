/**
 * CrabCode message type definitions.
 *
 * This file was reverse-engineered from factory functions in utils/messages.ts,
 * consumer imports across the codebase, and runtime shape analysis.
 * It is the single source of truth for all message types used in the
 * conversation pipeline, rendering layer, and SDK bridge.
 */

import type {
  APIError,
  BetaContentBlock,
  BetaMessage,
  BetaRawMessageStreamEvent,
  ContentBlockParam,
} from './api-types.js'
import type { UUID } from 'crypto'

import type { SDKAssistantMessageError } from '../entrypoints/agentSdkTypes.js'
import type { Attachment } from '../utils/attachments.js'
import type { PermissionMode } from './permissions.js'
import type { Progress } from '../Tool.js'
import type {
  BranchAction,
  CommitKind,
  PrAction,
} from '../tools/shared/gitOperationTracking.js'

// ---------------------------------------------------------------------------
// Scalar / enum types
// ---------------------------------------------------------------------------

export type SystemMessageLevel = 'info' | 'warning' | 'error' | 'suggestion'

/**
 * Provenance of a user message.
 * `undefined` is equivalent to `{ kind: 'human' }` (typed at the keyboard).
 * Discriminated union with `kind` field — consumers use `origin.kind` to branch.
 */
export type MessageOrigin =
  | { kind: 'human' }
  | { kind: 'task-notification' }
  | { kind: 'coordinator' }
  | { kind: 'channel'; server: string }
  | { kind: 'auto-accept' }

export type PartialCompactDirection = 'leading' | 'trailing' | 'from' | 'up_to'

// ---------------------------------------------------------------------------
// StopHookInfo (used by SystemStopHookSummaryMessage)
// ---------------------------------------------------------------------------

export interface StopHookInfo {
  hookName: string
  durationMs: number
  output?: string
}

// ---------------------------------------------------------------------------
// CompactMetadata (used by SystemCompactBoundaryMessage)
// ---------------------------------------------------------------------------

export interface CompactMetadata {
  trigger: 'manual' | 'auto'
  preTokens: number
  userContext?: string
  messagesSummarized?: number
  preCompactDiscoveredTools?: string[]
  preservedSegment?: {
    headUuid: string
    anchorUuid: string
    tailUuid: string
  }
}

// ---------------------------------------------------------------------------
// MicrocompactMetadata (used by SystemMicrocompactBoundaryMessage)
// ---------------------------------------------------------------------------

export interface MicrocompactMetadata {
  trigger: 'auto'
  preTokens: number
  tokensSaved: number
  compactedToolIds: string[]
  clearedAttachmentUUIDs: string[]
}

// ---------------------------------------------------------------------------
// Primary message types
// ---------------------------------------------------------------------------

export interface AssistantMessage {
  type: 'assistant'
  uuid: UUID
  timestamp: string
  message: BetaMessage & { container: null; context_management: null }
  requestId?: string
  apiError?: APIError
  error?: SDKAssistantMessageError
  errorDetails?: string
  isApiErrorMessage?: boolean
  isVirtual?: true
  // W-SESSION-TRANSCRIPT-CORRUPTION (2026-05-31): set on frames produced by the
  // worker's history reconstruction (extractRuntimeHistoryMessages /
  // turnsToMessages). These are model-context injection, never persisted —
  // isLoggableMessage excludes them. Declared here (not just attached via cast)
  // so the marker survives any field-aware serialization the way isMeta does.
  isReconstructedHistory?: true
  // PR-A (V3 billing transparency, W-DOC-VISION-WORK-REMEDIATION, 2026-07-01):
  // set when this turn's image blocks were degraded for a text-only model
  // (queryModel.ts's applyMediaCapabilityPolicy). `sidecarDescribed` = images
  // the vision sidecar successfully described (extra billed call, working);
  // `placeholderFallback` = images that fell back to an honest text
  // placeholder (sidecar off/unavailable/failed). Attached to exactly one
  // AssistantMessage per turn — see queryModel.ts for why.
  mediaSidecarUsed?: {
    /** Compatibility aggregate: fresh + memory cache + disk cache. */
    sidecarDescribed: number
    /** Compatibility alias retained for persisted pre-detail transcripts. */
    placeholderFallback: number
    freshDescriptions: number
    memoryCacheHits: number
    diskCacheHits: number
    placeholderFallbacks: number
    /**
     * PR-R2a (2026-07-24): per-outcome-kind tally of the placeholder
     * fallbacks (e.g. `{ consent_required: 2 }`). Optional — absent on
     * legacy transcripts and on worker-projected items (the protocol stamp
     * deliberately does not carry it).
     */
    placeholderFallbackKinds?: Record<string, number>
  }
}

export interface UserMessage {
  type: 'user'
  message: { role: 'user'; content: string | ContentBlockParam[] }
  uuid: UUID
  timestamp: string
  isMeta?: true
  isVisibleInTranscriptOnly?: true
  isVirtual?: true
  isCompactSummary?: true
  // W-SESSION-TRANSCRIPT-CORRUPTION (2026-05-31): see AssistantMessage above.
  isReconstructedHistory?: true
  toolUseResult?: unknown
  /** Host-validated display artifacts; never included in model content blocks. */
  toolArtifacts?: import('./toolArtifact.js').ToolArtifact[]
  mcpMeta?: {
    _meta?: Record<string, unknown>
    structuredContent?: Record<string, unknown>
  }
  imagePasteIds?: number[]
  sourceToolAssistantUUID?: UUID
  sourceToolUseID?: string
  permissionMode?: PermissionMode
  origin?: MessageOrigin
  summarizeMetadata?: {
    messagesSummarized: number
    userContext?: string
    direction?: PartialCompactDirection
  }
  /** The plan content string, present when this message carries a plan for verification. */
  planContent?: string
}

export interface ProgressMessage<P extends Progress = Progress> {
  type: 'progress'
  data: P
  toolUseID: string
  parentToolUseID: string
  uuid: UUID
  timestamp: string
}

export interface AttachmentMessage<A extends Attachment = Attachment> {
  type: 'attachment'
  attachment: A
  uuid: UUID
  timestamp: string
  toolUseID?: string
}

/**
 * HookResultMessage is structurally identical to AttachmentMessage.
 * Hook execution yields results via `createAttachmentMessage(...)`.
 */
export type HookResultMessage = AttachmentMessage

// ---------------------------------------------------------------------------
// Stream / transient event types
// ---------------------------------------------------------------------------

export interface StreamEvent {
  type: 'stream_event'
  event: BetaRawMessageStreamEvent
  ttftMs?: number
  uuid?: string
}

export interface RequestStartEvent {
  type: 'stream_request_start'
  uuid?: string
}

export interface TombstoneMessage {
  type: 'tombstone'
  message: Message
  uuid?: string
}

export interface ToolUseSummaryMessage {
  type: 'tool_use_summary'
  summary: string
  precedingToolUseIds: string[]
  uuid: UUID
  timestamp: string
}

// ---------------------------------------------------------------------------
// System message subtypes
// ---------------------------------------------------------------------------

/** Base fields shared by all system messages. */
interface SystemMessageBase {
  type: 'system'
  isMeta?: boolean
  timestamp: string
  uuid: UUID
}

export interface SystemInformationalMessage extends SystemMessageBase {
  subtype: 'informational'
  content: string
  level: SystemMessageLevel
  toolUseID?: string
  preventContinuation?: boolean
}

export interface SystemPermissionRetryMessage extends SystemMessageBase {
  subtype: 'permission_retry'
  content: string
  commands: string[]
  level: SystemMessageLevel
}

export interface SystemScheduledTaskFireMessage extends SystemMessageBase {
  subtype: 'scheduled_task_fire'
  content: string
}

export interface SystemStopHookSummaryMessage extends SystemMessageBase {
  subtype: 'stop_hook_summary'
  hookCount: number
  hookInfos: StopHookInfo[]
  hookErrors: string[]
  preventedContinuation: boolean
  stopReason: string | undefined
  hasOutput: boolean
  level: SystemMessageLevel
  toolUseID?: string
  hookLabel?: string
  totalDurationMs?: number
}

export interface SystemTurnDurationMessage extends SystemMessageBase {
  subtype: 'turn_duration'
  durationMs: number
  budgetTokens?: number
  budgetLimit?: number
  budgetNudges?: number
  messageCount?: number
}

export interface SystemAwaySummaryMessage extends SystemMessageBase {
  subtype: 'away_summary'
  content: string
}

export interface SystemMemorySavedMessage extends SystemMessageBase {
  subtype: 'memory_saved'
  writtenPaths: string[]
  teamCount?: number
}

export interface SystemAgentsKilledMessage extends SystemMessageBase {
  subtype: 'agents_killed'
}

export interface SystemApiMetricsMessage extends SystemMessageBase {
  subtype: 'api_metrics'
  ttftMs: number
  otps: number
  isP50?: boolean
  hookDurationMs?: number
  turnDurationMs?: number
  toolDurationMs?: number
  classifierDurationMs?: number
  toolCount?: number
  hookCount?: number
  classifierCount?: number
  configWriteCount?: number
}

export interface SystemLocalCommandMessage extends SystemMessageBase {
  subtype: 'local_command'
  content: string
  level?: SystemMessageLevel
}

export interface SystemAPIErrorMessage extends SystemMessageBase {
  subtype: 'api_error'
  level: 'error'
  cause?: Error
  error: APIError
  retryInMs: number
  retryAttempt: number
  maxRetries: number
}

export interface SystemCompactBoundaryMessage extends SystemMessageBase {
  subtype: 'compact_boundary'
  content: string
  level: SystemMessageLevel
  compactMetadata: CompactMetadata
  logicalParentUuid?: UUID
}

export interface SystemMicrocompactBoundaryMessage extends SystemMessageBase {
  subtype: 'microcompact_boundary'
  content: string
  level: SystemMessageLevel
  microcompactMetadata: MicrocompactMetadata
}

export interface SystemCommandInputMessage extends SystemMessageBase {
  subtype: 'command_input'
  content: string
}

export interface SystemThinkingMessage extends SystemMessageBase {
  subtype: 'thinking'
  content: string
}

export interface SystemFileSnapshotMessage extends SystemMessageBase {
  subtype: 'file_snapshot'
  content: string
  level: SystemMessageLevel
  snapshotFiles: Array<{ key: string; path: string; content: string }>
}

/**
 * Discriminated union of all system message subtypes.
 */
export type SystemMessage =
  | SystemInformationalMessage
  | SystemPermissionRetryMessage
  | SystemScheduledTaskFireMessage
  | SystemStopHookSummaryMessage
  | SystemTurnDurationMessage
  | SystemAwaySummaryMessage
  | SystemMemorySavedMessage
  | SystemAgentsKilledMessage
  | SystemApiMetricsMessage
  | SystemLocalCommandMessage
  | SystemAPIErrorMessage
  | SystemCompactBoundaryMessage
  | SystemMicrocompactBoundaryMessage
  | SystemCommandInputMessage
  | SystemThinkingMessage
  | SystemFileSnapshotMessage

// ---------------------------------------------------------------------------
// Top-level Message union
// ---------------------------------------------------------------------------

/**
 * Any message that can appear in the conversation transcript.
 */
export type Message =
  | AssistantMessage
  | UserMessage
  | ProgressMessage
  | AttachmentMessage
  | SystemMessage
  | StreamEvent
  | RequestStartEvent
  | TombstoneMessage
  | ToolUseSummaryMessage

// ---------------------------------------------------------------------------
// Normalized message types (single content block per message)
// ---------------------------------------------------------------------------

/**
 * An AssistantMessage whose content array has been split so that each
 * NormalizedAssistantMessage carries exactly one BetaContentBlock.
 */
// eslint-disable-next-line @typescript-eslint/no-unused-vars
export interface NormalizedAssistantMessage<_T extends BetaContentBlock = BetaContentBlock>
  extends Omit<AssistantMessage, 'message'> {
  type: 'assistant'
  message: Omit<AssistantMessage['message'], 'content'> & {
    content: [BetaContentBlock]
  }
}

/**
 * A UserMessage whose content has been split so that each
 * NormalizedUserMessage carries a single ContentBlockParam (or a string).
 */
export interface NormalizedUserMessage extends Omit<UserMessage, 'message'> {
  type: 'user'
  message: {
    role: 'user'
    content: string | [ContentBlockParam]
  }
}

/**
 * The full set of message shapes after normalization.
 */
export type NormalizedMessage =
  | NormalizedAssistantMessage
  | NormalizedUserMessage
  | ProgressMessage
  | AttachmentMessage
  | SystemMessage

// ---------------------------------------------------------------------------
// UI / rendering types
// ---------------------------------------------------------------------------

/**
 * A message produced by grouping consecutive tool_use calls for the same
 * tool into a single visual unit. Created by `groupToolUses()`.
 */
export interface GroupedToolUseMessage {
  type: 'grouped_tool_use'
  toolName: string
  messages: NormalizedAssistantMessage[]
  results: NormalizedUserMessage[]
  displayMessage: NormalizedAssistantMessage
  uuid: string
  timestamp: string
  messageId?: string
}

/**
 * A collapsed summary of consecutive Read / Search / Glob / Bash-read
 * operations. Created by `collapseReadSearchGroups()`.
 */
export interface CollapsedReadSearchGroup {
  type: 'collapsed_read_search'
  searchCount: number
  readCount: number
  listCount: number
  replCount: number
  memorySearchCount: number
  memoryReadCount: number
  memoryWriteCount: number
  readFilePaths: string[]
  searchArgs: string[]
  latestDisplayHint: string | undefined
  messages: CollapsibleMessage[]
  displayMessage: CollapsibleMessage
  uuid: UUID
  timestamp: string
  /** Team memory counts (feature-gated) */
  teamMemorySearchCount?: number
  teamMemoryReadCount?: number
  teamMemoryWriteCount?: number
  /** MCP tool calls collapsed into this group */
  mcpCallCount?: number
  mcpServerNames?: string[]
  /** Bash commands that are not search/read */
  bashCount?: number
  gitOpBashCount?: number
  commits?: Array<{ sha: string; kind: CommitKind }>
  pushes?: Array<{ branch: string }>
  branches?: Array<{ ref: string; action: BranchAction }>
  prs?: Array<{ number: number; url?: string; action: PrAction }>
  /** PreToolUse hook timing absorbed from hook summary messages */
  hookTotalMs?: number
  hookCount?: number
  hookInfos?: StopHookInfo[]
  /** Relevant memories absorbed into this group */
  relevantMemories?: Array<{ path: string; content: string; mtimeMs: number }>
}

/**
 * A message that can be collapsed into a `CollapsedReadSearchGroup`.
 * Includes both normalized assistant/user messages and grouped tool uses.
 */
export type CollapsibleMessage =
  | NormalizedAssistantMessage
  | NormalizedUserMessage
  | GroupedToolUseMessage

/**
 * Any message shape that the rendering layer can display.
 * Includes normalized messages plus synthetic UI-only types.
 */
export type RenderableMessage =
  | NormalizedAssistantMessage
  | NormalizedUserMessage
  | AttachmentMessage
  | SystemMessage
  | GroupedToolUseMessage
  | CollapsedReadSearchGroup
