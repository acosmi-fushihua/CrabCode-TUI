import type { ContentBlockParam } from '../../types/api-types.js'
import type {
  AttachmentMessage,
  Message,
  SystemMessage,
  UserMessage,
} from '../../types/message.js'
import type { ShellProgress } from '../../types/tools.js'
import {
  localExecBridge,
  type SpawnProgressCallback,
} from '../../runtime/localProcess.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../../services/analytics/index.js'
import {
  isImageOutput,
  resizeShellImageOutput,
  resetCwdIfOutsideProject,
  stdErrAppendShellResetMessage,
  stripEmptyLines,
} from '../../tools/BashTool/utils.js'
import { extractCrabCodeHints } from '../crabCodeHints.js'
import { detectCodeIndexingFromCommand } from '../codeIndexing.js'
import { errorMessage } from '../errors.js'
import {
  createSyntheticUserCaveatMessage,
  createProgressMessage,
  createUserInterruptionMessage,
  createUserMessage,
  prepareUserContent,
} from '../messages.js'
import { maybeRecordPluginHint } from '../plugins/hintRecommendation.js'
import { resolveDefaultShell } from '../shell/resolveDefaultShell.js'
import { isPowerShellToolEnabled } from '../shell/shellToolUtils.js'
import {
  buildLargeToolResultMessage,
  generatePreview,
  PREVIEW_SIZE_BYTES,
} from '../toolResultStorage.js'
import { escapeXml } from '../xml.js'
import type { ProcessUserInputContext } from './processUserInputCore.js'

/**
 * Renderer-free implementation of explicit `!`/bash input.
 *
 * Execution, telemetry, cwd repair, persisted-output handling, image
 * resizing, and transcript projection match the interactive implementation;
 * only the transient legacy progress component is omitted because progress is
 * rendered by the native TUI from StructuredIO events.
 */
export async function processBashCommand(
  inputString: string,
  precedingInputBlocks: ContentBlockParam[],
  attachmentMessages: AttachmentMessage[],
  context: ProcessUserInputContext,
  _setToolJSX: import('../../Tool.js').SetToolJSXFn,
  onLocalInputEvent?: (event: Message) => void,
): Promise<{
  messages: (UserMessage | AttachmentMessage | SystemMessage)[]
  shouldQuery: boolean
}> {
  const usePowerShell =
    isPowerShellToolEnabled() &&
    resolveDefaultShell() === 'powershell'
  logEvent('tengu_input_bash', { powershell: usePowerShell })

  const userMessage = createUserMessage({
    content: prepareUserContent({
      inputString: `<bash-input>${inputString}</bash-input>`,
      precedingInputBlocks,
    }),
  })

  try {
    const progressType: ShellProgress['type'] = usePowerShell
      ? 'powershell_progress'
      : 'bash_progress'
    const startTime = Date.now()
    let progressCounter = 0
    const shellOnProgress: SpawnProgressCallback = (
      lastLines,
      allLines,
      totalLines,
      totalBytes,
      isIncomplete,
    ) => {
      if (!onLocalInputEvent) return
      onLocalInputEvent(
        createProgressMessage({
          toolUseID: `${usePowerShell ? 'ps' : 'bash'}-progress-${progressCounter++}`,
          // A local `!` command has no assistant tool_use. Its existing user
          // message UUID is the stable turn-local identity available on both
          // sides of the process-private renderer boundary.
          parentToolUseID: userMessage.uuid,
          data: {
            type: progressType,
            output: lastLines,
            fullOutput: allLines,
            elapsedTimeSeconds: (Date.now() - startTime) / 1000,
            totalLines,
            totalBytes: isIncomplete ? totalBytes : undefined,
          },
        }),
      )
    }

    const result = await localExecBridge.spawnManaged(
      {
        command: inputString,
        shell: usePowerShell ? 'powershell' : 'bash',
        dangerouslyDisableSandbox: true,
      },
      {
        onProgress: shellOnProgress,
        abortSignal: context.abortController.signal,
      },
    )

    let stderr = result.stderr
    if (
      resetCwdIfOutsideProject(
        context.getAppState().toolPermissionContext,
      )
    ) {
      stderr = stdErrAppendShellResetMessage(stderr)
    }

    const commandType = inputString.split(' ')[0]
    logEvent('tengu_bash_tool_command_executed', {
      command_type:
        commandType as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      stdout_length: result.stdout.length,
      stderr_length: 0,
      exit_code: result.exitCode,
      interrupted: result.interrupted ?? false,
    })
    const codeIndexingTool = detectCodeIndexingFromCommand(inputString)
    if (codeIndexingTool) {
      logEvent('tengu_code_indexing_tool_used', {
        tool: codeIndexingTool as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        source:
          'cli' as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        success: result.exitCode === 0,
      })
    }

    if (result.interrupted) {
      return {
        messages: [
          createSyntheticUserCaveatMessage(),
          userMessage,
          createUserInterruptionMessage({ toolUse: false }),
          ...attachmentMessages,
        ],
        shouldQuery: false,
      }
    }

    let processedStdout = stripEmptyLines(result.stdout)
    const extracted = extractCrabCodeHints(processedStdout, inputString)
    processedStdout = extracted.stripped
    for (const hint of extracted.hints) {
      maybeRecordPluginHint(hint)
    }

    if (isImageOutput(processedStdout)) {
      const resized = await resizeShellImageOutput(
        processedStdout,
        result.persistedOutputPath,
        result.persistedOutputSize,
      )
      if (resized) processedStdout = resized
    }

    let stdout: string
    if (result.persistedOutputPath) {
      const preview = generatePreview(
        processedStdout,
        PREVIEW_SIZE_BYTES,
      )
      stdout = buildLargeToolResultMessage({
        filepath: result.persistedOutputPath,
        originalSize: result.persistedOutputSize ?? 0,
        isJson: false,
        preview: preview.preview,
        hasMore: preview.hasMore,
      })
    } else {
      stdout = escapeXml(processedStdout)
    }

    return {
      messages: [
        createSyntheticUserCaveatMessage(),
        userMessage,
        ...attachmentMessages,
        createUserMessage({
          content: `<bash-stdout>${stdout}</bash-stdout><bash-stderr>${escapeXml(stderr)}</bash-stderr>`,
        }),
      ],
      shouldQuery: false,
    }
  } catch (error) {
    return {
      messages: [
        createSyntheticUserCaveatMessage(),
        userMessage,
        ...attachmentMessages,
        createUserMessage({
          content: `<bash-stderr>Command failed: ${escapeXml(
            errorMessage(error),
          )}</bash-stderr>`,
        }),
      ],
      shouldQuery: false,
    }
  }
}
