import { basename } from 'path'
import type { CanUseToolFn } from '../../../types/canUseTool.js'
import { ENTRYPOINT_NAME } from '../../../memdir/memdir.js'
import {
  formatMemoryManifest,
  scanMemoryFiles,
} from '../../../memdir/memoryScan.js'
import { isAutoMemoryEnabled, isAutoMemPath } from '../../../memdir/paths.js'
import { isEnvTruthy } from '../../../utils/envUtils.js'
import type { Tool } from '../../../Tool.js'
import { BASH_TOOL_NAME } from '../../../tools/BashTool/toolName.js'
import { FILE_EDIT_TOOL_NAME } from '../../../tools/FileEditTool/constants.js'
import { FILE_READ_TOOL_NAME } from '../../../tools/FileReadTool/prompt.js'
import { FILE_WRITE_TOOL_NAME } from '../../../tools/FileWriteTool/prompt.js'
import { GLOB_TOOL_NAME } from '../../../tools/GlobTool/prompt.js'
import { GREP_TOOL_NAME } from '../../../tools/GrepTool/prompt.js'
import { REPL_TOOL_NAME } from '../../../tools/REPLTool/constants.js'
import type { AssistantMessage, Message } from '../../../types/message.js'
import { createAbortController } from '../../../utils/abortController.js'
import { count, uniq } from '../../../utils/array.js'
import { logForDebugging } from '../../../utils/debug.js'
import {
  type CacheSafeParams,
  runForkedAgent,
} from '../../../utils/forkedAgent.js'
import {
  createMemorySavedMessage,
  createUserMessage,
} from '../../../utils/messages.js'
import { feature } from '../../../utils/featurePolyfill.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../../analytics/growthbook.js'
import { logEvent } from '../../analytics/index.js'
import { sanitizeToolNameForAnalytics } from '../../analytics/metadata.js'
import {
  buildExtractAutoOnlyPrompt,
  buildExtractCombinedPrompt,
} from '../../extractMemories/prompts.js'
import type {
  MemoryTriggerResult,
  MemoryTriggerRunnerInput,
} from '../turnEnd.js'

/* eslint-disable @typescript-eslint/no-require-imports */
const teamMemPaths = feature('TEAMMEM')
  ? (require('../../../memdir/teamMemPaths.js') as typeof import('../../../memdir/teamMemPaths.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

function isModelVisibleMessage(message: Message): boolean {
  return message.type === 'user' || message.type === 'assistant'
}

function countModelVisibleMessagesSince(
  messages: Message[],
  sinceUuid: string | undefined,
): number {
  if (sinceUuid === undefined) {
    return count(messages, isModelVisibleMessage)
  }

  let foundStart = false
  let n = 0
  for (const message of messages) {
    if (!foundStart) {
      if (message.uuid === sinceUuid) {
        foundStart = true
      }
      continue
    }
    if (isModelVisibleMessage(message)) {
      n++
    }
  }
  return foundStart ? n : count(messages, isModelVisibleMessage)
}

function getWrittenFilePath(block: {
  type: string
  name?: string
  input?: unknown
}): string | undefined {
  if (
    block.type !== 'tool_use' ||
    (block.name !== FILE_EDIT_TOOL_NAME && block.name !== FILE_WRITE_TOOL_NAME)
  ) {
    return undefined
  }
  const input = block.input
  if (typeof input === 'object' && input !== null && 'file_path' in input) {
    const fp = (input as { file_path: unknown }).file_path
    return typeof fp === 'string' ? fp : undefined
  }
  return undefined
}

export function extractWrittenPaths(agentMessages: Message[]): string[] {
  const paths: string[] = []
  for (const message of agentMessages) {
    if (message.type !== 'assistant') {
      continue
    }
    const content = (message as AssistantMessage).message.content
    if (!Array.isArray(content)) {
      continue
    }
    for (const block of content) {
      const filePath = getWrittenFilePath(block)
      if (filePath !== undefined) {
        paths.push(filePath)
      }
    }
  }
  return uniq(paths)
}

function hasMemoryWritesSince(
  messages: Message[],
  sinceUuid: string | undefined,
): boolean {
  let foundStart = sinceUuid === undefined
  for (const message of messages) {
    if (!foundStart) {
      if (message.uuid === sinceUuid) {
        foundStart = true
      }
      continue
    }
    if (message.type !== 'assistant') {
      continue
    }
    const content = (message as AssistantMessage).message.content
    if (!Array.isArray(content)) {
      continue
    }
    for (const block of content) {
      const filePath = getWrittenFilePath(block)
      if (filePath !== undefined && isWritableMemoryPath(filePath)) {
        return true
      }
    }
  }
  return false
}

function isWritableMemoryPath(filePath: string): boolean {
  return (
    isAutoMemPath(filePath) ||
    (feature('TEAMMEM') && (teamMemPaths?.isTeamMemPath(filePath) ?? false))
  )
}

function denyAutoMemTool(tool: Tool, reason: string) {
  logForDebugging(`[autoMem] denied ${tool.name}: ${reason}`)
  logEvent('tengu_auto_mem_tool_denied', {
    tool_name: sanitizeToolNameForAnalytics(tool.name),
  })
  return {
    behavior: 'deny' as const,
    message: reason,
    decisionReason: { type: 'other' as const, reason },
  }
}

export function createAutoMemCanUseTool(memoryDir: string): CanUseToolFn {
  return async (tool: Tool, input: Record<string, unknown>) => {
    if (tool.name === REPL_TOOL_NAME) {
      return { behavior: 'allow' as const, updatedInput: input }
    }

    if (
      tool.name === FILE_READ_TOOL_NAME ||
      tool.name === GREP_TOOL_NAME ||
      tool.name === GLOB_TOOL_NAME
    ) {
      return { behavior: 'allow' as const, updatedInput: input }
    }

    if (tool.name === BASH_TOOL_NAME) {
      const parsed = tool.inputSchema.safeParse(input)
      if (parsed.success && tool.isReadOnly(parsed.data)) {
        return { behavior: 'allow' as const, updatedInput: input }
      }
      return denyAutoMemTool(
        tool,
        'Only read-only shell commands are permitted in this context (ls, find, grep, cat, stat, wc, head, tail, and similar)',
      )
    }

    if (
      (tool.name === FILE_EDIT_TOOL_NAME ||
        tool.name === FILE_WRITE_TOOL_NAME) &&
      'file_path' in input
    ) {
      const filePath = input.file_path
      if (typeof filePath === 'string' && isWritableMemoryPath(filePath)) {
        return { behavior: 'allow' as const, updatedInput: input }
      }
    }

    return denyAutoMemTool(
      tool,
      `only ${FILE_READ_TOOL_NAME}, ${GREP_TOOL_NAME}, ${GLOB_TOOL_NAME}, read-only ${BASH_TOOL_NAME}, and ${FILE_EDIT_TOOL_NAME}/${FILE_WRITE_TOOL_NAME} within ${memoryDir} are allowed`,
    )
  }
}

function stringPayload(
  payload: Record<string, unknown>,
  key: string,
): string | undefined {
  const value = payload[key]
  return typeof value === 'string' ? value : undefined
}

function numberPayload(
  payload: Record<string, unknown>,
  key: string,
): number | undefined {
  const value = payload[key]
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined
}

function booleanPayload(
  payload: Record<string, unknown>,
  key: string,
): boolean | undefined {
  const value = payload[key]
  return typeof value === 'boolean' ? value : undefined
}

function isTeamMemoryEnabled(payload: Record<string, unknown>): boolean {
  return (
    booleanPayload(payload, 'team_memory_enabled') ??
    (feature('TEAMMEM') && (teamMemPaths?.isTeamMemoryEnabled() ?? false))
  )
}

export async function runExtractMemoryTrigger(
  input: MemoryTriggerRunnerInput,
): Promise<MemoryTriggerResult> {
  const {
    context,
    trigger,
    appendSystemMessage,
    cacheSafeParams,
    assertDeliveryAuthority,
  } = input
  const payload = trigger.runner_payload
  const memoryDir = stringPayload(payload, 'memory_dir')
  if (!memoryDir) {
    throw new Error('extract runner missing memory_dir')
  }

  if (
    context.toolUseContext.agentId ||
    !isAutoMemoryEnabled() ||
    isEnvTruthy(process.env.CRABCODE_REMOTE)
  ) {
    return { writtenPaths: [] }
  }

  const previousCursorUuid = stringPayload(payload, 'previous_cursor_uuid')
  const newMessageCount =
    numberPayload(payload, 'new_message_count') ??
    countModelVisibleMessagesSince(context.messages, previousCursorUuid)

  if (hasMemoryWritesSince(context.messages, previousCursorUuid)) {
    logForDebugging(
      '[extractMemories] skipping — conversation already wrote to memory files',
    )
    logEvent('tengu_extract_memories_skipped_direct_write', {
      message_count: newMessageCount,
    })
    return { writtenPaths: [] }
  }

  const startTime = Date.now()
  try {
    logForDebugging(
      `[extractMemories] starting — ${newMessageCount} new messages, memoryDir=${memoryDir}`,
    )

    const existingMemories = formatMemoryManifest(
      await scanMemoryFiles(memoryDir, createAbortController().signal),
    )
    const skipIndex =
      booleanPayload(payload, 'skip_index') ??
      getFeatureValue_CACHED_MAY_BE_STALE('tengu_moth_copse', false)
    const userPrompt =
      feature('TEAMMEM') && isTeamMemoryEnabled(payload)
        ? buildExtractCombinedPrompt(
            newMessageCount,
            existingMemories,
            skipIndex,
          )
        : buildExtractAutoOnlyPrompt(
            newMessageCount,
            existingMemories,
            skipIndex,
          )

    const baseCanUseTool = createAutoMemCanUseTool(memoryDir)
    const result = await runForkedAgent({
      promptMessages: [createUserMessage({ content: userPrompt })],
      cacheSafeParams: cacheSafeParams as CacheSafeParams,
      canUseTool: async (...args) => {
        assertDeliveryAuthority()
        return baseCanUseTool(...args)
      },
      querySource: 'extract_memories',
      forkLabel: 'extract_memories',
      skipTranscript: true,
      maxTurns: 5,
    })

    const writtenPaths = extractWrittenPaths(result.messages)
    const memoryPaths = writtenPaths.filter(
      p => basename(p) !== ENTRYPOINT_NAME,
    )
    const teamCount = feature('TEAMMEM')
      ? count(memoryPaths, teamMemPaths!.isTeamMemPath)
      : 0
    const turnCount = count(result.messages, m => m.type === 'assistant')
    const totalInput =
      result.totalUsage.input_tokens +
      result.totalUsage.cache_creation_input_tokens +
      result.totalUsage.cache_read_input_tokens
    const hitPct =
      totalInput > 0
        ? ((result.totalUsage.cache_read_input_tokens / totalInput) * 100).toFixed(1)
        : '0.0'

    logForDebugging(
      `[extractMemories] finished — ${writtenPaths.length} files written, cache: read=${result.totalUsage.cache_read_input_tokens} create=${result.totalUsage.cache_creation_input_tokens} input=${result.totalUsage.input_tokens} (${hitPct}% hit)`,
    )
    if (writtenPaths.length > 0) {
      logForDebugging(
        `[extractMemories] memories saved: ${writtenPaths.join(', ')}`,
      )
    } else {
      logForDebugging('[extractMemories] no memories saved this run')
    }

    logEvent('tengu_extract_memories_extraction', {
      input_tokens: result.totalUsage.input_tokens,
      output_tokens: result.totalUsage.output_tokens,
      cache_read_input_tokens: result.totalUsage.cache_read_input_tokens,
      cache_creation_input_tokens:
        result.totalUsage.cache_creation_input_tokens,
      message_count: newMessageCount,
      turn_count: turnCount,
      files_written: writtenPaths.length,
      memories_saved: memoryPaths.length,
      team_memories_saved: teamCount,
      duration_ms: Date.now() - startTime,
    })

    logForDebugging(
      `[extractMemories] writtenPaths=${writtenPaths.length} memoryPaths=${memoryPaths.length} appendSystemMessage defined=${appendSystemMessage != null}`,
    )
    if (memoryPaths.length > 0) {
      const msg = createMemorySavedMessage(memoryPaths)
      if (feature('TEAMMEM')) {
        msg.teamCount = teamCount
      }
      appendSystemMessage?.(msg)
    }

    return {
      writtenPaths,
      usage: result.totalUsage as unknown as Record<string, unknown>,
    }
  } catch (error) {
    logForDebugging(`[extractMemories] error: ${error}`)
    logEvent('tengu_extract_memories_error', {
      duration_ms: Date.now() - startTime,
    })
    throw error
  }
}
