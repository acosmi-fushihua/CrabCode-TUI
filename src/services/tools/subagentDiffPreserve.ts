// Preserve minimal diff metadata when stripping a sub-agent's rich `toolUseResult`.
//
// Background: `toolExecution.ts` sets `toolUseResult` to `undefined` for
// sub-agent tool results (when `agentId && !preserveToolUseResults`) to trim the
// persisted / in-memory message size. `toolUseResult` is NOT model context — the
// model only sees the `tool_result` content block; this field maps to the SDK's
// `tool_use_result`, which is renderer-only metadata (not an Anthropic API field).
//
// But that field is the SOLE data source the renderer uses to show a file edit's
// `+N / −N` diff (worker `diffStringFromToolResult` reads `structuredPatch` /
// `content` / `gitDiff` / `oldString` / `newString`). Once it is `undefined`, the
// worker early-returns an empty diff and the edit renders as `−0` — in the main
// stream (pre PR-A) and, because the same stripped message is what gets recorded
// to the sidechain `subagents/agent-<id>.jsonl`, in the sub-window too.
//
// So for file-edit tools we keep the diff *sources* and drop only the heavy
// redundant `originalFile` (the full prior file — never read by the renderer) and
// the full `content` of an update (the non-empty `structuredPatch` already
// carries that diff). Non-file-edit tools keep returning `undefined` (full strip).

import { FILE_WRITE_TOOL_NAME } from '../../tools/FileWriteTool/prompt.js'
import { FILE_EDIT_TOOL_NAME } from '../../tools/FileEditTool/constants.js'

/**
 * Tools whose `toolUseResult` carries a renderable diff (`structuredPatch` /
 * `content`). `NotebookEdit` is deliberately excluded: it produces neither, so
 * preserving its result fixes no `−0` and would just be noise.
 */
const DIFF_BEARING_FILE_EDIT_TOOLS: ReadonlySet<string> = new Set([
  FILE_WRITE_TOOL_NAME, // 'Write'
  FILE_EDIT_TOOL_NAME, // 'Edit'
])

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/**
 * Project a sub-agent's file-edit `toolUseResult` down to the minimal diff
 * metadata the renderer consumes. Returns `undefined` for non-file-edit tools
 * (the caller then fully strips, preserving prior behavior).
 *
 * Kept: `structuredPatch`, `gitDiff`, `oldString`, `newString`, `filePath`,
 * a `type` add/update discriminator, and `content` ONLY when there is no
 * non-empty `structuredPatch` (the new-file create path, where `content` is the
 * only diff source). Dropped: `originalFile` always, and `content` on updates.
 *
 * The `type` discriminator pairs with the worker's `patchChangeKindFromToolResult`,
 * which now prefers `data.type` (`'create'` → add / `'update'` → update) before
 * falling back to the `originalFile` emptiness heuristic — so dropping
 * `originalFile` does not misclassify edits as new-file adds. `Write` already
 * emits `type`; `Edit` does not but always targets an existing file, so we
 * inject `type: 'update'`.
 */
export function minimalDiffResultForStrippedSubagent(
  toolName: string,
  toolUseResult: unknown,
): unknown {
  if (!DIFF_BEARING_FILE_EDIT_TOOLS.has(toolName)) return undefined
  if (!isRecord(toolUseResult)) return undefined
  // File-edit tools return a flat `data` object as their result, but tolerate a
  // `{ data: {...} }` wrapper defensively (matches the worker's own unwrap).
  const data = isRecord(toolUseResult.data) ? toolUseResult.data : toolUseResult

  const out: Record<string, unknown> = {}

  const filePath = data.filePath ?? data.file_path
  if (filePath !== undefined) out.filePath = filePath

  // add/update discriminator (see doc comment).
  if (typeof data.type === 'string') {
    out.type = data.type
  } else if (toolName === FILE_EDIT_TOOL_NAME) {
    out.type = 'update'
  }

  const structuredPatch = data.structuredPatch
  if (structuredPatch !== undefined) out.structuredPatch = structuredPatch
  if (data.gitDiff !== undefined) out.gitDiff = data.gitDiff
  if (typeof data.oldString === 'string') out.oldString = data.oldString
  if (typeof data.newString === 'string') out.newString = data.newString

  // `content` is only needed for the create-diff path (absent / empty patch);
  // on updates the non-empty `structuredPatch` already carries the diff, so the
  // full new-file body is dropped.
  const patchEmpty =
    !Array.isArray(structuredPatch) || structuredPatch.length === 0
  if (patchEmpty && typeof data.content === 'string') out.content = data.content

  return out
}
