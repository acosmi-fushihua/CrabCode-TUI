import type { ContentBlockParam } from '../types/api-types.js'

export type DirectTuiInputRoute = {
  mode: 'prompt' | 'bash'
  value: string | ContentBlockParam[]
}

export function isDirectTuiBashContentBlocks(
  content: ContentBlockParam[],
): boolean {
  return (
    content.length > 0 &&
    content.at(-1)?.type === 'text' &&
    content.slice(0, -1).every(block => block.type !== 'text')
  )
}

/**
 * Restore the fixed CrabCode input-mode rule at the native TUI ingress.
 *
 * The historical composer selected bash mode only when the first character
 * was `!`, then removed exactly that one character before execution. The
 * native composer sends the same user text through the existing StructuredIO
 * user envelope, so this remains a process-local routing decision: no input
 * mode or command field is added to the transport.
 *
 * With native image attachments, the fixed current composer emits one leading
 * text block followed by image blocks. processUserInput's existing bash and
 * slash-command paths consume their command from the final text block and
 * treat earlier blocks as attachments, so move that one composer block behind
 * the unchanged attachment blocks. Ambiguous multi-text payloads remain
 * ordinary prompts.
 */
export function routeDirectTuiInput(
  content: string | ContentBlockParam[],
): DirectTuiInputRoute | null {
  if (typeof content === 'string') {
    if (!content.startsWith('!')) {
      return { mode: 'prompt', value: content }
    }
    const command = content.slice(1)
    // In the fixed composer, the first `!` selected bash mode without
    // entering the text buffer. Enter therefore saw an empty input and did
    // not submit. Preserve that boundary instead of executing an empty shell.
    return command.trim().length === 0
      ? null
      : { mode: 'bash', value: command }
  }

  const [composerText, ...attachmentBlocks] = content
  if (
    composerText?.type === 'text' &&
    composerText.text.startsWith('!') &&
    attachmentBlocks.every(block => block.type !== 'text')
  ) {
    const command = composerText.text.slice(1)
    if (attachmentBlocks.length === 0 && command.trim().length === 0) {
      return null
    }
    return {
      mode: 'bash',
      value: [
        ...attachmentBlocks,
        { ...composerText, text: command },
      ],
    }
  }

  if (
    composerText?.type === 'text' &&
    composerText.text.trimStart().startsWith('/') &&
    attachmentBlocks.length > 0 &&
    attachmentBlocks.every(block => block.type !== 'text')
  ) {
    return {
      mode: 'prompt',
      value: [...attachmentBlocks, composerText],
    }
  }

  return { mode: 'prompt', value: content }
}
