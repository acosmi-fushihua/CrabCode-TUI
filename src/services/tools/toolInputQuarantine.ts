import type { ToolUseBlock } from '../../types/api-types.js'
import { FILE_EDIT_TOOL_NAME } from '../../tools/FileEditTool/constants.js'
import {
  FILE_WRITE_TOOL_NAME,
  LARGE_ARTIFACT_RECOVERY_INSTRUCTION,
} from '../../tools/FileWriteTool/prompt.js'
import { NOTEBOOK_EDIT_TOOL_NAME } from '../../tools/NotebookEditTool/constants.js'
import { hashContent } from '../../utils/hash.js'

const MALFORMED_TOOL_INPUT_MARKER = '__crabcodeToolInputError' as const

/**
 * A serializable, payload-free marker for streamed tool JSON that was non-empty
 * but could not be parsed. Keeping this distinct from `{}` is load-bearing:
 * `{}` is a legitimate zero-argument-tool convention and ordinary tool input,
 * while this value must never reach a tool implementation.
 */
export type MalformedToolInput = {
  [MALFORMED_TOOL_INPUT_MARKER]: {
    kind: 'malformed_json'
    inputLength: number
    /** Correlation-only digest. The raw input is deliberately not retained. */
    inputHash: string
  }
}

export function createMalformedToolInput(rawInput: string): MalformedToolInput {
  return {
    [MALFORMED_TOOL_INPUT_MARKER]: {
      kind: 'malformed_json',
      inputLength: rawInput.length,
      inputHash: hashContent(rawInput),
    },
  }
}

export function isMalformedToolInput(input: unknown): input is MalformedToolInput {
  if (!isRecord(input)) return false
  const marker = input[MALFORMED_TOOL_INPUT_MARKER]
  return (
    isRecord(marker) &&
    marker.kind === 'malformed_json' &&
    typeof marker.inputLength === 'number' &&
    Number.isSafeInteger(marker.inputLength) &&
    marker.inputLength > 0 &&
    typeof marker.inputHash === 'string' &&
    marker.inputHash.length > 0
  )
}

export type ToolInputQuarantineReason =
  | 'malformed_json'
  | 'empty_file_mutation'
  | 'file_mutation_pending_terminal'

// MultiEdit is retained as a legacy projection/tool name even though it has no
// current first-party definition. Treating it as a mutation here is harmless
// and keeps resumed transcripts fail-closed.
const FILE_MUTATION_TOOL_NAMES = new Set<string>([
  FILE_WRITE_TOOL_NAME,
  FILE_EDIT_TOOL_NAME,
  NOTEBOOK_EDIT_TOOL_NAME,
  'MultiEdit',
])

export function isFileMutationToolName(name: string): boolean {
  return FILE_MUTATION_TOOL_NAMES.has(name)
}

/**
 * Return a reason only for inputs that cannot safely enter the streaming tool
 * executor before the final message_delta/usage is known.
 */
export function getToolInputQuarantineReason(
  block: ToolUseBlock,
): ToolInputQuarantineReason | null {
  if (isMalformedToolInput(block.input)) return 'malformed_json'
  if (isFileMutationToolName(block.name)) {
    if (isRecord(block.input) && Object.keys(block.input).length === 0) {
      return 'empty_file_mutation'
    }
    return 'file_mutation_pending_terminal'
  }
  return null
}

export type ToolInputTerminalMetadata = {
  outputTokens: number | null | undefined
  stopReason: string | null | undefined
  maxOutputTokens: number
}

export type ToolInputQuarantineDecision =
  | { kind: 'release' }
  | {
      kind: 'retry_with_larger_output'
      reason: 'likely_truncated_tool_input'
    }
  | {
      kind: 'fail'
      reason:
        | 'malformed_tool_input'
        | 'likely_truncated_tool_input'
        | 'missing_terminal_metadata'
    }

export function isOutputNearLimit(
  outputTokens: number,
  maxOutputTokens: number,
): boolean {
  if (
    !Number.isFinite(outputTokens) ||
    !Number.isFinite(maxOutputTokens) ||
    outputTokens < 0 ||
    maxOutputTokens <= 0
  ) {
    return false
  }
  return (
    outputTokens >= maxOutputTokens * 0.98 ||
    (maxOutputTokens > 128 && outputTokens >= maxOutputTokens - 128)
  )
}

/**
 * Resolve a quarantined input only after the stream has reached a terminal
 * message_delta. Low-token file mutations (including a real `{}`) are released
 * after finality — `{}` then reaches ordinary schema validation. Malformed JSON
 * is never released. Near-limit failures may take one caller-controlled
 * high-budget retry.
 */
export function decideToolInputQuarantine(options: {
  reason: ToolInputQuarantineReason
  terminal: ToolInputTerminalMetadata
  mayRetryWithLargerOutput: boolean
}): ToolInputQuarantineDecision {
  const { reason, terminal, mayRetryWithLargerOutput } = options
  if (
    typeof terminal.stopReason !== 'string' ||
    terminal.stopReason.length === 0 ||
    typeof terminal.outputTokens !== 'number' ||
    !Number.isFinite(terminal.outputTokens)
  ) {
    return { kind: 'fail', reason: 'missing_terminal_metadata' }
  }

  const nearLimit = isOutputNearLimit(
    terminal.outputTokens,
    terminal.maxOutputTokens,
  )
  if (nearLimit) {
    return mayRetryWithLargerOutput
      ? {
          kind: 'retry_with_larger_output',
          reason: 'likely_truncated_tool_input',
        }
      : { kind: 'fail', reason: 'likely_truncated_tool_input' }
  }

  if (reason === 'malformed_json') {
    return { kind: 'fail', reason: 'malformed_tool_input' }
  }
  return { kind: 'release' }
}

export function buildToolInputQuarantineError(options: {
  toolName: string
  decision: Extract<ToolInputQuarantineDecision, { kind: 'fail' }>
  inputLength?: number
  /**
   * Stream terminal metadata for honest attribution (2026-07-17 audit RC-2
   * 连带项): an upstream-truncated tool JSON classified as malformed must carry
   * the evidence (stop_reason/output_tokens vs requested max) instead of
   * reading as "the model wrote bad JSON" with nothing to go on.
   */
  terminal?: ToolInputTerminalMetadata
}): string {
  const toolName = options.toolName.replace(/[^A-Za-z0-9_.:-]/g, '_').slice(0, 80)
  const lengthDetail =
    typeof options.inputLength === 'number'
      ? ` (${options.inputLength} characters before parsing)`
      : ''
  const diagnostics = options.terminal
    ? ` [stream diagnostics: stop_reason=${options.terminal.stopReason ?? 'unknown'}, output_tokens=${options.terminal.outputTokens ?? 'unknown'}, requested_max_tokens=${options.terminal.maxOutputTokens}]`
    : ''

  if (options.decision.reason === 'missing_terminal_metadata') {
    return `The ${toolName} tool input was quarantined because the model stream ended without final usage/stop metadata. The tool was not executed.${diagnostics}`
  }
  if (options.decision.reason === 'likely_truncated_tool_input') {
    return `The ${toolName} tool input appears truncated at the output-token limit${lengthDetail}. The tool was not executed.${diagnostics} ${LARGE_ARTIFACT_RECOVERY_INSTRUCTION}`
  }
  return `The model returned malformed JSON for the ${toolName} tool${lengthDetail}. The tool was quarantined and not executed.${diagnostics}`
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
