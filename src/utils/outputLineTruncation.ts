const MAX_LINES_TO_SHOW = 3

/** Fast newline-only counterpart of the interactive output preview check. */
export function isOutputLineTruncated(content: string): boolean {
  let position = 0
  for (let index = 0; index <= MAX_LINES_TO_SHOW; index++) {
    position = content.indexOf('\n', position)
    if (position === -1) return false
    position++
  }
  return position < content.length
}
