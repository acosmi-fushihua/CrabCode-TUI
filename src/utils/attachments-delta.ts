/**
 * attachments-delta.ts -- Delta attachment factories.
 *
 * Split from attachments.ts (3997 lines) for maintainability. Contains
 * delta-style attachments that diff current state against what's already
 * been announced in the conversation: deferred tools, agent listings,
 * MCP instructions, date changes, and ultrathink effort.
 */
import {
  logEvent,
} from 'src/services/analytics/index.js'
import { toolMatchesName, type Tools, type ToolUseContext } from '../Tool.js'
import type { Message } from 'src/types/message.js'
import {
  getDeferredToolsDelta,
  isDeferredToolsDeltaEnabled,
  isToolSearchEnabledOptimistic,
  isToolSearchToolAvailable,
  modelSupportsToolReference,
  type DeferredToolsDeltaScanContext,
} from './toolSearch.js'
import {
  getMcpInstructionsDelta,
  isMcpInstructionsDeltaEnabled,
} from './mcpInstructionsDelta.js'
import type { MCPServerConnection } from '../services/mcp/types.js'
import { AGENT_TOOL_NAME } from '../tools/AgentTool/constants.js'
import {
  formatAgentLine,
  shouldInjectAgentListInMessages,
} from '../tools/AgentTool/prompt.js'
import { filterAgentsByMcpRequirements } from '../tools/AgentTool/loadAgentsDir.js'
import { filterDeniedAgents } from './permissions/permissions.js'
import { mcpInfoFromString } from '../services/mcp/mcpStringUtils.js'
import { getSubscriptionType } from './auth.js'
import { getLocalISODate } from '../constants/common.js'
import {
  getLastEmittedDate,
  setLastEmittedDate,
  getKairosActive,
} from '../bootstrap/state.js'
import { hasUltrathinkKeyword, isUltrathinkEnabled } from './thinking.js'
import { feature } from './featurePolyfill.js'
/* eslint-disable @typescript-eslint/no-require-imports */
const sessionTranscriptModule = feature('KAIROS')
  ? (require('../services/sessionTranscript/sessionTranscript.js') as typeof import('../services/sessionTranscript/sessionTranscript.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

import type { Attachment } from './attachments-types.js'

/**
 * Detects when the local date has changed since the last turn (user coding
 * past midnight) and emits an attachment to notify the model.
 *
 * The date_change attachment is appended at the tail of the conversation,
 * so the model learns the new date without mutating the cached prefix.
 * messages[0] (from getUserContext -> prependUserContext) intentionally
 * keeps the stale date -- clearing that cache would regenerate the prefix
 * and turn the entire conversation into cache_creation on the next turn
 * (~920K effective tokens per midnight crossing per overnight session).
 *
 * Exported for testing -- regression guard for the cache-clear removal.
 */
export function getDateChangeAttachments(
  messages: Message[] | undefined,
): Attachment[] {
  const currentDate = getLocalISODate()
  const lastDate = getLastEmittedDate()

  if (lastDate === null) {
    // First turn -- just record, no attachment needed
    setLastEmittedDate(currentDate)
    return []
  }

  if (currentDate === lastDate) {
    return []
  }

  setLastEmittedDate(currentDate)

  // Assistant mode: flush yesterday's transcript to the per-day file so
  // the /dream skill (1-5am local) finds it even if no compaction fires
  // today. Fire-and-forget; writeSessionTranscriptSegment buckets by
  // message timestamp so a multi-day gap flushes each day correctly.
  if (feature('KAIROS')) {
    if (getKairosActive() && messages !== undefined) {
      sessionTranscriptModule?.flushOnDateChange(messages, currentDate)
    }
  }

  return [{ type: 'date_change', newDate: currentDate }]
}

export function getUltrathinkEffortAttachment(input: string | null): Attachment[] {
  if (!isUltrathinkEnabled() || !input || !hasUltrathinkKeyword(input)) {
    return []
  }
  logEvent('tengu_ultrathink', {})
  return [{ type: 'ultrathink_effort', level: 'high' }]
}

// Exported for compact.ts -- the gate must be identical at both call sites.
export function getDeferredToolsDeltaAttachment(
  tools: Tools,
  model: string,
  messages: Message[] | undefined,
  scanContext?: DeferredToolsDeltaScanContext,
): Attachment[] {
  if (!isDeferredToolsDeltaEnabled()) return []
  // These three checks mirror the sync parts of isToolSearchEnabled --
  // the attachment text says "available via ToolSearch", so ToolSearch
  // has to actually be in the request. The async auto-threshold check
  // is not replicated (would double-fire tengu_tool_search_mode_decision);
  // in tst-auto below-threshold the attachment can fire while ToolSearch
  // is filtered out, but that's a narrow case and the tools announced
  // are directly callable anyway.
  if (!isToolSearchEnabledOptimistic()) return []
  if (!modelSupportsToolReference(model)) return []
  if (!isToolSearchToolAvailable(tools)) return []
  const delta = getDeferredToolsDelta(tools, messages ?? [], scanContext)
  if (!delta) return []
  return [{ type: 'deferred_tools_delta', ...delta }]
}

/**
 * Diff the current filtered agent pool against what's already been announced
 * in this conversation (reconstructed from prior agent_listing_delta
 * attachments). Returns [] if nothing changed or the gate is off.
 *
 * The agent list was embedded in AgentTool's description, causing ~10.2% of
 * fleet cache_creation: MCP async connect, /reload-plugins, or
 * permission-mode change -> description changes -> full tool-schema cache bust.
 * Moving the list here keeps the tool description static.
 *
 * Exported for compact.ts -- re-announces the full set after compaction eats
 * prior deltas.
 */
export function getAgentListingDeltaAttachment(
  toolUseContext: ToolUseContext,
  messages: Message[] | undefined,
): Attachment[] {
  if (!shouldInjectAgentListInMessages()) return []

  // Skip if AgentTool isn't in the pool -- the listing would be unactionable.
  if (
    !toolUseContext.options.tools.some(t => toolMatchesName(t, AGENT_TOOL_NAME))
  ) {
    return []
  }

  const { activeAgents, allowedAgentTypes } =
    toolUseContext.options.agentDefinitions

  // Mirror AgentTool.prompt()'s filtering: MCP requirements -> deny rules ->
  // allowedAgentTypes restriction. Keep this in sync with AgentTool.tsx.
  const mcpServers = new Set<string>()
  for (const tool of toolUseContext.options.tools) {
    const serverName =
      tool.mcpInfo?.serverName ?? mcpInfoFromString(tool.name)?.serverName
    if (serverName) mcpServers.add(serverName)
  }
  const permissionContext = toolUseContext.getAppState().toolPermissionContext
  let filtered = filterDeniedAgents(
    filterAgentsByMcpRequirements(activeAgents, [...mcpServers]),
    permissionContext,
    AGENT_TOOL_NAME,
  )
  if (allowedAgentTypes) {
    filtered = filtered.filter(a => allowedAgentTypes.includes(a.agentType))
  }

  // Reconstruct announced set from prior deltas in the transcript.
  const announced = new Set<string>()
  for (const msg of messages ?? []) {
    if (msg.type !== 'attachment') continue
    if (msg.attachment.type !== 'agent_listing_delta') continue
    for (const t of msg.attachment.addedTypes) announced.add(t)
    for (const t of msg.attachment.removedTypes) announced.delete(t)
  }

  const currentTypes = new Set(filtered.map(a => a.agentType))
  const added = filtered.filter(a => !announced.has(a.agentType))
  const removed: string[] = []
  for (const t of announced) {
    if (!currentTypes.has(t)) removed.push(t)
  }

  if (added.length === 0 && removed.length === 0) return []

  // Sort for deterministic output -- agent load order is nondeterministic
  // (plugin load races, MCP async connect).
  added.sort((a, b) => a.agentType.localeCompare(b.agentType))
  removed.sort()

  return [
    {
      type: 'agent_listing_delta',
      addedTypes: added.map(a => a.agentType),
      addedLines: added.map(formatAgentLine),
      removedTypes: removed,
      isInitial: announced.size === 0,
      showConcurrencyNote: getSubscriptionType() !== 'pro',
    },
  ]
}

// Exported for compact.ts / reactiveCompact.ts -- single source of truth for the gate.
export function getMcpInstructionsDeltaAttachment(
  mcpClients: MCPServerConnection[],
  tools: Tools,
  model: string,
  messages: Message[] | undefined,
): Attachment[] {
  if (!isMcpInstructionsDeltaEnabled()) return []

  const delta = getMcpInstructionsDelta(mcpClients, messages ?? [], [])
  if (!delta) return []
  return [{ type: 'mcp_instructions_delta', ...delta }]
}
