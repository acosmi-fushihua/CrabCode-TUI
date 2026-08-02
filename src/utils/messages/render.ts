/**
 * render.ts -- TS-only UI helpers: extractTag, reorderMessagesInUI,
 * buildMessageLookups, shouldShowUserMessage, etc.
 *
 * These are client/renderer-only utilities that will NOT be codegen'd to
 * Go/Rust. They depend on envelope.ts for shared types and predicates.
 */

import type { BetaToolUseBlock, ToolUseBlock, ToolUseBlockParam } from '../../types/api-types.js'
import { feature } from '../featurePolyfill.js'

import type {
  HookEvent,
} from 'src/entrypoints/agentSdkTypes.js'

import { NO_CONTENT_MESSAGE } from '../../constants/messages.js'
import {
  COMMAND_ARGS_TAG,
  COMMAND_NAME_TAG,
} from '../../constants/xml.js'
import { stripIdeContextTags } from '../displayTags.js'
import { count } from '../array.js'
import { escapeRegExp } from '../stringUtils.js'
import type {
  HookAttachment,
  HookPermissionDecisionAttachment,
} from '../attachments.js'

import type {
  AssistantMessage,
  AttachmentMessage,
  Message,
  NormalizedAssistantMessage,
  NormalizedMessage,
  NormalizedUserMessage,
  ProgressMessage,
  SystemMessage,
  UserMessage,
} from '../../types/message.js'

import {
  DENIAL_WORKAROUND_GUIDANCE,
  extractTextContent,
  getToolUseID,
  getUserMessageText,
  isHookAttachmentMessage,
  isToolUseRequestMessage,
} from './envelope.js'

// Hook attachments that have a hookName field (excludes HookPermissionDecisionAttachment)
type HookAttachmentWithName = Exclude<
  HookAttachment,
  HookPermissionDecisionAttachment
>

// ---------------------------------------------------------------------------
// extractTag
// ---------------------------------------------------------------------------

export function extractTag(html: string, tagName: string): string | null {
  if (!html.trim() || !tagName.trim()) {
    return null
  }

  const escapedTag = escapeRegExp(tagName)

  // Create regex pattern that handles:
  // 1. Self-closing tags
  // 2. Tags with attributes
  // 3. Nested tags of the same type
  // 4. Multiline content
  const pattern = new RegExp(
    `<${escapedTag}(?:\\s+[^>]*)?>` + // Opening tag with optional attributes
      '([\\s\\S]*?)' + // Content (non-greedy match)
      `<\\/${escapedTag}>`, // Closing tag
    'gi',
  )

  let match
  let depth = 0
  let lastIndex = 0
  const openingTag = new RegExp(`<${escapedTag}(?:\\s+[^>]*?)?>`, 'gi')
  const closingTag = new RegExp(`<\\/${escapedTag}>`, 'gi')

  while ((match = pattern.exec(html)) !== null) {
    // Check for nested tags
    const content = match[1]
    const beforeMatch = html.slice(lastIndex, match.index)

    // Reset depth counter
    depth = 0

    // Count opening tags before this match
    openingTag.lastIndex = 0
    while (openingTag.exec(beforeMatch) !== null) {
      depth++
    }

    // Count closing tags before this match
    closingTag.lastIndex = 0
    while (closingTag.exec(beforeMatch) !== null) {
      depth--
    }

    // Only include content if we're at the correct nesting level
    if (depth === 0 && content) {
      return content
    }

    lastIndex = match.index + match[0].length
  }

  return null
}

// ---------------------------------------------------------------------------
// reorderMessagesInUI
// ---------------------------------------------------------------------------

type ToolUseRequestMessage = NormalizedAssistantMessage & {
  message: { content: [ToolUseBlock] }
}

// Re-order, to move result messages to be after their tool use messages
export function reorderMessagesInUI(
  messages: (
    | NormalizedUserMessage
    | NormalizedAssistantMessage
    | AttachmentMessage
    | SystemMessage
  )[],
  syntheticStreamingToolUseMessages: NormalizedAssistantMessage[],
): (
  | NormalizedUserMessage
  | NormalizedAssistantMessage
  | AttachmentMessage
  | SystemMessage
)[] {
  // Maps tool use ID to its related messages
  const toolUseGroups = new Map<
    string,
    {
      toolUse: ToolUseRequestMessage | null
      preHooks: AttachmentMessage[]
      toolResult: NormalizedUserMessage | null
      postHooks: AttachmentMessage[]
    }
  >()

  // First pass: group messages by tool use ID
  for (const message of messages) {
    // Handle tool use messages
    if (isToolUseRequestMessage(message)) {
      const toolUseID = message.message.content[0]?.id
      if (toolUseID) {
        if (!toolUseGroups.has(toolUseID)) {
          toolUseGroups.set(toolUseID, {
            toolUse: null,
            preHooks: [],
            toolResult: null,
            postHooks: [],
          })
        }
        toolUseGroups.get(toolUseID)!.toolUse = message as ToolUseRequestMessage
      }
      continue
    }

    // Handle pre-tool-use hooks
    if (
      isHookAttachmentMessage(message) &&
      message.attachment.hookEvent === 'PreToolUse'
    ) {
      const toolUseID = message.attachment.toolUseID
      if (!toolUseGroups.has(toolUseID)) {
        toolUseGroups.set(toolUseID, {
          toolUse: null,
          preHooks: [],
          toolResult: null,
          postHooks: [],
        })
      }
      toolUseGroups.get(toolUseID)!.preHooks.push(message)
      continue
    }

    // Handle tool results
    if (
      message.type === 'user'
    ) {
      const _c0r = message.message.content[0]
      if (typeof _c0r !== 'string' && _c0r?.type === 'tool_result') {
        const toolUseID = _c0r.tool_use_id
        if (!toolUseGroups.has(toolUseID)) {
          toolUseGroups.set(toolUseID, {
            toolUse: null,
            preHooks: [],
            toolResult: null,
            postHooks: [],
          })
        }
        toolUseGroups.get(toolUseID)!.toolResult = message
        continue
      }
    }

    // Handle post-tool-use hooks
    if (
      isHookAttachmentMessage(message) &&
      message.attachment.hookEvent === 'PostToolUse'
    ) {
      const toolUseID = message.attachment.toolUseID
      if (!toolUseGroups.has(toolUseID)) {
        toolUseGroups.set(toolUseID, {
          toolUse: null,
          preHooks: [],
          toolResult: null,
          postHooks: [],
        })
      }
      toolUseGroups.get(toolUseID)!.postHooks.push(message)
      continue
    }
  }

  // Second pass: reconstruct the message list in the correct order
  const result: (
    | NormalizedUserMessage
    | NormalizedAssistantMessage
    | AttachmentMessage
    | SystemMessage
  )[] = []
  const processedToolUses = new Set<string>()

  for (const message of messages) {
    // Check if this is a tool use
    if (isToolUseRequestMessage(message)) {
      const toolUseID = message.message.content[0]?.id
      if (toolUseID && !processedToolUses.has(toolUseID)) {
        processedToolUses.add(toolUseID)
        const group = toolUseGroups.get(toolUseID)
        if (group && group.toolUse) {
          // Output in order: tool use, pre hooks, tool result, post hooks
          result.push(group.toolUse)
          result.push(...group.preHooks)
          if (group.toolResult) {
            result.push(group.toolResult)
          }
          result.push(...group.postHooks)
        }
      }
      continue
    }

    // Check if this message is part of a tool use group
    if (
      isHookAttachmentMessage(message) &&
      (message.attachment.hookEvent === 'PreToolUse' ||
        message.attachment.hookEvent === 'PostToolUse')
    ) {
      // Skip - already handled in tool use groups
      continue
    }

    if (
      message.type === 'user'
    ) {
      const _c0skip = message.message.content[0]
      if (typeof _c0skip !== 'string' && _c0skip?.type === 'tool_result') {
        // Skip - already handled in tool use groups
        continue
      }
    }

    // Handle api error messages (only keep the last one)
    if (message.type === 'system' && message.subtype === 'api_error') {
      const last = result.at(-1)
      if (last?.type === 'system' && last.subtype === 'api_error') {
        result[result.length - 1] = message
      } else {
        result.push(message)
      }
      continue
    }

    // Add standalone messages
    result.push(message)
  }

  // Add synthetic streaming tool use messages
  for (const message of syntheticStreamingToolUseMessages) {
    result.push(message)
  }

  // Filter to keep only the last api error message
  const last = result.at(-1)
  return result.filter(
    _ => _.type !== 'system' || _.subtype !== 'api_error' || _ === last,
  )
}

// ---------------------------------------------------------------------------
// buildMessageLookups / buildSubagentLookups
// ---------------------------------------------------------------------------

export type MessageLookups = {
  siblingToolUseIDs: Map<string, Set<string>>
  progressMessagesByToolUseID: Map<string, ProgressMessage[]>
  inProgressHookCounts: Map<string, Map<HookEvent, number>>
  resolvedHookCounts: Map<string, Map<HookEvent, number>>
  /** Maps tool_use_id to the user message containing its tool_result */
  toolResultByToolUseID: Map<string, NormalizedMessage>
  /** Maps tool_use_id to the ToolUseBlockParam */
  toolUseByToolUseID: Map<string, ToolUseBlockParam>
  /** Total count of normalized messages (for truncation indicator text) */
  normalizedMessageCount: number
  /** Set of tool use IDs that have a corresponding tool_result */
  resolvedToolUseIDs: Set<string>
  /** Set of tool use IDs that have an errored tool_result */
  erroredToolUseIDs: Set<string>
}

/**
 * Build pre-computed lookups for efficient O(1) access to message relationships.
 * Call once per render, then use the lookups for all messages.
 */
export function buildMessageLookups(
  normalizedMessages: NormalizedMessage[],
  messages: Message[],
): MessageLookups {
  // First pass: group assistant messages by ID and collect all tool use IDs per message
  const toolUseIDsByMessageID = new Map<string, Set<string>>()
  const toolUseIDToMessageID = new Map<string, string>()
  const toolUseByToolUseID = new Map<string, ToolUseBlockParam>()
  for (const msg of messages) {
    if (msg.type === 'assistant') {
      const id = msg.message.id
      let toolUseIDs = toolUseIDsByMessageID.get(id)
      if (!toolUseIDs) {
        toolUseIDs = new Set()
        toolUseIDsByMessageID.set(id, toolUseIDs)
      }
      for (const content of msg.message.content) {
        if (content.type === 'tool_use') {
          toolUseIDs.add(content.id)
          toolUseIDToMessageID.set(content.id, id)
          toolUseByToolUseID.set(content.id, content)
        }
      }
    }
  }

  // Build sibling lookup - each tool use ID maps to all sibling tool use IDs
  const siblingToolUseIDs = new Map<string, Set<string>>()
  for (const [toolUseID, messageID] of toolUseIDToMessageID) {
    siblingToolUseIDs.set(toolUseID, toolUseIDsByMessageID.get(messageID)!)
  }

  // Single pass over normalizedMessages to build progress, hook, and tool result lookups
  const progressMessagesByToolUseID = new Map<string, ProgressMessage[]>()
  const inProgressHookCounts = new Map<string, Map<HookEvent, number>>()
  const resolvedHookNames = new Map<string, Map<HookEvent, Set<string>>>()
  const toolResultByToolUseID = new Map<string, NormalizedMessage>()
  const resolvedToolUseIDs = new Set<string>()
  const erroredToolUseIDs = new Set<string>()

  for (const msg of normalizedMessages) {
    if (msg.type === 'progress') {
      const toolUseID = msg.parentToolUseID
      const existing = progressMessagesByToolUseID.get(toolUseID)
      if (existing) {
        existing.push(msg)
      } else {
        progressMessagesByToolUseID.set(toolUseID, [msg])
      }

      if (msg.data.type === 'hook_progress') {
        const hookEvent = msg.data.hookEvent
        let byHookEvent = inProgressHookCounts.get(toolUseID)
        if (!byHookEvent) {
          byHookEvent = new Map()
          inProgressHookCounts.set(toolUseID, byHookEvent)
        }
        byHookEvent.set(hookEvent, (byHookEvent.get(hookEvent) ?? 0) + 1)
      }
    }

    if (msg.type === 'user') {
      for (const content of Array.isArray(msg.message.content) ? msg.message.content : []) {
        if (typeof content !== 'string' && content.type === 'tool_result') {
          toolResultByToolUseID.set(content.tool_use_id, msg)
          resolvedToolUseIDs.add(content.tool_use_id)
          if (content.is_error) {
            erroredToolUseIDs.add(content.tool_use_id)
          }
        }
      }
    }

    if (msg.type === 'assistant') {
      for (const content of msg.message.content) {
        if (
          'tool_use_id' in content &&
          typeof (content as { tool_use_id: string }).tool_use_id === 'string'
        ) {
          resolvedToolUseIDs.add(
            (content as { tool_use_id: string }).tool_use_id,
          )
        }
        if ((content.type as string) === 'advisor_tool_result') {
          const result = content as {
            tool_use_id: string
            content: { type: string }
          }
          if (result.content.type === 'advisor_tool_result_error') {
            erroredToolUseIDs.add(result.tool_use_id)
          }
        }
      }
    }

    if (isHookAttachmentMessage(msg)) {
      const toolUseID = msg.attachment.toolUseID
      const hookEvent = msg.attachment.hookEvent
      const hookName = (msg.attachment as HookAttachmentWithName).hookName
      if (hookName !== undefined) {
        let byHookEvent = resolvedHookNames.get(toolUseID)
        if (!byHookEvent) {
          byHookEvent = new Map()
          resolvedHookNames.set(toolUseID, byHookEvent)
        }
        let names = byHookEvent.get(hookEvent)
        if (!names) {
          names = new Set()
          byHookEvent.set(hookEvent, names)
        }
        names.add(hookName)
      }
    }
  }

  // Convert resolved hook name sets to counts
  const resolvedHookCounts = new Map<string, Map<HookEvent, number>>()
  for (const [toolUseID, byHookEvent] of resolvedHookNames) {
    const countMap = new Map<HookEvent, number>()
    for (const [hookEvent, names] of byHookEvent) {
      countMap.set(hookEvent, names.size)
    }
    resolvedHookCounts.set(toolUseID, countMap)
  }

  // Mark orphaned server_tool_use / mcp_tool_use blocks as errored
  const lastMsg = messages.at(-1)
  const lastAssistantMsgId =
    lastMsg?.type === 'assistant' ? lastMsg.message.id : undefined
  for (const msg of normalizedMessages) {
    if (msg.type !== 'assistant') continue
    if (msg.message.id === lastAssistantMsgId) continue
    for (const content of msg.message.content) {
      if (
        (content.type === 'server_tool_use' ||
          content.type === 'mcp_tool_use') &&
        !resolvedToolUseIDs.has((content as { id: string }).id)
      ) {
        const id = (content as { id: string }).id
        resolvedToolUseIDs.add(id)
        erroredToolUseIDs.add(id)
      }
    }
  }

  return {
    siblingToolUseIDs,
    progressMessagesByToolUseID,
    inProgressHookCounts,
    resolvedHookCounts,
    toolResultByToolUseID,
    toolUseByToolUseID,
    normalizedMessageCount: normalizedMessages.length,
    resolvedToolUseIDs,
    erroredToolUseIDs,
  }
}

/** Empty lookups for static rendering contexts that don't need real lookups. */
export const EMPTY_LOOKUPS: MessageLookups = {
  siblingToolUseIDs: new Map(),
  progressMessagesByToolUseID: new Map(),
  inProgressHookCounts: new Map(),
  resolvedHookCounts: new Map(),
  toolResultByToolUseID: new Map(),
  toolUseByToolUseID: new Map(),
  normalizedMessageCount: 0,
  resolvedToolUseIDs: new Set(),
  erroredToolUseIDs: new Set(),
}

/**
 * Shared empty Set singleton.
 */
export const EMPTY_STRING_SET: ReadonlySet<string> = Object.freeze(
  new Set<string>(),
)

/**
 * Build lookups from subagent/skill progress messages.
 */
export function buildSubagentLookups(
  messages: { message: AssistantMessage | NormalizedUserMessage }[],
): { lookups: MessageLookups; inProgressToolUseIDs: Set<string> } {
  const toolUseByToolUseID = new Map<string, ToolUseBlockParam>()
  const resolvedToolUseIDs = new Set<string>()
  const toolResultByToolUseID = new Map<
    string,
    NormalizedUserMessage & { type: 'user' }
  >()

  for (const { message: msg } of messages) {
    if (msg.type === 'assistant') {
      for (const content of msg.message.content) {
        if (content.type === 'tool_use') {
          toolUseByToolUseID.set(content.id, content as ToolUseBlockParam)
        }
      }
    } else if (msg.type === 'user') {
      for (const content of Array.isArray(msg.message.content) ? msg.message.content : []) {
        if (typeof content !== 'string' && content.type === 'tool_result') {
          resolvedToolUseIDs.add(content.tool_use_id)
          toolResultByToolUseID.set(content.tool_use_id, msg)
        }
      }
    }
  }

  const inProgressToolUseIDs = new Set<string>()
  for (const id of toolUseByToolUseID.keys()) {
    if (!resolvedToolUseIDs.has(id)) {
      inProgressToolUseIDs.add(id)
    }
  }

  return {
    lookups: {
      ...EMPTY_LOOKUPS,
      toolUseByToolUseID,
      resolvedToolUseIDs,
      toolResultByToolUseID,
    },
    inProgressToolUseIDs,
  }
}

// ---------------------------------------------------------------------------
// Lookup accessors
// ---------------------------------------------------------------------------

/**
 * Get sibling tool use IDs using pre-computed lookup. O(1).
 */
export function getSiblingToolUseIDsFromLookup(
  message: NormalizedMessage,
  lookups: MessageLookups,
): ReadonlySet<string> {
  const toolUseID = getToolUseID(message)
  if (!toolUseID) {
    return EMPTY_STRING_SET
  }
  return lookups.siblingToolUseIDs.get(toolUseID) ?? EMPTY_STRING_SET
}

/**
 * Get progress messages for a message using pre-computed lookup. O(1).
 */
export function getProgressMessagesFromLookup(
  message: NormalizedMessage,
  lookups: MessageLookups,
): ProgressMessage[] {
  const toolUseID = getToolUseID(message)
  if (!toolUseID) {
    return []
  }
  return lookups.progressMessagesByToolUseID.get(toolUseID) ?? []
}

/**
 * Check for unresolved hooks using pre-computed lookup. O(1).
 */
export function hasUnresolvedHooksFromLookup(
  toolUseID: string,
  hookEvent: HookEvent,
  lookups: MessageLookups,
): boolean {
  const inProgressCount =
    lookups.inProgressHookCounts.get(toolUseID)?.get(hookEvent) ?? 0
  const resolvedCount =
    lookups.resolvedHookCounts.get(toolUseID)?.get(hookEvent) ?? 0
  return inProgressCount > resolvedCount
}

/**
 * Get tool use IDs from normalized messages.
 */
export function getToolUseIDs(
  normalizedMessages: NormalizedMessage[],
): Set<string> {
  return new Set(
    normalizedMessages
      .filter(
        (_): _ is NormalizedAssistantMessage<BetaToolUseBlock> =>
          _.type === 'assistant' &&
          Array.isArray(_.message.content) &&
          _.message.content[0]?.type === 'tool_use',
      )
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      .map(_ => (_.message.content[0] as any).id),
  )
}

// ---------------------------------------------------------------------------
// Non-lookup equivalents
// ---------------------------------------------------------------------------

export function hasUnresolvedHooks(
  messages: NormalizedMessage[],
  toolUseID: string,
  hookEvent: HookEvent,
) {
  const inProgressHookCount = getInProgressHookCount(
    messages,
    toolUseID,
    hookEvent,
  )
  const resolvedHookCount = getResolvedHookCount(messages, toolUseID, hookEvent)

  if (inProgressHookCount > resolvedHookCount) {
    return true
  }

  return false
}

function getInProgressHookCount(
  messages: NormalizedMessage[],
  toolUseID: string,
  hookEvent: HookEvent,
): number {
  return count(
    messages,
    _ =>
      _.type === 'progress' &&
      _.data.type === 'hook_progress' &&
      _.data.hookEvent === hookEvent &&
      _.parentToolUseID === toolUseID,
  )
}

function getResolvedHookCount(
  messages: NormalizedMessage[],
  toolUseID: string,
  hookEvent: HookEvent,
): number {
  const uniqueHookNames = new Set(
    messages
      .filter(
        (_): _ is AttachmentMessage<HookAttachmentWithName> =>
          isHookAttachmentMessage(_) &&
          _.attachment.toolUseID === toolUseID &&
          _.attachment.hookEvent === hookEvent,
      )
      .map(_ => _.attachment.hookName),
  )
  return uniqueHookNames.size
}

export function getSiblingToolUseIDs(
  message: NormalizedMessage,
  messages: Message[],
): Set<string> {
  const toolUseID = getToolUseID(message)
  if (!toolUseID) {
    return new Set()
  }

  const unnormalizedMessage = messages.find(
    (_): _ is AssistantMessage =>
      _.type === 'assistant' &&
      _.message.content.some(_ => _.type === 'tool_use' && _.id === toolUseID),
  )
  if (!unnormalizedMessage) {
    return new Set()
  }

  const messageID = unnormalizedMessage.message.id
  const siblingMessages = messages.filter(
    (_): _ is AssistantMessage =>
      _.type === 'assistant' && _.message.id === messageID,
  )

  return new Set(
    siblingMessages.flatMap(_ =>
      _.message.content.filter(_ => _.type === 'tool_use').map(_ => _.id),
    ),
  )
}

// ---------------------------------------------------------------------------
// UI display helpers
// ---------------------------------------------------------------------------

export function shouldShowUserMessage(
  message: NormalizedMessage,
  isTranscriptMode: boolean,
): boolean {
  if (message.type !== 'user') return true
  if (message.isMeta) {
    if (
      (feature('KAIROS') || feature('KAIROS_CHANNELS')) &&
      message.origin?.kind === 'channel'
    )
      return true
    return false
  }
  if (message.isVisibleInTranscriptOnly && !isTranscriptMode) return false
  return true
}

export function isEmptyMessageText(text: string): boolean {
  return (
    stripPromptXMLTags(text).trim() === '' || text.trim() === NO_CONTENT_MESSAGE
  )
}

const STRIPPED_TAGS_RE =
  /<(commit_analysis|context|function_analysis|pr_analysis)>.*?<\/\1>\n?/gs

export function stripPromptXMLTags(content: string): string {
  return content.replace(STRIPPED_TAGS_RE, '').trim()
}

export function textForResubmit(
  msg: UserMessage,
): { text: string; mode: 'bash' | 'prompt' } | null {
  const content = getUserMessageText(msg)
  if (content === null) return null
  const bash = extractTag(content, 'bash-input')
  if (bash) return { text: bash, mode: 'bash' }
  const cmd = extractTag(content, COMMAND_NAME_TAG)
  if (cmd) {
    const args = extractTag(content, COMMAND_ARGS_TAG) ?? ''
    return { text: `${cmd} ${args}`, mode: 'prompt' }
  }
  return { text: stripIdeContextTags(content), mode: 'prompt' }
}

// ---------------------------------------------------------------------------
// Classifier denial helpers
// ---------------------------------------------------------------------------

// Prefix used by UI to detect classifier denials and render them concisely
const AUTO_MODE_REJECTION_PREFIX =
  'Permission for this action has been denied. Reason: '

/**
 * Check if a tool result message is a classifier denial.
 */
export function isClassifierDenial(content: string): boolean {
  return content.startsWith(AUTO_MODE_REJECTION_PREFIX)
}

/**
 * Build a rejection message for auto mode classifier denials.
 */
export function buildYoloRejectionMessage(reason: string): string {
  const prefix = AUTO_MODE_REJECTION_PREFIX

  const ruleHint = feature('BASH_CLASSIFIER')
    ? `To allow this type of action in the future, the user can add a permission rule like ` +
      `Bash(prompt: <description of allowed action>) to their settings. ` +
      `At the end of your session, recommend what permission rules to add so you don't get blocked again.`
    : `To allow this type of action in the future, the user can add a Bash permission rule to their settings.`

  return (
    `${prefix}${reason}. ` +
    `If you have other tasks that don't depend on this action, continue working on those. ` +
    `${DENIAL_WORKAROUND_GUIDANCE} ` +
    ruleHint
  )
}

/**
 * Build a message for when the auto mode classifier is temporarily unavailable.
 */
export function buildClassifierUnavailableMessage(
  toolName: string,
  classifierModel: string,
): string {
  return (
    `${classifierModel} is temporarily unavailable, so auto mode cannot determine the safety of ${toolName} right now. ` +
    `Wait briefly and then try this action again. ` +
    `If it keeps failing, continue with other tasks that don't require this action and come back to it later. ` +
    `Note: reading files, searching code, and other read-only operations do not require the classifier and can still be used.`
  )
}

// ---------------------------------------------------------------------------
// Plan phase 4 control (exported for normalize.ts)
// ---------------------------------------------------------------------------

export const PLAN_PHASE4_CONTROL = `### Phase 4: Final Plan
Goal: Write your final plan to the plan file (the only file you can edit).
- Begin with a **Context** section: explain why this change is being made — the problem or need it addresses, what prompted it, and the intended outcome
- Include only your recommended approach, not all alternatives
- Ensure that the plan file is concise enough to scan quickly, but detailed enough to execute effectively
- Include the paths of critical files to be modified
- Reference existing functions and utilities you found that should be reused, with their file paths
- Include a verification section describing how to test the changes end-to-end (run the code, use MCP tools, run tests)`
