import { getOriginalCwd, getSurface } from '../../../bootstrap/state.js'
import type { SetAppState } from '../../../Task.js'
import type { Message } from '../../../types/message.js'
import { createChildAbortController } from '../../../utils/abortController.js'
import { logForDebugging } from '../../../utils/debug.js'
import {
  type CacheSafeParams,
  runForkedAgent,
} from '../../../utils/forkedAgent.js'
import {
  createUserMessage,
  createMemorySavedMessage,
  createSystemMessage,
} from '../../../utils/messages.js'
import { getProjectDir } from '../../../utils/sessionStorage.js'
import {
  registerDreamTask,
  addDreamTurn,
  completeDreamTask,
  failDreamTask,
  isDreamTask,
} from '../../../tasks/DreamTask/DreamTask.js'
import { FILE_EDIT_TOOL_NAME } from '../../../tools/FileEditTool/constants.js'
import { FILE_WRITE_TOOL_NAME } from '../../../tools/FileWriteTool/prompt.js'
import { logEvent } from '../../analytics/index.js'
import { buildConsolidationPrompt } from '../../autoDream/consolidationPrompt.js'
import { DREAM_SUBAGENT_AGENT_TYPE } from '../../dreamSubagent/agentDefinition.js'
import {
  createAutoMemCanUseTool,
  extractWrittenPaths,
} from '../extract/runner.js'
import type {
  MemoryTriggerResult,
  MemoryTriggerRunnerInput,
} from '../turnEnd.js'

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

function stringArrayPayload(
  payload: Record<string, unknown>,
  key: string,
): string[] {
  const value = payload[key]
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === 'string')
    : []
}

function makeDreamProgressWatcher(
  taskId: string,
  setAppState: SetAppState,
): (msg: Message) => void {
  return msg => {
    if (msg.type !== 'assistant') return
    let text = ''
    let toolUseCount = 0
    const touchedPaths: string[] = []
    for (const block of msg.message.content) {
      if (block.type === 'text') {
        text += block.text
      } else if (block.type === 'tool_use') {
        toolUseCount++
        if (
          block.name === FILE_EDIT_TOOL_NAME ||
          block.name === FILE_WRITE_TOOL_NAME
        ) {
          const input = block.input as { file_path?: unknown }
          if (typeof input.file_path === 'string') {
            touchedPaths.push(input.file_path)
          }
        }
      }
    }
    addDreamTurn(
      taskId,
      { text: text.trim(), toolUseCount },
      touchedPaths,
      setAppState,
    )
  }
}

export async function runDreamMemoryTrigger(
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
  const memoryRoot = stringPayload(payload, 'memory_dir')
  if (!memoryRoot) {
    throw new Error('dream runner missing memory_dir')
  }

  const setAppState =
    context.toolUseContext.setAppStateForTasks ??
    context.toolUseContext.setAppState
  const getAppState = context.toolUseContext.getAppState
  if (!setAppState || !getAppState) {
    throw new Error('dream runner missing app state hooks')
  }

  const sessionIds = stringArrayPayload(payload, 'session_ids')
  const sessionsReviewing =
    numberPayload(payload, 'sessions_since_last_consolidation') ??
    sessionIds.length
  const priorMtime = numberPayload(payload, 'prior_mtime_ms') ?? 0
  const hoursSince = numberPayload(
    payload,
    'hours_since_last_consolidation',
  )

  logForDebugging(
    `[autoDream] firing — ${hoursSince?.toFixed(1) ?? '?'}h since last, ${sessionsReviewing} sessions to review`,
  )
  logEvent('tengu_auto_dream_fired', {
    hours_since: Math.round(hoursSince ?? 0),
    sessions_since: sessionsReviewing,
  })

  // The task panel needs its own controller so a user can cancel only this
  // dream task, but durable recovery is also owned by the current Memory
  // leader. Link the task controller to the authoritative runner context so a
  // leader stop/shutdown abort reaches the in-flight model call immediately.
  const abortController = createChildAbortController(
    cacheSafeParams.toolUseContext.abortController,
  )
  const taskId = registerDreamTask(setAppState, {
    sessionsReviewing,
    priorMtime,
    abortController,
  })

  try {
    const transcriptDir =
      stringPayload(payload, 'transcript_dir') ?? getProjectDir(getOriginalCwd())
    const extra = `\n\n**Tool constraints for this run:** Bash is restricted to read-only commands (\`ls\`, \`find\`, \`grep\`, \`cat\`, \`stat\`, \`wc\`, \`head\`, \`tail\`, and similar). Anything that writes, redirects to a file, or modifies state will be denied. Plan your exploration with this in mind — no need to probe.\n\nSessions since last consolidation (${sessionsReviewing}):\n${sessionIds.map(id => `- ${id}`).join('\n')}`
    const prompt = buildConsolidationPrompt(memoryRoot, transcriptDir, extra)

    const baseCanUseTool = createAutoMemCanUseTool(memoryRoot)
    const result = await runForkedAgent({
      promptMessages: [createUserMessage({ content: prompt })],
      cacheSafeParams: cacheSafeParams as CacheSafeParams,
      canUseTool: async (...args) => {
        assertDeliveryAuthority()
        return baseCanUseTool(...args)
      },
      querySource: 'auto_dream',
      forkLabel: 'auto_dream',
      skipTranscript: true,
      // W-MEMORY-DREAM-REBUILD v7 P6.1: tag the fork with the dream-subagent
      // identity so the task panel / telemetry / agent-state surfaces can
      // distinguish dream forks from generic general-purpose subagents.
      overrides: {
        abortController,
        agentType: DREAM_SUBAGENT_AGENT_TYPE,
      },
      onMessage: makeDreamProgressWatcher(taskId, setAppState),
    })

    completeDreamTask(taskId, setAppState)
    const dreamState = getAppState().tasks?.[taskId]
    const watchedPaths = isDreamTask(dreamState) ? dreamState.filesTouched : []
    const writtenPaths = [
      ...new Set([...watchedPaths, ...extractWrittenPaths(result.messages)]),
    ]
    if (appendSystemMessage && watchedPaths.length > 0) {
      appendSystemMessage(createMemorySavedMessage(watchedPaths))
      // The native TUI exposes generated memory artifacts as ordinary files.
      if (getSurface() === 'tui') {
        appendSystemMessage(
          createSystemMessage(
            '做梦/想象产物已写入上述路径；可直接用终端编辑器打开、检索和审阅。',
            'info',
          ),
        )
      }
    }
    logForDebugging(
      `[autoDream] completed — cache: read=${result.totalUsage.cache_read_input_tokens} created=${result.totalUsage.cache_creation_input_tokens}`,
    )
    logEvent('tengu_auto_dream_completed', {
      cache_read: result.totalUsage.cache_read_input_tokens,
      cache_created: result.totalUsage.cache_creation_input_tokens,
      output: result.totalUsage.output_tokens,
      sessions_reviewed: sessionsReviewing,
    })
    return {
      writtenPaths,
      usage: result.totalUsage as unknown as Record<string, unknown>,
    }
  } catch (error) {
    if (abortController.signal.aborted) {
      logForDebugging('[autoDream] aborted by user')
      throw error
    }
    logForDebugging(`[autoDream] fork failed: ${(error as Error).message}`)
    logEvent('tengu_auto_dream_failed', {})
    failDreamTask(taskId, setAppState)
    throw error
  }
}
