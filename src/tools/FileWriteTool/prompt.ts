import { BASH_TOOL_NAME } from '../BashTool/toolName.js'
import { FILE_READ_TOOL_NAME } from '../FileReadTool/toolName.js'

export const FILE_WRITE_TOOL_NAME = 'Write'
export const DESCRIPTION = 'Write a file to the local filesystem.'

export const LARGE_ARTIFACT_RECOVERY_INSTRUCTION =
  'Switch to a source-to-target local converter or bounded incremental chunks; do not resend the same complete artifact or inline it in a shell command.'

function getPreReadInstruction(): string {
  return `\n- If this is an existing file, you MUST use the ${FILE_READ_TOOL_NAME} tool first to read the file's contents. This tool will fail if you did not read the file first.`
}

export function getWriteToolDescription(): string {
  return `Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.${getPreReadInstruction()}
- Prefer the Edit tool for modifying existing files \u2014 it only sends the diff. Only use this tool to create new files or for complete rewrites.
- Large artifact strategy: for long generated HTML, Markdown, JSON, reports, or source-to-target conversions that may exceed the response budget, do not send the entire artifact as one Write.content payload.
- If source material already exists, read it first, then prefer a short local converter or Node/Python script run through ${BASH_TOOL_NAME} (when available and permitted). Pass source and target paths as arguments; never interpolate source contents into the shell command, and keep the normal approval/sandbox boundary.
- If conversion is not possible, build the artifact in bounded sections or chunks and read back the target to verify it. Do not replace one oversized Write payload with an oversized echo, heredoc, or inline shell payload.
- NEVER create documentation files (*.md) or README files unless explicitly requested by the User.
- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked.`
}
