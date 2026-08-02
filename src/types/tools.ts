/**
 * Tool progress types — reverse-engineered from usage across the codebase.
 *
 * This file is the centralized definition for all tool progress discriminated
 * union members. Each progress type is identified by its `type` field.
 *
 * Reconstruction sources:
 *   - BashTool.tsx:665-676          (BashProgress creation)
 *   - PowerShellTool.tsx:472-484    (PowerShellProgress creation)
 *   - AgentTool.tsx:796-804,1112-1122 (AgentToolProgress creation)
 *   - mcp/client.ts:1850,1888,1929,3105 (MCPProgress creation)
 *   - WebSearchTool.ts:347-353,377-384 (WebSearchProgress creation)
 *   - SkillTool.ts:250-258          (SkillToolProgress creation)
 *   - TaskOutputTool.tsx:244-251    (TaskOutputProgress creation)
 *   - BashModeProgress.tsx:34       (ShellProgress consumption)
 *   - sdkEventQueue.ts:30-33       (SdkWorkflowProgress usage)
 *
 * Reconstructed: 2026-04-03
 */

import type { AgentId } from './ids.js'
import type { NormalizedMessage } from './message.js'

// ---------------------------------------------------------------------------
// Shell-family progress (Bash & PowerShell)
// ---------------------------------------------------------------------------

/**
 * Progress emitted during bash command execution.
 * Created in BashTool.tsx via onProgress({ data: { type: 'bash_progress', ... } }).
 */
export type BashProgress = {
  type: 'bash_progress'
  /** Tail of recent output lines (last progress slice). */
  output: string
  /** Full accumulated stdout/stderr so far. */
  fullOutput: string
  /** Wall-clock seconds since the command started. */
  elapsedTimeSeconds: number
  /** Total number of output lines so far. */
  totalLines: number
  /** Total bytes of output so far (0 when complete). */
  totalBytes?: number
  /** Background task ID when the command has been backgrounded. */
  taskId?: string
  /** Timeout in milliseconds (present when an explicit timeout was set). */
  timeoutMs?: number
}

/**
 * Progress emitted during PowerShell command execution.
 * Created in PowerShellTool.tsx via onProgress({ data: { type: 'powershell_progress', ... } }).
 * Shape mirrors BashProgress with a different discriminant.
 */
export type PowerShellProgress = {
  type: 'powershell_progress'
  output: string
  fullOutput: string
  elapsedTimeSeconds: number
  totalLines: number
  totalBytes?: number
  taskId?: string
  timeoutMs?: number
}

/**
 * Union of shell-backend progress types.
 * Used in BashModeProgress.tsx and processBashCommand.tsx as a shared type
 * for both bash and powershell progress rendering.
 * Also forwarded by AgentTool to parent when sub-agent runs shell commands.
 */
export type ShellProgress = BashProgress | PowerShellProgress

// ---------------------------------------------------------------------------
// Agent / Skill progress
// ---------------------------------------------------------------------------

/**
 * Progress emitted by the AgentTool (sub-agent execution).
 * Each progress event carries a normalized message from the sub-agent's
 * conversation so the UI can render an expanding timeline.
 */
export type AgentToolProgress = {
  type: 'agent_progress'
  /** A normalized message (user or assistant) from the sub-agent conversation. */
  message: NormalizedMessage
  /** The original prompt text that was sent to the sub-agent. */
  prompt: string
  /** The branded agent ID for this sub-agent instance. */
  agentId: AgentId
}

/**
 * Progress emitted by the SkillTool.
 * Structurally identical to AgentToolProgress but uses a different discriminant
 * so the UI can distinguish skill invocations from generic sub-agents.
 */
export type SkillToolProgress = {
  type: 'skill_progress'
  message: NormalizedMessage
  prompt: string
  agentId: AgentId
}

// ---------------------------------------------------------------------------
// MCP progress
// ---------------------------------------------------------------------------

/**
 * Progress emitted during MCP tool calls.
 * The `status` field tracks the lifecycle: started -> progress -> completed/failed.
 * The optional `progress`/`total`/`progressMessage` fields are populated by the
 * MCP SDK's `onprogress` callback during the 'progress' status phase.
 */
export type MCPProgress = {
  type: 'mcp_progress'
  status: 'started' | 'progress' | 'completed' | 'failed'
  serverName: string
  toolName: string
  /** Elapsed wall-clock milliseconds (present on completed / failed). */
  elapsedTimeMs?: number
  /** Numeric progress value from the MCP SDK (present during 'progress' status). */
  progress?: number
  /** Total target value from the MCP SDK (present during 'progress' status). */
  total?: number
  /** Human-readable progress message from the MCP server. */
  progressMessage?: string
}

// ---------------------------------------------------------------------------
// Web search progress
// ---------------------------------------------------------------------------

/**
 * Progress emitted by the WebSearchTool.
 * Uses a nested discriminant on `type` to distinguish query updates from result
 * reception events.
 */
export type WebSearchProgress =
  | {
      type: 'query_update'
      query: string
    }
  | {
      type: 'search_results_received'
      resultCount: number
      query: string
    }

// ---------------------------------------------------------------------------
// Task output progress
// ---------------------------------------------------------------------------

/**
 * Progress emitted by the TaskOutputTool while waiting for a background task
 * to complete.
 */
export type TaskOutputProgress = {
  type: 'waiting_for_task'
  taskDescription: string
  taskType: string
}

// ---------------------------------------------------------------------------
// REPL progress (placeholder)
// ---------------------------------------------------------------------------

/**
 * Progress emitted by the REPL tool. The REPL tool is conditionally loaded
 * (ant-only) and its progress implementation is not present in the open-source
 * tree. This type exists so the ToolProgressData union and Tool.ts re-exports
 * compile correctly.
 */
export type REPLToolProgress = {
  type: 'repl_progress'
  [key: string]: unknown
}

// ---------------------------------------------------------------------------
// SDK workflow progress (serialized in task_progress events)
// ---------------------------------------------------------------------------

/**
 * A single workflow progress entry emitted inside `task_progress` SDK events.
 * Clients upsert by `${type}:${index}` then group by `phaseIndex` to rebuild
 * the phase tree (see sdkEventQueue.ts commentary).
 *
 * Because this type is only serialized to JSON for external consumers and
 * never pattern-matched internally, the shape is kept deliberately loose.
 */
export type SdkWorkflowProgress = {
  type: string
  index: number
  phaseIndex: number
  [key: string]: unknown
}

/**
 * Live progress emitted by WorkflowTool while a plugin workflow is running.
 * The task id links the foreground tool call to the task/output-file surface.
 */
export type WorkflowToolProgress = {
  type: 'workflow_progress'
  taskId: string
  workflow: string
  phase?: string
  phaseIndex: number
  message: string
  agentsStarted: number
  agentsCompleted: number
}

// ---------------------------------------------------------------------------
// Aggregate union
// ---------------------------------------------------------------------------

/**
 * Discriminated union of all tool progress types.
 * Used as the generic constraint for ToolProgress<P> and ToolCallProgress<P>
 * in Tool.ts. The `type` field is the discriminant.
 */
export type ToolProgressData =
  | BashProgress
  | PowerShellProgress
  | AgentToolProgress
  | SkillToolProgress
  | MCPProgress
  | WebSearchProgress
  | TaskOutputProgress
  | REPLToolProgress
  | WorkflowToolProgress
