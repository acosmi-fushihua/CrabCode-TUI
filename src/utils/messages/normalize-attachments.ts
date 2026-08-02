/**
 * normalize-attachments.ts -- Attachment normalization for the API.
 *
 * Contains the large normalizeAttachmentForAPI function (~800 lines)
 * which converts Attachment objects into UserMessage arrays for the API.
 */

import type { ContentBlockParam, TextBlockParam } from '../../types/api-types.js'
import { feature } from '../featurePolyfill.js'

import { AGENT_TOOL_NAME } from 'src/tools/AgentTool/constants.js'
import { BashTool } from 'src/tools/BashTool/BashTool.js'
import { ExitPlanModeV2Tool } from 'src/tools/ExitPlanModeTool/ExitPlanModeV2Tool.js'
import {
  FILE_READ_TOOL_NAME,
  MAX_LINES_TO_READ,
} from 'src/tools/FileReadTool/prompt.js'

import { companionIntroText } from '../../buddy/prompt.js'
import { OUTPUT_STYLE_CONFIG } from '../../constants/outputStyles.js'
import { DiagnosticTrackingService } from '../../services/diagnosticTracking.js'
import {
  FileReadTool,
  type Output as FileReadToolOutput,
} from '../../tools/FileReadTool/FileReadTool.js'
import { SEND_MESSAGE_TOOL_NAME } from '../../tools/SendMessageTool/constants.js'
import { TASK_CREATE_TOOL_NAME } from '../../tools/TaskCreateTool/constants.js'
import { TASK_OUTPUT_TOOL_NAME } from '../../tools/TaskOutputTool/constants.js'
import { TASK_UPDATE_TOOL_NAME } from '../../tools/TaskUpdateTool/constants.js'
import { TOOL_SEARCH_TOOL_NAME } from '../../tools/ToolSearchTool/constants.js'
import type {
  MessageOrigin,
  UserMessage,
} from '../../types/message.js'
import { isAgentSwarmsEnabled } from '../agentSwarmsEnabled.js'
import type {
  Attachment,
} from '../attachments.js'
import { memoryHeader } from '../attachments.js'
import { quote } from '../bash/shellQuote.js'
import { logAntError } from '../debug.js'
import { formatFileSize, formatNumber } from '../format.js'
import { logMCPDebug } from '../log.js'
import { isTodoV2Enabled } from '../tasks.js'

import {
  createUserMessage,
  wrapCommandText,
} from './factory.js'

import {
  wrapInSystemReminder,
  wrapMessagesInSystemReminder,
  createToolResultMessage,
  createToolUseMessage,
  getPlanModeInstructions,
  getAutoModeInstructions,
} from './normalize-compact.js'

// Lazy import to avoid circular dependency (teammateMailbox -> teammate -> ... -> messages)
function getTeammateMailbox(): typeof import('../teammateMailbox.js') {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  return require('../teammateMailbox.js')
}

// ---------------------------------------------------------------------------
// normalizeAttachmentForAPI
// ---------------------------------------------------------------------------

export function normalizeAttachmentForAPI(
  attachment: Attachment,
): UserMessage[] {
  if (isAgentSwarmsEnabled()) {
    if (attachment.type === 'teammate_mailbox') {
      return [
        createUserMessage({
          content: getTeammateMailbox().formatTeammateMessages(
            attachment.messages,
          ),
          isMeta: true,
        }),
      ]
    }
    if (attachment.type === 'team_context') {
      return [
        createUserMessage({
          content: wrapInSystemReminder(`
# Team Coordination

You are a teammate in team "${attachment.teamName}".

**Your Identity:**
- Name: ${attachment.agentName}

**Team Resources:**
- Team config: ${attachment.teamConfigPath}
- Task list: ${attachment.taskListPath}

**Team Leader:** The team lead's name is "team-lead". Send updates and completion notifications to them.

Read the team config to discover your teammates' names. Check the task list periodically. Create new tasks when work should be divided. Mark tasks resolved when complete.

**IMPORTANT:** Always refer to teammates by their NAME (e.g., "team-lead", "analyzer", "researcher"), never by UUID. When messaging, use the name directly:

\`\`\`json
{
  "to": "team-lead",
  "message": "Your message here",
  "summary": "Brief 5-10 word preview"
}
\`\`\`
`),
          isMeta: true,
        }),
      ]
    }
  }

  if (feature('EXPERIMENTAL_SKILL_SEARCH')) {
    if (attachment.type === 'skill_discovery') {
      if (attachment.skills.length === 0) return []
      const lines = attachment.skills.map(s => `- ${s.name}: ${s.description}`)
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content:
            `Skills relevant to your task:\n\n${lines.join('\n')}\n\n` +
            `These skills encode project-specific conventions. ` +
            `Invoke via Skill("<name>") for complete instructions.`,
          isMeta: true,
        }),
      ])
    }
  }

  // eslint-disable-next-line @typescript-eslint/switch-exhaustiveness-check
  switch (attachment.type) {
    case 'directory': {
      return wrapMessagesInSystemReminder([
        createToolUseMessage(BashTool.name, {
          command: `ls ${quote([attachment.path])}`,
          description: `Lists files in ${attachment.path}`,
        }),
        createToolResultMessage(BashTool, {
          stdout: attachment.content,
          stderr: '',
          interrupted: false,
        }),
      ])
    }
    case 'edited_text_file':
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `Note: ${attachment.filename} was modified, either by the user or by a linter. This change was intentional, so make sure to take it into account as you proceed (ie. don't revert it unless the user asks you to). Don't tell the user this, since they are already aware. Here are the relevant changes (shown with line numbers):\n${attachment.snippet}`,
          isMeta: true,
        }),
      ])
    case 'file': {
      const fileContent = attachment.content as FileReadToolOutput
      switch (fileContent.type) {
        case 'image': {
          return wrapMessagesInSystemReminder([
            createToolUseMessage(FileReadTool.name, {
              file_path: attachment.filename,
            }),
            createToolResultMessage(FileReadTool, fileContent),
          ])
        }
        case 'text': {
          return wrapMessagesInSystemReminder([
            createToolUseMessage(FileReadTool.name, {
              file_path: attachment.filename,
            }),
            createToolResultMessage(FileReadTool, fileContent),
            ...(attachment.truncated
              ? [
                  createUserMessage({
                    content: `Note: The file ${attachment.filename} was too large and has been truncated to the first ${MAX_LINES_TO_READ} lines. Don't tell the user about this truncation. Use ${FileReadTool.name} to read more of the file if you need.`,
                    isMeta: true,
                  }),
                ]
              : []),
          ])
        }
        case 'notebook': {
          return wrapMessagesInSystemReminder([
            createToolUseMessage(FileReadTool.name, {
              file_path: attachment.filename,
            }),
            createToolResultMessage(FileReadTool, fileContent),
          ])
        }
        case 'pdf': {
          return wrapMessagesInSystemReminder([
            createToolUseMessage(FileReadTool.name, {
              file_path: attachment.filename,
            }),
            createToolResultMessage(FileReadTool, fileContent),
          ])
        }
      }
      break
    }
    case 'compact_file_reference': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `Note: ${attachment.filename} was read before the last conversation was summarized, but the contents are too large to include. Use ${FileReadTool.name} tool if you need to access it.`,
          isMeta: true,
        }),
      ])
    }
    case 'pdf_reference': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content:
            `PDF file: ${attachment.filename} (${attachment.pageCount} pages, ${formatFileSize(attachment.fileSize)}). ` +
            `This PDF is too large to read all at once. You MUST use the ${FILE_READ_TOOL_NAME} tool with the pages parameter ` +
            `to read specific page ranges (e.g., pages: "1-5"). Do NOT call ${FILE_READ_TOOL_NAME} without the pages parameter ` +
            `or it will fail. Start by reading the first few pages to understand the structure, then read more as needed. ` +
            `Maximum 20 pages per request.`,
          isMeta: true,
        }),
      ])
    }
    case 'selected_lines_in_ide': {
      const maxSelectionLength = 2000
      const content =
        attachment.content.length > maxSelectionLength
          ? attachment.content.substring(0, maxSelectionLength) +
            '\n... (truncated)'
          : attachment.content

      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The user selected the lines ${attachment.lineStart} to ${attachment.lineEnd} from ${attachment.filename}:\n${content}\n\nThis may or may not be related to the current task.`,
          isMeta: true,
        }),
      ])
    }
    case 'opened_file_in_ide': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The user opened the file ${attachment.filename} in the IDE. This may or may not be related to the current task.`,
          isMeta: true,
        }),
      ])
    }
    case 'plan_file_reference': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `A plan file exists from plan mode at: ${attachment.planFilePath}\n\nPlan contents:\n\n${attachment.planContent}\n\nIf this plan is relevant to the current work and not already complete, continue working on it.`,
          isMeta: true,
        }),
      ])
    }
    case 'invoked_skills': {
      if (attachment.skills.length === 0) {
        return []
      }

      const skillsContent = attachment.skills
        .map(
          skill =>
            `### Skill: ${skill.name}\nPath: ${skill.path}\n\n${skill.content}`,
        )
        .join('\n\n---\n\n')

      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The following skills were invoked in this session. Continue to follow these guidelines:\n\n${skillsContent}`,
          isMeta: true,
        }),
      ])
    }
    case 'todo_reminder': {
      const todoItems = attachment.content
        .map((todo, index) => `${index + 1}. [${todo.status}] ${todo.content}`)
        .join('\n')

      let message = `The TodoWrite tool hasn't been used recently. If you're working on tasks that would benefit from tracking progress, consider using the TodoWrite tool to track progress. Also consider cleaning up the todo list if has become stale and no longer matches what you are working on. Only use it if it's relevant to the current work. This is just a gentle reminder - ignore if not applicable. Make sure that you NEVER mention this reminder to the user\n`
      if (todoItems.length > 0) {
        message += `\n\nHere are the existing contents of your todo list:\n\n[${todoItems}]`
      }

      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: message,
          isMeta: true,
        }),
      ])
    }
    case 'task_reminder': {
      if (!isTodoV2Enabled()) {
        return []
      }
      const taskItems = attachment.content
        .map(task => `#${task.id}. [${task.status}] ${task.subject}`)
        .join('\n')

      let message = `The task tools haven't been used recently. If you're working on tasks that would benefit from tracking progress, consider using ${TASK_CREATE_TOOL_NAME} to add new tasks and ${TASK_UPDATE_TOOL_NAME} to update task status (set to in_progress when starting, completed when done). Also consider cleaning up the task list if it has become stale. Only use these if relevant to the current work. This is just a gentle reminder - ignore if not applicable. Make sure that you NEVER mention this reminder to the user\n`
      if (taskItems.length > 0) {
        message += `\n\nHere are the existing tasks:\n\n${taskItems}`
      }

      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: message,
          isMeta: true,
        }),
      ])
    }
    case 'nested_memory': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `Contents of ${attachment.content.path}:\n\n${attachment.content.content}`,
          isMeta: true,
        }),
      ])
    }
    case 'relevant_memories': {
      return wrapMessagesInSystemReminder(
        attachment.memories.map(m => {
          const header = m.header ?? memoryHeader(m.path, m.mtimeMs)
          return createUserMessage({
            content: `${header}\n\n${m.content}`,
            isMeta: true,
          })
        }),
      )
    }
    case 'dynamic_skill': {
      return []
    }
    case 'skill_listing': {
      if (!attachment.content) {
        return []
      }
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The following skills are available for use with the Skill tool:\n\n${attachment.content}`,
          isMeta: true,
        }),
      ])
    }
    case 'queued_command': {
      const origin: MessageOrigin | undefined =
        attachment.origin ??
        (attachment.commandMode === 'task-notification'
          ? { kind: 'task-notification' }
          : undefined)

      const metaProp =
        origin !== undefined || attachment.isMeta
          ? ({ isMeta: true } as const)
          : {}

      if (Array.isArray(attachment.prompt)) {
        const textContent = attachment.prompt
          .filter((block): block is TextBlockParam => block.type === 'text')
          .map(block => block.text)
          .join('\n')

        const imageBlocks = attachment.prompt.filter(
          block => block.type === 'image',
        )

        const content: ContentBlockParam[] = [
          {
            type: 'text',
            text: wrapCommandText(textContent, origin),
          },
          ...imageBlocks,
        ]

        return wrapMessagesInSystemReminder([
          createUserMessage({
            content,
            ...metaProp,
            origin,
            uuid: attachment.source_uuid,
          }),
        ])
      }

      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: wrapCommandText(String(attachment.prompt), origin),
          ...metaProp,
          origin,
          uuid: attachment.source_uuid,
        }),
      ])
    }
    case 'output_style': {
      const outputStyle =
        OUTPUT_STYLE_CONFIG[
          attachment.style as keyof typeof OUTPUT_STYLE_CONFIG
        ]
      if (!outputStyle) {
        return []
      }
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `${outputStyle.name} output style is active. Remember to follow the specific guidelines for this style.`,
          isMeta: true,
        }),
      ])
    }
    case 'diagnostics': {
      if (attachment.files.length === 0) return []

      const diagnosticSummary =
        DiagnosticTrackingService.formatDiagnosticsSummary(attachment.files)

      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `<new-diagnostics>The following new diagnostic issues were detected:\n\n${diagnosticSummary}</new-diagnostics>`,
          isMeta: true,
        }),
      ])
    }
    case 'plan_mode': {
      return getPlanModeInstructions(attachment)
    }
    case 'plan_mode_reentry': {
      const content = `## Re-entering Plan Mode

You are returning to plan mode after having previously exited it. A plan file exists at ${attachment.planFilePath} from your previous planning session.

**Before proceeding with any new planning, you should:**
1. Read the existing plan file to understand what was previously planned
2. Evaluate the user's current request against that plan
3. Decide how to proceed:
   - **Different task**: If the user's request is for a different task—even if it's similar or related—start fresh by overwriting the existing plan
   - **Same task, continuing**: If this is explicitly a continuation or refinement of the exact same task, modify the existing plan while cleaning up outdated or irrelevant sections
4. Continue on with the plan process and most importantly you should always edit the plan file one way or the other before calling ${ExitPlanModeV2Tool.name}

Treat this as a fresh planning session. Do not assume the existing plan is relevant without evaluating it first.`

      return wrapMessagesInSystemReminder([
        createUserMessage({ content, isMeta: true }),
      ])
    }
    case 'plan_mode_exit': {
      const planReference = attachment.planExists
        ? ` The plan file is located at ${attachment.planFilePath} if you need to reference it.`
        : ''
      const content = `## Exited Plan Mode

You have exited plan mode. You can now make edits, run tools, and take actions.${planReference}`

      return wrapMessagesInSystemReminder([
        createUserMessage({ content, isMeta: true }),
      ])
    }
    case 'auto_mode': {
      return getAutoModeInstructions(attachment)
    }
    case 'auto_mode_exit': {
      const content = `## Exited Auto Mode

You have exited auto mode. The user may now want to interact more directly. You should ask clarifying questions when the approach is ambiguous rather than making assumptions.`

      return wrapMessagesInSystemReminder([
        createUserMessage({ content, isMeta: true }),
      ])
    }
    case 'critical_system_reminder': {
      // W-PROMPT-SYSTEM-ROOTCAUSE PR-7 (audit D-3): trusted-source
      // allowlist. The reminder content is a high-authority instruction
      // that re-injects on every user turn; only `'built-in'`
      // (CrabCode-authored agents) and `'managed'` (policy/enterprise
      // settings) may inject. Other sources (user-installed agents,
      // plugin agents, project/local agents) are dropped from the
      // injection pipeline; their reminder text was already untrusted
      // text supplied via Markdown frontmatter.
      //
      // Missing source is not trusted. Producers must tag built-in or
      // managed policy reminders explicitly before they can enter the
      // high-authority reminder lane.
      const source = attachment.trustedSource
      if (source !== 'built-in' && source !== 'managed') {
        return []
      }
      // Even trusted sources are routed through wrapInSystemReminder
      // (inside wrapMessagesInSystemReminder), which neutralizes any
      // `</system-reminder>` / `[PROJECT_RULE]` / hook-tag literals
      // appearing inside the reminder text.
      return wrapMessagesInSystemReminder([
        createUserMessage({ content: attachment.content, isMeta: true }),
      ])
    }
    case 'mcp_resource': {
      const content = attachment.content
      if (!content || !content.contents || content.contents.length === 0) {
        return wrapMessagesInSystemReminder([
          createUserMessage({
            content: `<mcp-resource server="${attachment.server}" uri="${attachment.uri}">(No content)</mcp-resource>`,
            isMeta: true,
          }),
        ])
      }

      const transformedBlocks: ContentBlockParam[] = []

      for (const item of content.contents) {
        if (item && typeof item === 'object') {
          if ('text' in item && typeof item.text === 'string') {
            transformedBlocks.push(
              {
                type: 'text',
                text: 'Full contents of resource:',
              },
              {
                type: 'text',
                text: item.text,
              },
              {
                type: 'text',
                text: 'Do NOT read this resource again unless you think it may have changed, since you already have the full contents.',
              },
            )
          } else if ('blob' in item) {
            const mimeType =
              'mimeType' in item
                ? String(item.mimeType)
                : 'application/octet-stream'
            transformedBlocks.push({
              type: 'text',
              text: `[Binary content: ${mimeType}]`,
            })
          }
        }
      }

      if (transformedBlocks.length > 0) {
        return wrapMessagesInSystemReminder([
          createUserMessage({
            content: transformedBlocks,
            isMeta: true,
          }),
        ])
      } else {
        logMCPDebug(
          attachment.server,
          `No displayable content found in MCP resource ${attachment.uri}.`,
        )
        return wrapMessagesInSystemReminder([
          createUserMessage({
            content: `<mcp-resource server="${attachment.server}" uri="${attachment.uri}">(No displayable content)</mcp-resource>`,
            isMeta: true,
          }),
        ])
      }
    }
    case 'agent_mention': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The user has expressed a desire to invoke the agent "${attachment.agentType}". Please invoke the agent appropriately, passing in the required context to it. `,
          isMeta: true,
        }),
      ])
    }
    case 'task_status': {
      const displayStatus =
        attachment.status === 'killed' ? 'stopped' : attachment.status

      if (attachment.status === 'killed') {
        return [
          createUserMessage({
            content: wrapInSystemReminder(
              `Task "${attachment.description}" (${attachment.taskId}) was stopped by the user.`,
            ),
            isMeta: true,
          }),
        ]
      }

      if (attachment.status === 'running') {
        const parts = [
          `Background agent "${attachment.description}" (${attachment.taskId}) is still running.`,
        ]
        if (attachment.deltaSummary) {
          parts.push(`Progress: ${attachment.deltaSummary}`)
        }
        if (attachment.outputFilePath) {
          parts.push(
            `Do NOT spawn a duplicate. You will be notified when it completes. You can read partial output at ${attachment.outputFilePath} or send it a message with ${SEND_MESSAGE_TOOL_NAME}.`,
          )
        } else {
          parts.push(
            `Do NOT spawn a duplicate. You will be notified when it completes. You can check its progress with the ${TASK_OUTPUT_TOOL_NAME} tool or send it a message with ${SEND_MESSAGE_TOOL_NAME}.`,
          )
        }
        return [
          createUserMessage({
            content: wrapInSystemReminder(parts.join(' ')),
            isMeta: true,
          }),
        ]
      }

      const messageParts: string[] = [
        `Task ${attachment.taskId}`,
        `(type: ${attachment.taskType})`,
        `(status: ${displayStatus})`,
        `(description: ${attachment.description})`,
      ]

      if (attachment.deltaSummary) {
        messageParts.push(`Delta: ${attachment.deltaSummary}`)
      }

      if (attachment.outputFilePath) {
        messageParts.push(
          `Read the output file to retrieve the result: ${attachment.outputFilePath}`,
        )
      } else {
        messageParts.push(
          `You can check its output using the ${TASK_OUTPUT_TOOL_NAME} tool.`,
        )
      }

      return [
        createUserMessage({
          content: wrapInSystemReminder(messageParts.join(' ')),
          isMeta: true,
        }),
      ]
    }
    case 'async_hook_response': {
      const response = attachment.response
      const messages: UserMessage[] = []

      if (response.systemMessage) {
        messages.push(
          createUserMessage({
            content: response.systemMessage,
            isMeta: true,
          }),
        )
      }

      if (
        response.hookSpecificOutput &&
        'additionalContext' in response.hookSpecificOutput &&
        response.hookSpecificOutput.additionalContext
      ) {
        messages.push(
          createUserMessage({
            content: response.hookSpecificOutput.additionalContext,
            isMeta: true,
          }),
        )
      }

      return wrapMessagesInSystemReminder(messages)
    }
    case 'token_usage':
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `Token usage: ${attachment.used}/${attachment.total}; ${attachment.remaining} remaining`,
          ),
          isMeta: true,
        }),
      ]
    case 'budget_usd':
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `USD budget: $${attachment.used}/$${attachment.total}; $${attachment.remaining} remaining`,
          ),
          isMeta: true,
        }),
      ]
    case 'output_token_usage': {
      const turnText =
        attachment.budget !== null
          ? `${formatNumber(attachment.turn)} / ${formatNumber(attachment.budget)}`
          : formatNumber(attachment.turn)
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `Output tokens \u2014 turn: ${turnText} \u00b7 session: ${formatNumber(attachment.session)}`,
          ),
          isMeta: true,
        }),
      ]
    }
    case 'hook_blocking_error':
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `${attachment.hookName} hook blocking error from command: "${attachment.blockingError.command}": ${attachment.blockingError.blockingError}`,
          ),
          isMeta: true,
        }),
      ]
    case 'hook_success':
      if (
        attachment.hookEvent !== 'SessionStart' &&
        attachment.hookEvent !== 'UserPromptSubmit'
      ) {
        return []
      }
      if (attachment.content === '') {
        return []
      }
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `${attachment.hookName} hook success: ${attachment.content}`,
          ),
          isMeta: true,
        }),
      ]
    case 'hook_additional_context': {
      if (attachment.content.length === 0) {
        return []
      }
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `${attachment.hookName} hook additional context: ${attachment.content.join('\n')}`,
          ),
          isMeta: true,
        }),
      ]
    }
    case 'hook_stopped_continuation':
      return [
        createUserMessage({
          content: wrapInSystemReminder(
            `${attachment.hookName} hook stopped continuation: ${attachment.message}`,
          ),
          isMeta: true,
        }),
      ]
    case 'compaction_reminder': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content:
            'Auto-compact is enabled. When the context window is nearly full, older messages will be automatically summarized so you can continue working seamlessly. There is no need to stop or rush \u2014 you have unlimited context through automatic compaction.',
          isMeta: true,
        }),
      ])
    }
    case 'context_efficiency': {
      if (feature('HISTORY_SNIP')) {
        const { SNIP_NUDGE_TEXT } =
          // eslint-disable-next-line @typescript-eslint/no-require-imports
          require('../../services/compact/snipCompact.js') as typeof import('../../services/compact/snipCompact.js')
        return wrapMessagesInSystemReminder([
          createUserMessage({
            content: SNIP_NUDGE_TEXT,
            isMeta: true,
          }),
        ])
      }
      return []
    }
    case 'date_change': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The date has changed. Today's date is now ${attachment.newDate}. DO NOT mention this to the user explicitly because they are already aware.`,
          isMeta: true,
        }),
      ])
    }
    case 'ultrathink_effort': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: `The user has requested reasoning effort level: ${attachment.level}. Apply this to the current turn.`,
          isMeta: true,
        }),
      ])
    }
    case 'deferred_tools_delta': {
      const parts: string[] = []
      if (attachment.addedLines.length > 0) {
        // CrabCode does not know upstream identity through the acosmi
        // gateway; the protocol contract is included unconditionally —
        // redundant for upstreams that internalize the deferred-tool
        // convention, load-bearing for those that do not.
        parts.push(
          `The following deferred tools are now available via ${TOOL_SEARCH_TOOL_NAME}. ` +
            `Their schemas are NOT loaded — calling them directly will fail with InputValidationError. ` +
            `You MUST first call ${TOOL_SEARCH_TOOL_NAME} with input { "query": "select:<tool_name>" } ` +
            `to load each tool's schema before invoking it.\n` +
            `${attachment.addedLines.join('\n')}`,
        )
      }
      if (attachment.removedNames.length > 0) {
        parts.push(
          `The following deferred tools are no longer available (their MCP server disconnected). Do not search for them — ${TOOL_SEARCH_TOOL_NAME} will return no match:\n${attachment.removedNames.join('\n')}`,
        )
      }
      return wrapMessagesInSystemReminder([
        createUserMessage({ content: parts.join('\n\n'), isMeta: true }),
      ])
    }
    case 'agent_listing_delta': {
      const parts: string[] = []
      if (attachment.addedLines.length > 0) {
        const header = attachment.isInitial
          ? 'Available agent types for the Agent tool:'
          : 'New agent types are now available for the Agent tool:'
        parts.push(`${header}\n${attachment.addedLines.join('\n')}`)
      }
      if (attachment.removedTypes.length > 0) {
        parts.push(
          `The following agent types are no longer available:\n${attachment.removedTypes.map(t => `- ${t}`).join('\n')}`,
        )
      }
      if (attachment.isInitial && attachment.showConcurrencyNote) {
        parts.push(
          `Launch multiple agents concurrently whenever possible, to maximize performance; to do that, use a single message with multiple tool uses (the client throttles concurrent agent dispatches; submitting more queues, not fails).`,
        )
      }
      return wrapMessagesInSystemReminder([
        createUserMessage({ content: parts.join('\n\n'), isMeta: true }),
      ])
    }
    case 'mcp_instructions_delta': {
      const parts: string[] = []
      if (attachment.addedBlocks.length > 0) {
        parts.push(
          `# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n${attachment.addedBlocks.join('\n\n')}`,
        )
      }
      if (attachment.removedNames.length > 0) {
        parts.push(
          `The following MCP servers have disconnected. Their instructions above no longer apply:\n${attachment.removedNames.join('\n')}`,
        )
      }
      return wrapMessagesInSystemReminder([
        createUserMessage({ content: parts.join('\n\n'), isMeta: true }),
      ])
    }
    case 'companion_intro': {
      return wrapMessagesInSystemReminder([
        createUserMessage({
          content: companionIntroText(attachment.name, attachment.species),
          isMeta: true,
        }),
      ])
    }
    case 'verify_plan_reminder': {
      /* eslint-disable-next-line custom-rules/no-process-env-top-level */
      const toolName =
        process.env.CRABCODE_VERIFY_PLAN === 'true'
          ? 'VerifyPlanExecution'
          : ''
      const content = `You have completed implementing the plan. Please call the "${toolName}" tool directly (NOT the ${AGENT_TOOL_NAME} tool or an agent) to verify that all plan items were completed correctly.`
      return wrapMessagesInSystemReminder([
        createUserMessage({ content, isMeta: true }),
      ])
    }
    case 'already_read_file':
    case 'command_permissions':
    case 'edited_image_file':
    case 'hook_cancelled':
    case 'hook_error_during_execution':
    case 'hook_non_blocking_error':
    case 'hook_system_message':
    case 'structured_output':
    case 'hook_permission_decision':
      return []
  }

  // Handle legacy attachments that were removed
  const LEGACY_ATTACHMENT_TYPES = [
    'autocheckpointing',
    'background_task_status',
    'todo',
    'task_progress',
    'ultramemory',
  ]
  if (LEGACY_ATTACHMENT_TYPES.includes((attachment as { type: string }).type)) {
    return []
  }

  logAntError(
    'normalizeAttachmentForAPI',
    new Error(
      `Unknown attachment type: ${(attachment as { type: string }).type}`,
    ),
  )
  return []
}
