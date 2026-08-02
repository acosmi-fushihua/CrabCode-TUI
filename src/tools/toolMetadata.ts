import { TOOL_SUMMARY_MAX_LENGTH } from '../constants/toolLimits.js'
import { getDisplayPath } from '../utils/file.js'
import { truncate } from '../utils/format.js'
import { getPlansDirectory } from '../utils/plans.js'
import { getTaskOutputDir } from '../utils/task/diskOutput.js'

export function getAgentOutputTaskId(filePath: string): string | null {
  const prefix = `${getTaskOutputDir()}/`
  const suffix = '.output'
  if (!filePath.startsWith(prefix) || !filePath.endsWith(suffix)) return null
  const taskId = filePath.slice(prefix.length, -suffix.length)
  return taskId.length > 0 &&
    taskId.length <= 20 &&
    /^[a-zA-Z0-9_-]+$/.test(taskId)
    ? taskId
    : null
}

export function fileReadUserFacingName(
  input: { file_path?: string } | undefined,
): string {
  if (input?.file_path?.startsWith(getPlansDirectory())) return 'Reading Plan'
  if (input?.file_path && getAgentOutputTaskId(input.file_path)) {
    return 'Read agent output'
  }
  return 'Read'
}

export function fileReadToolUseSummary(
  input: { file_path?: string } | undefined,
): string | null {
  if (!input?.file_path) return null
  return getAgentOutputTaskId(input.file_path) ?? getDisplayPath(input.file_path)
}

export function fileEditUserFacingName(
  input:
    | {
        file_path?: string
        old_string?: string
        edits?: unknown[]
      }
    | undefined,
): string {
  if (!input) return 'Update'
  if (input.file_path?.startsWith(getPlansDirectory())) return 'Updated plan'
  if (input.edits != null) return 'Update'
  return input.old_string === '' ? 'Create' : 'Update'
}

export function fileEditToolUseSummary(
  input: { file_path?: string } | undefined,
): string | null {
  return input?.file_path ? getDisplayPath(input.file_path) : null
}

export function fileWriteUserFacingName(
  input: { file_path?: string } | undefined,
): string {
  return input?.file_path?.startsWith(getPlansDirectory())
    ? 'Updated plan'
    : 'Write'
}

const FILE_WRITE_PREVIEW_LINES = 10

export function isFileWriteResultTruncated(output: {
  type: string
  content: string
}): boolean {
  if (output.type !== 'create') return false
  let position = 0
  for (let index = 0; index < FILE_WRITE_PREVIEW_LINES; index++) {
    position = output.content.indexOf('\n', position)
    if (position === -1) return false
    position++
  }
  return position < output.content.length
}

export function fileWriteToolUseSummary(
  input: { file_path?: string } | undefined,
): string | null {
  return input?.file_path ? getDisplayPath(input.file_path) : null
}

export function searchUserFacingName(): string {
  return 'Search'
}

export function searchToolUseSummary(
  input: { pattern?: string } | undefined,
): string | null {
  return input?.pattern
    ? truncate(input.pattern, TOOL_SUMMARY_MAX_LENGTH)
    : null
}

export function webFetchToolUseSummary(
  input: { url?: string } | undefined,
): string | null {
  return input?.url ? truncate(input.url, TOOL_SUMMARY_MAX_LENGTH) : null
}

export function webSearchToolUseSummary(
  input: { query?: string } | undefined,
): string | null {
  return input?.query ? truncate(input.query, TOOL_SUMMARY_MAX_LENGTH) : null
}

export function notebookToolUseSummary(
  input: { notebook_path?: string } | undefined,
): string | null {
  return input?.notebook_path ? getDisplayPath(input.notebook_path) : null
}

export function lspUserFacingName(): string {
  return 'LSP'
}

export function readMcpResourceUserFacingName(): string {
  return 'readMcpResource'
}
