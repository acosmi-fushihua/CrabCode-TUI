
// BetaJSONOutputFormat is not yet in the SDK typings; use a local placeholder.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type BetaJSONOutputFormat = any

import type {
  BetaToolChoiceAuto,
  BetaToolChoiceTool,
  BetaToolUnion,
  ClientOptions,
} from '../../types/api-types.js'
import type { QuerySource } from 'src/constants/querySource.js'
import type { Notification } from 'src/types/notification.js'
import type { AgentId } from 'src/types/ids.js'
import type { EffortValue } from 'src/utils/effort.js'
import type {
  AccountBridgeRuntimeAccess,
  CrabCodeThinkingMode,
} from '../accountBridge/types.js'
import type { AgentDefinition } from '../../tools/AgentTool/loadAgentsDir.js'
import type {
  QueryChainTracking,
  ToolPermissionContext,
  Tools,
} from '../../Tool.js'

// Define a type that represents valid JSON values
export type JsonValue = string | number | boolean | null | JsonObject | JsonArray
export type JsonObject = { [key: string]: JsonValue }
export type JsonArray = JsonValue[]

// output_config.task_budget — API-side token budget awareness for the model.
// Stainless SDK types don't yet include task_budget on BetaOutputConfig, so we
// define the wire shape locally and cast. The API validates on receipt; see
// api/api/schemas/messages/request/output_config.py:12-39 in the monorepo.
// Beta: task-budgets-2026-03-13 (EAP, crabcode-strudel-eap only as of Mar 2026).
export type TaskBudgetParam = {
  type: 'tokens'
  total: number
  remaining?: number
}

export type Options = {
  getToolPermissionContext: () => Promise<ToolPermissionContext>
  model: string
  toolChoice?: BetaToolChoiceTool | BetaToolChoiceAuto | undefined
  isNonInteractiveSession: boolean
  extraToolSchemas?: BetaToolUnion[]
  maxOutputTokensOverride?: number
  fallbackModel?: string
  onStreamingFallback?: () => void
  querySource: QuerySource
  agents: AgentDefinition[]
  allowedAgentTypes?: string[]
  hasAppendSystemPrompt: boolean
  fetchOverride?: ClientOptions['fetch']
  enablePromptCaching?: boolean
  skipCacheWrite?: boolean
  temperatureOverride?: number
  effortValue?: EffortValue
  /** User-selected cross-runtime thinking mode; account routes validate it dynamically. */
  crabcodeThinkingMode?: CrabCodeThinkingMode
  /** Worker-private, exact per-turn route capability and loopback endpoint. */
  accountBridgeRuntimeAccess?: AccountBridgeRuntimeAccess
  mcpTools: Tools
  hasPendingMcpServers?: boolean
  queryTracking?: QueryChainTracking
  agentId?: AgentId // Only set for subagents
  outputFormat?: BetaJSONOutputFormat
  fastMode?: boolean
  advisorModel?: string
  addNotification?: (notif: Notification) => void
  // API-side task budget (output_config.task_budget). Distinct from the
  // tokenBudget.ts +500k auto-continue feature — this one is sent to the API
  // so the model can pace itself. `remaining` is computed by the caller
  // (query.ts decrements across the agentic loop).
  taskBudget?: { total: number; remaining?: number }
}

export type FastModeOptions = Omit<
  Options,
  'model' | 'getToolPermissionContext'
> & {
  /**
   * Active conversation model for transcript-derived auxiliary work. When it
   * is custom/local/account, queryFastMode preserves that exact non-gateway
   * route instead of substituting the managed small/fast model.
   */
  sessionModel?: string
}

export type QueryWithModelOptions = Omit<Options, 'getToolPermissionContext'>

export type CachedMCEditsBlock = {
  type: 'cache_edits'
  edits: { type: 'delete'; cache_reference: string }[]
}

export type CachedMCPinnedEdits = {
  userMessageIndex: number
  block: CachedMCEditsBlock
}
