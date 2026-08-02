import chalk from 'chalk'
import { t } from '../../i18n/index.js'
import type { CompactMetadata } from '../../types/message.js'
import { formatTokens } from '../../utils/format.js'

/**
 * Pure scanning/formatting core for /compact-history. Kept separate from
 * compact-history.ts (the LocalCommandCall) so it stays importable without
 * pulling the sessionStorage / keybindings module graphs — those have
 * load-order-sensitive cycles that blow up under direct test import (same
 * reason tests/unit/commands.test.ts can't import commands.ts).
 */

/**
 * Substring pre-filter for candidate lines, then each candidate is confirmed
 * by parsing and checking type/subtype — mirroring the marker-confirmation
 * pattern in sessionStoragePortable.ts (compactBoundaryMarker +
 * parseBoundaryLine): the marker can also appear inside user content, so a
 * substring hit alone is not proof.
 */
const COMPACT_BOUNDARY_MARKER = '"compact_boundary"'

export interface CompactBoundarySummary {
  timestamp?: string
  trigger?: CompactMetadata['trigger']
  preTokens?: number
  messagesSummarized?: number
}

/**
 * Scan raw transcript JSONL content for compact_boundary entries, in file
 * (= chronological, append-only) order. Missing/invalid fields are omitted
 * from the summary rather than fabricated.
 */
export function scanCompactBoundaries(
  jsonlContent: string,
): CompactBoundarySummary[] {
  const boundaries: CompactBoundarySummary[] = []
  for (const line of jsonlContent.split('\n')) {
    if (!line.includes(COMPACT_BOUNDARY_MARKER)) continue
    let parsed: {
      type?: unknown
      subtype?: unknown
      timestamp?: unknown
      compactMetadata?: {
        trigger?: unknown
        preTokens?: unknown
        messagesSummarized?: unknown
      }
    }
    try {
      parsed = JSON.parse(line)
    } catch {
      continue
    }
    if (parsed?.type !== 'system' || parsed.subtype !== 'compact_boundary') {
      continue
    }
    const meta = parsed.compactMetadata
    const summary: CompactBoundarySummary = {}
    if (typeof parsed.timestamp === 'string') {
      summary.timestamp = parsed.timestamp
    }
    if (meta?.trigger === 'manual' || meta?.trigger === 'auto') {
      summary.trigger = meta.trigger
    }
    if (typeof meta?.preTokens === 'number' && Number.isFinite(meta.preTokens)) {
      summary.preTokens = meta.preTokens
    }
    if (
      typeof meta?.messagesSummarized === 'number' &&
      Number.isFinite(meta.messagesSummarized)
    ) {
      summary.messagesSummarized = meta.messagesSummarized
    }
    boundaries.push(summary)
  }
  return boundaries
}

/**
 * Render the history listing. `expandShortcut` is injected by the caller
 * (getShortcutDisplay) so this stays pure/testable without keybinding I/O.
 */
export function formatCompactHistory(
  boundaries: CompactBoundarySummary[],
  opts: { transcriptPath: string; expandShortcut: string },
): string {
  const footer = chalk.dim(
    [
      t('compact_history_transcript_path', { path: opts.transcriptPath }),
      t('compact_history_open_hint', { shortcut: opts.expandShortcut }),
    ].join('\n'),
  )

  if (boundaries.length === 0) {
    return [t('compact_history_empty'), '', footer].join('\n')
  }

  const lines = boundaries.map(b => {
    const parts: string[] = []
    if (b.timestamp !== undefined) {
      const date = new Date(b.timestamp)
      parts.push(
        Number.isNaN(date.getTime()) ? b.timestamp : date.toLocaleString(),
      )
    }
    if (b.trigger !== undefined) {
      parts.push(
        t(
          b.trigger === 'manual'
            ? 'compact_history_trigger_manual'
            : 'compact_history_trigger_auto',
        ),
      )
    }
    if (b.preTokens !== undefined) {
      parts.push(
        t('compact_history_pre_tokens', { tokens: formatTokens(b.preTokens) }),
      )
    }
    if (b.messagesSummarized !== undefined) {
      parts.push(
        t('compact_history_messages_summarized', {
          count: b.messagesSummarized,
        }),
      )
    }
    return `  ${parts.join(' · ')}`
  })

  return [
    t('compact_history_header', { count: boundaries.length }),
    ...lines,
    '',
    footer,
  ].join('\n')
}
