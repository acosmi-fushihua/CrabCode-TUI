import { readFile } from 'fs/promises'
import { getShortcutDisplay } from '../../keybindings/shortcutFormat.js'
import type { LocalCommandCall } from '../../types/command.js'
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

export const call: LocalCommandCall = async () => {
  const transcriptPath = getTranscriptPath()

  // Missing file (nothing persisted yet) reads the same as "no compactions".
  let content = ''
  try {
    content = await readFile(transcriptPath, 'utf8')
  } catch {
    // ENOENT etc. — fall through with empty content
  }

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
