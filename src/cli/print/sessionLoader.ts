// ANT-ONLY import markers must not be reordered
import { feature } from '../../utils/featurePolyfill.js'
import { dirname } from 'path'
import { StructuredIO } from 'src/cli/structuredIO.js'
import type { Message, NormalizedUserMessage } from 'src/types/message.js'
import type { AppState } from 'src/state/AppStateStore.js'
import { logError } from 'src/utils/log.js'
import {
  logEvent,
} from 'src/services/analytics/index.js'
import {
  loadConversationForResume,
  type TurnInterruptionState,
} from 'src/utils/conversationRecovery.js'
import { fromArray } from 'src/utils/generators.js'
import {
  gracefulShutdownSync,
} from 'src/utils/gracefulShutdown.js'
import {
  getSessionId,
  isSessionPersistenceDisabled,
  switchSession,
} from 'src/bootstrap/state.js'
import { randomUUID } from 'crypto'
import { EMPTY_USAGE } from 'src/services/api/logging.js'
import { getCwd } from 'src/utils/cwd.js'
import { asSessionId } from 'src/types/ids.js'
import {
  resetSessionFilePointer,
  restoreSessionMetadata,
  saveMode,
} from 'src/utils/sessionStorage.js'
import { parseSessionIdentifier } from 'src/utils/sessionUrl.js'
import {
  restoreSessionStateFromLog,
} from 'src/utils/sessionRestore.js'
import {
  processSessionStartHooks,
} from 'src/utils/sessionStart.js'
import type { SDKUserMessage } from 'src/entrypoints/agentSdkTypes.js'
import { jsonStringify } from 'src/utils/slowOperations.js'

// Dead code elimination: conditional imports
/* eslint-disable @typescript-eslint/no-require-imports */
const coordinatorModeModule = feature('COORDINATOR_MODE')
  ? (require('src/coordinator/coordinatorMode.js') as typeof import('src/coordinator/coordinatorMode.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

/**
 * Emits an error message in the correct format based on outputFormat.
 * When using stream-json, writes JSON to stdout; otherwise writes plain text to stderr.
 */
export function emitLoadError(
  message: string,
  outputFormat: string | undefined,
): void {
  if (outputFormat === 'stream-json') {
    const errorResult = {
      type: 'result',
      subtype: 'error_during_execution',
      duration_ms: 0,
      duration_api_ms: 0,
      is_error: true,
      num_turns: 0,
      stop_reason: null,
      session_id: getSessionId(),
      total_cost_usd: 0,
      usage: EMPTY_USAGE,
      modelUsage: {},
      permission_denials: [],
      uuid: randomUUID(),
      errors: [message],
    }
    process.stdout.write(jsonStringify(errorResult) + '\n')
  } else {
    process.stderr.write(message + '\n')
  }
}

/**
 * Removes an interrupted user message and its synthetic assistant sentinel
 * from the message array. Used during gateway-triggered restarts to clean up
 * the message history before re-enqueuing the interrupted prompt.
 *
 * @internal Exported for testing
 */
export function removeInterruptedMessage(
  messages: Message[],
  interruptedUserMessage: NormalizedUserMessage,
): void {
  const idx = messages.findIndex(m => m.uuid === interruptedUserMessage.uuid)
  if (idx !== -1) {
    // Remove the user message and the sentinel that immediately follows it.
    // splice safely handles the case where idx is the last element.
    messages.splice(idx, 2)
  }
}

export type LoadInitialMessagesResult = {
  messages: Message[]
  turnInterruptionState?: TurnInterruptionState
  agentSetting?: string
}

export async function loadInitialMessages(
  setAppState: (f: (prev: AppState) => AppState) => void,
  options: {
    continue: boolean | undefined
    resume: string | boolean | undefined
    resumeSessionAt: string | undefined
    forkSession: boolean | undefined
    outputFormat: string | undefined
    sessionStartHooksPromise?: ReturnType<typeof processSessionStartHooks>
  },
): Promise<LoadInitialMessagesResult> {
  const persistSession = !isSessionPersistenceDisabled()
  // Handle continue in print mode
  if (options.continue) {
    try {
      logEvent('tengu_continue_print', {})

      const result = await loadConversationForResume(
        undefined /* sessionId */,
        undefined /* file path */,
      )
      if (result) {
        // Match coordinator mode to the resumed session's mode
        if (feature('COORDINATOR_MODE') && coordinatorModeModule) {
          const warning = coordinatorModeModule.matchSessionMode(result.mode)
          if (warning) {
            process.stderr.write(warning + '\n')
            // Refresh agent definitions to reflect the mode switch
            const {
              getAgentDefinitionsWithOverrides,
              getActiveAgentsFromList,
            } =
              // eslint-disable-next-line @typescript-eslint/no-require-imports
              require('src/tools/AgentTool/loadAgentsDir.js') as typeof import('src/tools/AgentTool/loadAgentsDir.js')
            getAgentDefinitionsWithOverrides.cache.clear?.()
            const freshAgentDefs = await getAgentDefinitionsWithOverrides(
              getCwd(),
            )

            setAppState(prev => ({
              ...prev,
              agentDefinitions: {
                ...freshAgentDefs,
                allAgents: freshAgentDefs.allAgents,
                activeAgents: getActiveAgentsFromList(freshAgentDefs.allAgents),
              },
            }))
          }
        }

        // Reuse the resumed session's ID
        if (!options.forkSession) {
          if (result.sessionId) {
            switchSession(
              asSessionId(result.sessionId),
              result.fullPath ? dirname(result.fullPath) : null,
            )
            if (persistSession) {
              await resetSessionFilePointer()
            }
          }
        }
        restoreSessionStateFromLog(result, setAppState)

        // Restore session metadata so it's re-appended on exit via reAppendSessionMetadata
        restoreSessionMetadata(
          options.forkSession
            ? { ...result, worktreeSession: undefined }
            : result,
        )

        // Write mode entry for the resumed session
        if (feature('COORDINATOR_MODE') && coordinatorModeModule) {
          saveMode(
            coordinatorModeModule.isCoordinatorMode()
              ? 'coordinator'
              : 'normal',
          )
        }

        return {
          messages: result.messages,
          turnInterruptionState: result.turnInterruptionState,
          agentSetting: result.agentSetting,
        }
      }
    } catch (error) {
      logError(error)
      gracefulShutdownSync(1)
      return { messages: [] }
    }
  }

  // Handle resume in print mode (accepts session ID or URL)
  // URLs are [ANT-ONLY]
  if (options.resume) {
    try {
      logEvent('tengu_resume_print', {})

      // Print mode accepts a local session UUID or JSONL transcript path.
      const parsedSessionId = parseSessionIdentifier(
        typeof options.resume === 'string' ? options.resume : '',
      )
      if (!parsedSessionId) {
        let errorMessage =
          'Error: --resume requires a valid session ID when used with --print. Usage: crabcode -p --resume <session-id>'
        if (typeof options.resume === 'string') {
          errorMessage += `. Session IDs must be in UUID format (e.g., 550e8400-e29b-41d4-a716-446655440000). Provided value "${options.resume}" is not a valid UUID`
        }
        emitLoadError(errorMessage, options.outputFormat)
        gracefulShutdownSync(1)
        return { messages: [] }
      }

      // Load the conversation with the specified session ID
      const result = await loadConversationForResume(
        parsedSessionId.sessionId,
        parsedSessionId.jsonlFile || undefined,
      )

      if (!result || result.messages.length === 0) {
        emitLoadError(
          `No conversation found with session ID: ${parsedSessionId.sessionId}`,
          options.outputFormat,
        )
        gracefulShutdownSync(1)
        return { messages: [] }
      }

      // Handle resumeSessionAt feature
      if (options.resumeSessionAt) {
        const index = result.messages.findIndex(
          m => m.uuid === options.resumeSessionAt,
        )
        if (index < 0) {
          emitLoadError(
            `No message found with message.uuid of: ${options.resumeSessionAt}`,
            options.outputFormat,
          )
          gracefulShutdownSync(1)
          return { messages: [] }
        }

        result.messages = index >= 0 ? result.messages.slice(0, index + 1) : []
      }

      // Match coordinator mode to the resumed session's mode
      if (feature('COORDINATOR_MODE') && coordinatorModeModule) {
        const warning = coordinatorModeModule.matchSessionMode(result.mode)
        if (warning) {
          process.stderr.write(warning + '\n')
          // Refresh agent definitions to reflect the mode switch
          const { getAgentDefinitionsWithOverrides, getActiveAgentsFromList } =
            // eslint-disable-next-line @typescript-eslint/no-require-imports
            require('src/tools/AgentTool/loadAgentsDir.js') as typeof import('src/tools/AgentTool/loadAgentsDir.js')
          getAgentDefinitionsWithOverrides.cache.clear?.()
          const freshAgentDefs = await getAgentDefinitionsWithOverrides(
            getCwd(),
          )

          setAppState(prev => ({
            ...prev,
            agentDefinitions: {
              ...freshAgentDefs,
              allAgents: freshAgentDefs.allAgents,
              activeAgents: getActiveAgentsFromList(freshAgentDefs.allAgents),
            },
          }))
        }
      }

      // Reuse the resumed session's ID
      if (!options.forkSession && result.sessionId) {
        switchSession(
          asSessionId(result.sessionId),
          result.fullPath ? dirname(result.fullPath) : null,
        )
        if (persistSession) {
          await resetSessionFilePointer()
        }
      }
      restoreSessionStateFromLog(result, setAppState)

      // Restore session metadata so it's re-appended on exit via reAppendSessionMetadata
      restoreSessionMetadata(
        options.forkSession
          ? { ...result, worktreeSession: undefined }
          : result,
      )

      // Write mode entry for the resumed session
      if (feature('COORDINATOR_MODE') && coordinatorModeModule) {
        saveMode(
          coordinatorModeModule.isCoordinatorMode() ? 'coordinator' : 'normal',
        )
      }

      return {
        messages: result.messages,
        turnInterruptionState: result.turnInterruptionState,
        agentSetting: result.agentSetting,
      }
    } catch (error) {
      logError(error)
      const errorMessage =
        error instanceof Error
          ? `Failed to resume session: ${error.message}`
          : 'Failed to resume session with --print mode'
      emitLoadError(errorMessage, options.outputFormat)
      gracefulShutdownSync(1)
      return { messages: [] }
    }
  }

  // Join the SessionStart hooks promise kicked in main.tsx (or run fresh if
  // it wasn't kicked — e.g. --continue with no prior session falls through
  // here with sessionStartHooksPromise undefined because main.tsx guards on continue)
  return {
    messages: await (options.sessionStartHooksPromise ??
      processSessionStartHooks('startup')),
  }
}

export function getStructuredIO(
  inputPrompt: string | AsyncIterable<string>,
  replayUserMessages?: boolean,
): StructuredIO {
  let inputStream: AsyncIterable<string>
  if (typeof inputPrompt === 'string') {
    if (inputPrompt.trim() !== '') {
      // Normalize to a streaming input.
      inputStream = fromArray([
        jsonStringify({
          type: 'user',
          session_id: '',
          message: {
            role: 'user',
            content: inputPrompt,
          },
          parent_tool_use_id: null,
        } satisfies SDKUserMessage),
      ])
    } else {
      // Empty string - create empty stream
      inputStream = fromArray([])
    }
  } else {
    inputStream = inputPrompt
  }

  return new StructuredIO(inputStream, replayUserMessages)
}
