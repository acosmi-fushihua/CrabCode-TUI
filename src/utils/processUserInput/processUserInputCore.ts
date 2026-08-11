import type {
  Base64ImageSource,
  ContentBlockParam,
  ImageBlockParam,
} from '../../types/api-types.js'
import { feature } from '../featurePolyfill.js'

import { randomUUID } from 'crypto'
import type { QuerySource } from 'src/constants/querySource.js'
import { logEvent } from 'src/services/analytics/index.js'
import { getContentText } from 'src/utils/messages.js'
import {
  findCommand,
  getCommandName,
  type LocalJSXCommandContext,
} from '../../types/command.js'
import type { CanUseToolFn } from '../../types/canUseTool.js'
import type { IDESelection } from '../../types/ideSelection.js'
import type { SetToolJSXFn, ToolUseContext } from '../../Tool.js'
import type {
  AssistantMessage,
  AttachmentMessage,
  Message,
  ProgressMessage,
  SystemMessage,
  UserMessage,
} from '../../types/message.js'
import type { PermissionMode } from '../../types/permissions.js'
import {
  isValidImagePaste,
  type PromptInputMode,
} from '../../types/textInputTypes.js'
import {
  type AgentMentionAttachment,
  createAttachmentMessage,
  getAttachmentMessages,
} from '../attachments.js'
import {
  automationSurfaceForSkillName,
  decideAutomationSurfaceRoute,
  type AutomationSurfaceCapabilityFailure,
  type AutomationSurfaceIntent,
} from '../browserAutomation/intentRoute.js'
import type { PastedContent } from '../config.js'
import type { EffortValue } from '../effort.js'
import { toArray } from '../generators.js'
import {
  executeUserPromptSubmitHooks,
  getUserPromptSubmitHookBlockingMessage,
} from '../hooks.js'
import {
  createImageMetadataText,
  maybeResizeAndDownsampleImageBlock,
} from '../imageResizer.js'
import { storeImages } from '../imageStore.js'
import {
  createCommandInputMessage,
  createSystemMessage,
  createUserMessage,
} from '../messages.js'
import { queryCheckpoint } from '../queryProfiler.js'
import { parseSlashCommand } from '../slashCommandParsing.js'
import { processTextPrompt } from './processTextPrompt.js'
export type ProcessUserInputContext = ToolUseContext & LocalJSXCommandContext

export type LocalCommandTerminalOutcome = {
  /**
   * `cancelled` is distinct from an execution error even though the current
   * SDK result schema represents both with `error_during_execution`.
   */
  status: 'error' | 'cancelled'
  /** Terminal diagnostic text, without any renderer XML wrapper. */
  message: string
}

export type ProcessUserInputRuntime = {
  loadSlashCommandProcessor: () => Promise<
    Pick<
      typeof import('./processSlashCommandHeadless.js'),
      'processSlashCommand'
    >
  >
  loadBashCommandProcessor: () => Promise<
    Pick<
      typeof import('./processBashCommandHeadless.js'),
      'processBashCommand'
    >
  >
}

export type ProcessUserInputBaseResult = {
  messages: (
    | UserMessage
    | AssistantMessage
    | AttachmentMessage
    | SystemMessage
    | ProgressMessage
  )[]
  shouldQuery: boolean
  allowedTools?: string[]
  /** Hard-routed interactive automation surface for turn-scoped tool/schema isolation. */
  automationSurface?: AutomationSurfaceIntent['backend']
  /**
   * Typed pre-model failure retained separately from the display string so
   * QueryEngine can project a visible assistant frame through the SDK/worker
   * channel even though this turn intentionally performs no model request.
   */
  automationFailure?: AutomationSurfaceCapabilityFailure & { message: string }
  model?: string
  effort?: EffortValue
  // Output text for non-interactive mode (e.g., forked commands)
  // When set, this is used as the result in -p mode instead of empty string
  resultText?: string
  /**
   * Terminal failure from a slash command that intentionally skipped the
   * model query. QueryEngine must not report these turns as successful merely
   * because `shouldQuery` is false.
   */
  localCommandOutcome?: LocalCommandTerminalOutcome
  // When set, prefills or submits the next input after command completes
  // Used by /discover to chain into the selected feature's command
  nextInput?: string
  submitNextInput?: boolean
}

export async function processUserInputCore({
  input,
  mode,
  setToolJSX,
  context,
  pastedContents,
  ideSelection,
  messages,
  setUserInputOnProcessing,
  uuid,
  isAlreadyProcessing,
  querySource,
  canUseTool,
  skipSlashCommands,
  isMeta,
  skipAttachments,
  localAutomationRoutingAllowed,
  onLocalInputEvent,
}: {
  input: string | Array<ContentBlockParam>
  mode: PromptInputMode
  setToolJSX: SetToolJSXFn
  context: ProcessUserInputContext
  pastedContents?: Record<number, PastedContent>
  ideSelection?: IDESelection
  messages?: Message[]
  setUserInputOnProcessing?: (prompt?: string) => void
  uuid?: string
  isAlreadyProcessing?: boolean
  querySource?: QuerySource
  canUseTool?: CanUseToolFn
  /** When true, input starting with `/` is treated as plain text. */
  skipSlashCommands?: boolean
  /**
   * When true, the resulting UserMessage gets `isMeta: true` (user-hidden,
   * model-visible). Propagated from `QueuedCommand.isMeta` for queued
   * system-generated prompts.
   */
  isMeta?: boolean
  skipAttachments?: boolean
  /**
   * Whether this input is a trusted, foreground human turn that may select an
   * interactive automation surface. Direct REPL callers omit it and retain the
   * historical local-interactive default; QueryEngine/headless callers must
   * pass their provenance decision explicitly so background/silent turns
   * cannot activate browser, Chrome, or Computer* skills from prompt text.
   */
  localAutomationRoutingAllowed?: boolean
  /** Host-owned capability tier for real Chrome and native desktop control. */
  /**
   * Process-local renderer observer for transient local-input events. It is
   * deliberately absent from ToolUseContext and every public transport.
   */
  onLocalInputEvent?: (event: Message) => void
}, runtime: ProcessUserInputRuntime): Promise<ProcessUserInputBaseResult> {
  const inputString = typeof input === 'string' ? input : null
  // Immediately show the user input prompt while we are still processing the input.
  // Skip for isMeta (system-generated prompts like scheduled tasks) — those
  // should run invisibly.
  if (mode === 'prompt' && inputString !== null && !isMeta) {
    setUserInputOnProcessing?.(inputString)
  }

  queryCheckpoint('query_process_user_input_base_start')

  const appState = context.getAppState()

  const result = await processUserInputBase(
    input,
    mode,
    setToolJSX,
    context,
    runtime,
    pastedContents,
    ideSelection,
    messages,
    uuid,
    isAlreadyProcessing,
    querySource,
    canUseTool,
    appState.toolPermissionContext.mode,
    skipSlashCommands,
    isMeta,
    skipAttachments,
    localAutomationRoutingAllowed,
    onLocalInputEvent,
  )
  queryCheckpoint('query_process_user_input_base_end')

  if (!result.shouldQuery) {
    return result
  }

  // Execute UserPromptSubmit hooks and handle blocking
  queryCheckpoint('query_hooks_start')
  const inputMessage = getContentText(input) || ''

  const userPromptSubmitResults = context.suppressUntrustedHooks
    ? []
    : executeUserPromptSubmitHooks(
        inputMessage,
        appState.toolPermissionContext.mode,
        context,
        context.requestPrompt,
      )
  for await (const hookResult of userPromptSubmitResults) {
    // We only care about the result
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    if ((hookResult.message as any)?.type === 'progress') {
      continue
    }

    // Return only a system-level error message, erasing the original user input
    if (hookResult.blockingError) {
      const blockingMessage = getUserPromptSubmitHookBlockingMessage(
        hookResult.blockingError,
      )
      return {
        messages: [
          // Blocking message rendered as system warning.
          createSystemMessage(
            `${blockingMessage}\n\nOriginal prompt: ${input}`,
            'warning',
          ),
        ],
        shouldQuery: false,
        allowedTools: result.allowedTools,
      }
    }

    // If preventContinuation is set, stop processing but keep the original
    // prompt in context.
    if (hookResult.preventContinuation) {
      const message = hookResult.stopReason
        ? `Operation stopped by hook: ${hookResult.stopReason}`
        : 'Operation stopped by hook'
      result.messages.push(
        createUserMessage({
          content: message,
        }),
      )
      result.shouldQuery = false
      return result
    }

    // Collect additional contexts
    if (
      hookResult.additionalContexts &&
      hookResult.additionalContexts.length > 0
    ) {
      result.messages.push(
        createAttachmentMessage({
          type: 'hook_additional_context',
          content: hookResult.additionalContexts.map(applyTruncation),
          hookName: 'UserPromptSubmit',
          toolUseID: `hook-${randomUUID()}`,
          hookEvent: 'UserPromptSubmit',
        }),
      )
    }

    // Handle hook result message attachment.
    if (hookResult.message) {
      switch (hookResult.message.attachment.type) {
        case 'hook_success':
          if (!hookResult.message.attachment.content) {
            // Skip if there is no content
            break
          }
          result.messages.push({
            ...hookResult.message,
            attachment: {
              ...hookResult.message.attachment,
              content: applyTruncation(hookResult.message.attachment.content),
            },
          })
          break
        default:
          result.messages.push(hookResult.message)
          break
      }
    }
  }
  queryCheckpoint('query_hooks_end')

  // Happy path: onQuery will clear userInputOnProcessing via startTransition
  // so it resolves in the same frame as deferredMessages (no flicker gap).
  // Error paths are handled by handlePromptSubmit's finally block.
  return result
}

const MAX_HOOK_OUTPUT_LENGTH = 10000

function applyTruncation(content: string): string {
  if (content.length > MAX_HOOK_OUTPUT_LENGTH) {
    return `${content.substring(0, MAX_HOOK_OUTPUT_LENGTH)}… [output truncated - exceeded ${MAX_HOOK_OUTPUT_LENGTH} characters]`
  }
  return content
}

async function processUserInputBase(
  input: string | Array<ContentBlockParam>,
  mode: PromptInputMode,
  setToolJSX: SetToolJSXFn,
  context: ProcessUserInputContext,
  runtime: ProcessUserInputRuntime,
  pastedContents?: Record<number, PastedContent>,
  ideSelection?: IDESelection,
  messages?: Message[],
  uuid?: string,
  isAlreadyProcessing?: boolean,
  querySource?: QuerySource,
  canUseTool?: CanUseToolFn,
  permissionMode?: PermissionMode,
  skipSlashCommands?: boolean,
  isMeta?: boolean,
  skipAttachments?: boolean,
  localAutomationRoutingAllowed?: boolean,
  onLocalInputEvent?: (event: Message) => void,
): Promise<ProcessUserInputBaseResult> {
  let inputString: string | null = null
  let precedingInputBlocks: ContentBlockParam[] = []

  // Collect image metadata texts for isMeta message
  const imageMetadataTexts: string[] = []

  // Normalized view of `input` with image blocks resized. For string input
  // this is just `input`; for array input it's the processed blocks. We pass
  // this (not raw `input`) to processTextPrompt so resized/normalized image
  // blocks actually reach the API — otherwise the resize work above is
  // discarded for the regular prompt path. Also normalizes bridge inputs
  // where iOS may send `mediaType` instead of `media_type` (mobile-apps#5825).
  let normalizedInput: string | ContentBlockParam[] = input

  if (typeof input === 'string') {
    inputString = input
  } else if (input.length > 0) {
    queryCheckpoint('query_image_processing_start')
    const processedBlocks: ContentBlockParam[] = []
    for (const block of input) {
      if (block.type === 'image') {
        const resized = await maybeResizeAndDownsampleImageBlock(block)
        // Collect image metadata for isMeta message
        if (resized.dimensions) {
          const metadataText = createImageMetadataText(resized.dimensions)
          if (metadataText) {
            imageMetadataTexts.push(metadataText)
          }
        }
        processedBlocks.push(resized.block)
      } else {
        processedBlocks.push(block)
      }
    }
    normalizedInput = processedBlocks
    queryCheckpoint('query_image_processing_end')
    // Extract the input string from the last content block if it is text,
    // and keep track of the preceding content blocks
    const lastBlock = processedBlocks[processedBlocks.length - 1]
    if (lastBlock?.type === 'text') {
      inputString = lastBlock.text
      precedingInputBlocks = processedBlocks.slice(0, -1)
    } else {
      precedingInputBlocks = processedBlocks
    }
  }

  if (inputString === null && mode !== 'prompt') {
    throw new Error(`Mode: ${mode} requires a string input.`)
  }

  // Extract and convert image content to content blocks early
  // Keep track of IDs in order for message storage
  const imageContents = pastedContents
    ? Object.values(pastedContents).filter(isValidImagePaste)
    : []
  const imagePasteIds = imageContents.map(img => img.id)

  // Store images to disk so CrabCode can reference the path in context
  // (for manipulation with CLI tools, uploading to PRs, etc.)
  const storedImagePaths = pastedContents
    ? await storeImages(pastedContents)
    : new Map<number, string>()

  // Resize pasted images to ensure they fit within API limits (parallel processing)
  queryCheckpoint('query_pasted_image_processing_start')
  const imageProcessingResults = await Promise.all(
    imageContents.map(async pastedImage => {
      const imageBlock: ImageBlockParam = {
        type: 'image',
        source: {
          type: 'base64',
          media_type: (pastedImage.mediaType ||
            'image/png') as Base64ImageSource['media_type'],
          data: pastedImage.content,
        },
      }
      logEvent('tengu_pasted_image_resize_attempt', {
        original_size_bytes: pastedImage.content.length,
      })
      const resized = await maybeResizeAndDownsampleImageBlock(imageBlock)
      return {
        resized,
        originalDimensions: pastedImage.dimensions,
        sourcePath:
          pastedImage.sourcePath ?? storedImagePaths.get(pastedImage.id),
      }
    }),
  )
  // Collect results preserving order
  const imageContentBlocks: ContentBlockParam[] = []
  for (const {
    resized,
    originalDimensions,
    sourcePath,
  } of imageProcessingResults) {
    // Collect image metadata for isMeta message (prefer resized dimensions)
    if (resized.dimensions) {
      const metadataText = createImageMetadataText(
        resized.dimensions,
        sourcePath,
      )
      if (metadataText) {
        imageMetadataTexts.push(metadataText)
      }
    } else if (originalDimensions) {
      // Fall back to original dimensions if resize didn't provide them
      const metadataText = createImageMetadataText(
        originalDimensions,
        sourcePath,
      )
      if (metadataText) {
        imageMetadataTexts.push(metadataText)
      }
    } else if (sourcePath) {
      // If we have a source path but no dimensions, still add source info
      imageMetadataTexts.push(`[Image source: ${sourcePath}]`)
    }
    imageContentBlocks.push(resized.block)
  }
  queryCheckpoint('query_pasted_image_processing_end')

  // A concrete, foreground terminal action may enter the isolated-browser
  // skill before the first model round. Background inputs cannot activate it.
  if (
    mode === 'prompt' &&
    inputString !== null &&
    !isMeta &&
    !skipSlashCommands &&
    !inputString.startsWith('/')
  ) {
    const automationRoute = decideAutomationSurfaceRoute({
      input: inputString,
      availableCommandNames: context.options.commands.map(getCommandName),
      availableToolNames: context.options.tools.map(tool => tool.name),
      localCapabilityRoutingAllowed: localAutomationRoutingAllowed !== false,
    })

    // A high-confidence UI route fails closed when its isolated browser is
    // unavailable instead of falling back into the unrestricted tool pool.
    if (automationRoute.type === 'failure') {
      const failureText = `Automation surface unavailable [${automationRoute.failure.kind}] (${automationRoute.failure.backend}): ${automationRoute.failure.guidance}`
      return {
        messages: [createSystemMessage(failureText, 'warning')],
        shouldQuery: false,
        resultText: failureText,
        automationFailure: {
          ...automationRoute.failure,
          message: failureText,
        },
      }
    }

    if (automationRoute.type === 'skill') {
      const { processSlashCommand } =
        await runtime.loadSlashCommandProcessor()
      const slashResult = await processSlashCommand(
        automationRoute.slashInput,
        precedingInputBlocks,
        imageContentBlocks,
        [],
        context,
        setToolJSX,
        uuid,
        isAlreadyProcessing,
        canUseTool,
      )
      return {
        ...addImageMetadataMessage(slashResult, imageMetadataTexts),
        automationSurface: automationRoute.backend,
      }
    }
  }

  // For slash commands, attachments will be extracted within getMessagesForSlashCommand
  const shouldExtractAttachments =
    !skipAttachments &&
    inputString !== null &&
    (mode !== 'prompt' || skipSlashCommands || !inputString.startsWith('/'))

  queryCheckpoint('query_attachment_loading_start')
  const attachmentMessages = shouldExtractAttachments
    ? await toArray(
        getAttachmentMessages(
          inputString,
          context,
          ideSelection ?? null,
          [], // queuedCommands - handled by query.ts for mid-turn attachments
          messages,
          querySource,
        ),
      )
    : []
  queryCheckpoint('query_attachment_loading_end')

  // Bash commands
  if (inputString !== null && mode === 'bash') {
    const { processBashCommand } =
      await runtime.loadBashCommandProcessor()
    return addImageMetadataMessage(
      await processBashCommand(
        inputString,
        precedingInputBlocks,
        attachmentMessages,
        context,
        setToolJSX,
        onLocalInputEvent,
      ),
      imageMetadataTexts,
    )
  }

  // Slash commands
  if (
    inputString !== null &&
    !skipSlashCommands &&
    inputString.startsWith('/')
  ) {
    const parsed = parseSlashCommand(inputString)
    const command = parsed
      ? findCommand(parsed.commandName, context.options.commands)
      : undefined
    const parsedAutomationSurface = parsed
      ? automationSurfaceForSkillName(parsed.commandName)
      : undefined
    const explicitAutomationSurface = command
      ? automationSurfaceForSkillName(command.name)
      : parsedAutomationSurface
    const explicitCommandName = command
      ? getCommandName(command)
      : parsed?.commandName
    if (
      explicitAutomationSurface !== undefined &&
      localAutomationRoutingAllowed === false
    ) {
      const msg = `/${explicitCommandName} is available only from a foreground interactive terminal turn; background, scheduled, and headless input cannot activate interactive automation.`
      return {
        messages: [
          createUserMessage({ content: inputString, uuid }),
          createCommandInputMessage(
            `<local-command-stderr>${msg}</local-command-stderr>`,
          ),
        ],
        shouldQuery: false,
        resultText: msg,
        localCommandOutcome: { status: 'error', message: msg },
      }
    }
    const { processSlashCommand } =
      await runtime.loadSlashCommandProcessor()
    const slashResult = await processSlashCommand(
      inputString,
      precedingInputBlocks,
      imageContentBlocks,
      attachmentMessages,
      context,
      setToolJSX,
      uuid,
      isAlreadyProcessing,
      canUseTool,
    )
    const result = addImageMetadataMessage(slashResult, imageMetadataTexts)
    return explicitAutomationSurface === undefined
      ? result
      : { ...result, automationSurface: explicitAutomationSurface }
  }

  // Log agent mention queries for analysis
  if (inputString !== null && mode === 'prompt') {
    const trimmedInput = inputString.trim()

    const agentMention = attachmentMessages.find(
      (m): m is AttachmentMessage<AgentMentionAttachment> =>
        m.attachment.type === 'agent_mention',
    )

    if (agentMention) {
      const agentMentionString = `@agent-${agentMention.attachment.agentType}`
      const isSubagentOnly = trimmedInput === agentMentionString
      const isPrefix =
        trimmedInput.startsWith(agentMentionString) && !isSubagentOnly

      // Log whenever users use @agent-<name> syntax
      logEvent('tengu_subagent_at_mention', {
        is_subagent_only: isSubagentOnly,
        is_prefix: isPrefix,
      })
    }
  }

  // Regular user prompt
  return addImageMetadataMessage(
    processTextPrompt(
      normalizedInput,
      imageContentBlocks,
      imagePasteIds,
      attachmentMessages,
      uuid,
      permissionMode,
      isMeta,
    ),
    imageMetadataTexts,
  )
}

// Adds image metadata texts as isMeta message to result
function addImageMetadataMessage(
  result: ProcessUserInputBaseResult,
  imageMetadataTexts: string[],
): ProcessUserInputBaseResult {
  if (imageMetadataTexts.length > 0) {
    result.messages.push(
      createUserMessage({
        content: imageMetadataTexts.map(text => ({ type: 'text', text })),
        isMeta: true,
      }),
    )
  }
  return result
}
