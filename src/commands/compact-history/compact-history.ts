import { readFile } from 'fs/promises'
import { getShortcutDisplay } from '../../keybindings/shortcutFormat.js'
import type { LocalCommandCall } from '../../types/command.js'
import {
  errorMessage,
  getErrnoCode,
  isENOENT,
} from '../../utils/errors.js'
import { getTranscriptPath } from '../../utils/sessionStorage.js'
import { formatCompactHistory, scanCompactBoundaries } from './historyCore.js'

/**
 * /compact-history — read-only, local (no LLM call).
 *
 * The transcript is append-only and compaction never deletes the original
 * messages from disk — it only appends a `compact_boundary` marker (see
 * createCompactBoundaryMessage in utils/messages/factory.ts). This command
 * scans the current session's transcript file for those markers and lists
 * each compaction event, so users can see that the pre-compaction original
 * text is still on disk and where to find it.
 *
 * Scanning/formatting live in ./historyCore.ts (pure, unit-tested); this
 * module only wires in the transcript path, file read, and keybinding lookup.
 */

export const call: LocalCommandCall = async (_args, context) => {
  const transcriptPath = getTranscriptPath()
  const signal = context.abortController.signal

  // Unlike an LLM turn, this command has no downstream generator that will
  // observe an already-cancelled signal for us. Keep an explicit checkpoint
  // before starting physical I/O so Esc/Ctrl-C cannot be reported as success.
  signal.throwIfAborted()

  // Missing file (nothing persisted yet) reads the same as "no compactions".
  // Every other I/O failure is real terminal truth: treating EACCES/EISDIR as
  // empty history made a broken transcript look healthy.
  let content = ''
  try {
    content = await readFile(transcriptPath, {
      encoding: 'utf8',
      signal,
    })
  } catch (error) {
    if (signal.aborted) signal.throwIfAborted()
    if (!isENOENT(error)) {
      const code = getErrnoCode(error)
      throw new Error(
        `Unable to read compact-history transcript${code ? ` (${code})` : ''}: ${errorMessage(error)}`,
        { cause: error },
      )
    }
  }

  // readFile cancellation is best-effort. Cover the race where it resolves
  // just before the abort notification, before synchronous scanning begins.
  signal.throwIfAborted()

  const expandShortcut = getShortcutDisplay(
    'app:toggleTranscript',
    'Global',
    'ctrl+o',
  )

  return {
    type: 'text',
    value: formatCompactHistory(scanCompactBoundaries(content), {
      transcriptPath,
      expandShortcut,
    }),
  }
}
