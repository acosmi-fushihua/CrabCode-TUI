/**
 * sessionStorage-paths.ts — Session path/directory helpers, shared constants,
 * type guards, and lightweight utility types.
 *
 * Split from sessionStorage.ts (refactor, no logic changes).
 * Zero internal dependencies — this is the leaf of the dependency tree.
 */
import { feature } from './featurePolyfill.js'
import type { UUID } from 'crypto'
import memoize from 'lodash-es/memoize.js'
import { join } from 'path'
import {
  getOriginalCwd,
  getSessionId,
  getSessionOverrideProjectRootCwd,
  getSessionProjectDir,
} from '../bootstrap/state.js'
import type { Entry, TranscriptMessage } from '../types/logs.js'
import type {
  AssistantMessage,
  AttachmentMessage,
  Message,
  SystemMessage,
  UserMessage,
} from '../types/message.js'
import { getCrabCodeConfigHomeDir } from './envUtils.js'
import { getFsImplementation } from './fsOperations.js'
import { resolveMacroVersion } from './macroVersion.js'
import { sanitizePath } from './path.js'

// ─── Constants ───────────────────────────────────────────────────────────────

// W-TRANSCRIPT-DUALWRITE RC-1c (E10): the old `typeof MACRO !== 'undefined'`
// guard ALWAYS fell to 'unknown' in bundled builds (bun `define` substitutes
// `MACRO.VERSION` member access but never the bare `MACRO` identifier, so
// `typeof MACRO` is 'undefined' and short-circuits). resolveMacroVersion()
// accesses `MACRO.VERSION` directly inside try/catch, cached at module init —
// also preserving the bun#26168 async-context workaround. See macroVersion.ts.
export const VERSION = resolveMacroVersion()

// 50MB — prevents OOM in the tombstone slow path which reads + rewrites the
// entire session file. Session files can grow to multiple GB (inc-3930).
export const MAX_TOMBSTONE_REWRITE_BYTES = 50 * 1024 * 1024

/**
 * Pre-compiled regex to skip non-meaningful messages when extracting first prompt.
 * Matches anything starting with a lowercase XML-like tag (IDE context, hook
 * output, task notifications, channel messages, etc.) or a synthetic interrupt
 * marker. Kept in sync with sessionStoragePortable.ts — generic pattern avoids
 * an ever-growing allowlist that falls behind as new notification types ship.
 */
export const SKIP_FIRST_PROMPT_PATTERN =
  /^(?:\s*<[a-z][\w-]*[\s>]|\[Request interrupted by user[^\]]*\])/

// 50 MB — session JSONL can grow to multiple GB (inc-3930). Callers that
// read the raw transcript must bail out above this threshold to avoid OOM.
export const MAX_TRANSCRIPT_READ_BYTES = 50 * 1024 * 1024


// ─── Types ───────────────────────────────────────────────────────────────────

export type Transcript = (
  | UserMessage
  | AssistantMessage
  | AttachmentMessage
  | SystemMessage
)[]

export type TeamInfo = {
  teamName?: string
  agentName?: string
}

export type LegacyProgressEntry = {
  type: 'progress'
  uuid: UUID
  parentUuid: UUID | null
}

// ─── Type guards ─────────────────────────────────────────────────────────────

/**
 * Type guard to check if an entry is a transcript message.
 * Transcript messages include user, assistant, attachment, and system messages.
 * IMPORTANT: This is the single source of truth for what constitutes a transcript message.
 * loadTranscriptFile() uses this to determine which messages to load into the chain.
 *
 * Progress messages are NOT transcript messages. They are ephemeral UI state
 * and must not be persisted to the JSONL or participate in the parentUuid
 * chain. Including them caused chain forks that orphaned real conversation
 * messages on resume (see #14373, #23537).
 */
export function isTranscriptMessage(entry: Entry): entry is TranscriptMessage {
  return (
    entry.type === 'user' ||
    entry.type === 'assistant' ||
    entry.type === 'attachment' ||
    entry.type === 'system'
  )
}

/**
 * Entries that participate in the parentUuid chain. Used on the write path
 * (insertMessageChain, useLogMessages) to skip progress when assigning
 * parentUuid. Old transcripts with progress already in the chain are handled
 * by the progressBridge rewrite in loadTranscriptFile.
 */
export function isChainParticipant(m: Pick<Message, 'type'>): boolean {
  return m.type !== 'progress'
}

/**
 * High-frequency tool progress ticks (1/sec for Sleep, per-chunk for Bash).
 * These are UI-only: not sent to the API, not rendered after the tool
 * completes. Used by REPL.tsx to replace-in-place instead of appending, and
 * by loadTranscriptFile to skip legacy entries from old transcripts.
 */
const EPHEMERAL_PROGRESS_TYPES = new Set([
  'bash_progress',
  'powershell_progress',
  'mcp_progress',
  ...(feature('PROACTIVE') || feature('KAIROS')
    ? (['sleep_progress'] as const)
    : []),
])
export function isEphemeralToolProgress(dataType: unknown): boolean {
  return typeof dataType === 'string' && EPHEMERAL_PROGRESS_TYPES.has(dataType)
}

/**
 * Progress entries in transcripts written before PR #24099. They are not
 * in the Entry type union anymore but still exist on disk with uuid and
 * parentUuid fields. loadTranscriptFile bridges the chain across them.
 */
export function isLegacyProgressEntry(
  entry: unknown,
): entry is LegacyProgressEntry {
  return (
    typeof entry === 'object' &&
    entry !== null &&
    'type' in entry &&
    entry.type === 'progress' &&
    'uuid' in entry &&
    typeof entry.uuid === 'string'
  )
}

// ─── Path helpers ────────────────────────────────────────────────────────────

export function getProjectsDir(): string {
  return join(getCrabCodeConfigHomeDir(), 'projects')
}

// Memoized: called 12+ times per turn via hooks.ts createBaseHookInput
// (PostToolUse path, 5×/turn) + various save* functions. Input is a cwd
// string; homedir/env/regex are all session-invariant so the result is
// stable for a given input. Worktree switches just change the key — no
// cache clear needed.
export const getProjectDir = memoize((projectDir: string): string => {
  return join(getProjectsDir(), sanitizePath(projectDir))
})

export function getTranscriptPath(): string {
  // A per-turn override binds transcript storage to that turn's project root.
  // An explicit sessionProjectDir still wins for worktree/cross-project resume.
  const overrideCwd = getSessionOverrideProjectRootCwd()
  const projectDir =
    getSessionProjectDir() ?? getProjectDir(overrideCwd ?? getOriginalCwd())
  return join(projectDir, `${getSessionId()}.jsonl`)
}

export function getTranscriptPathForSession(sessionId: string): string {
  // When asking for the CURRENT session's transcript, honor sessionProjectDir
  // the same way getTranscriptPath() does. Without this, hooks get a
  // transcript_path computed from originalCwd while the actual file was
  // written to sessionProjectDir (set by switchActiveSession on resume/branch)
  // — different directories, so the hook sees MISSING (gh-30217). CC-34
  // made sessionId + sessionProjectDir atomic precisely to prevent this
  // kind of drift; this function just wasn't updated to read both.
  //
  // For OTHER session IDs we can only guess via originalCwd — we don't
  // track a sessionId→projectDir map. Callers wanting a specific other
  // session's path should pass fullPath explicitly (most save* functions
  // already accept this).
  if (sessionId === getSessionId()) {
    return getTranscriptPath()
  }
  const projectDir = getProjectDir(getOriginalCwd())
  return join(projectDir, `${sessionId}.jsonl`)
}

export function sessionIdExists(sessionId: string): boolean {
  const projectDir = getProjectDir(getOriginalCwd())
  const sessionFile = join(projectDir, `${sessionId}.jsonl`)
  const fs = getFsImplementation()
  try {
    fs.statSync(sessionFile)
    return true
  } catch {
    return false
  }
}

// exported for testing
export function getNodeEnv(): string {
  return process.env.NODE_ENV || 'development'
}

// exported for testing
export function getUserType(): string {
  return process.env.USER_TYPE || 'external'
}

export function getEntrypoint(): string | undefined {
  return process.env.CRABCODE_ENTRYPOINT
}

export function isCustomTitleEnabled(): boolean {
  return true
}
