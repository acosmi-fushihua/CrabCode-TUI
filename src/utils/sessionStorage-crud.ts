/**
 * sessionStorage-crud.ts — Project class, session file management, transcript
 * recording, queue operations, file-history/attribution/content-replacement
 * persistence and flush.
 *
 * Split from sessionStorage.ts (refactor, no logic changes).
 * Depends on: sessionStorage-paths, sessionStorage-transcript
 */
import type { UUID } from 'crypto'
import {
  appendFile as fsAppendFile,
  open as fsOpen,
  mkdir,
  readFile,
  stat,
  writeFile,
} from 'fs/promises'
import { dirname, join } from 'path'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'
import {
  getOriginalCwd,
  getPlanSlugCache,
  getPromptId,
  getSessionId,
  getSessionProjectDir,
  isSessionPersistenceDisabled,
} from '../bootstrap/state.js'
import {
  type AgentId,
  asAgentId,
  type SessionId,
} from '../types/ids.js'
import type { AttributionSnapshotMessage } from '../types/logs.js'
import {
  type ContentReplacementEntry,
  type ContextCollapseCommitEntry,
  type ContextCollapseSnapshotEntry,
  type Entry,
  type PersistedWorktreeSession,
  type TranscriptMessage,
} from '../types/logs.js'
import type { Message, SystemCompactBoundaryMessage } from '../types/message.js'
import type { QueueOperationMessage } from '../types/messageQueueTypes.js'
import { registerCleanup } from './cleanupRegistry.js'
import { updateSessionName } from './concurrentSessions.js'
import { getCwd } from './cwd.js'
import { logForDebugging } from './debug.js'
import { isEnvTruthy } from './envUtils.js'
import { isFsInaccessible } from './errors.js'
import type { FileHistorySnapshot } from './fileHistory.js'
import { formatFileSize } from './format.js'
import { getFsImplementation } from './fsOperations.js'
import { getBranch } from './git.js'
import { logError } from './log.js'
import { isCompactBoundaryMessage } from './messages.js'
import {
  extractLastJsonStringField,
  LITE_READ_BUF_SIZE,
} from './sessionStoragePortable.js'
import { getAgentTranscriptPath } from './sessionStorage-agent.js'
import {
  type TeamInfo,
  type Transcript,
  getNodeEnv,
  getUserType,
  getEntrypoint,
  getProjectDir,
  getTranscriptPath,
  getTranscriptPathForSession,
  isChainParticipant,
  isTranscriptMessage,
  MAX_TOMBSTONE_REWRITE_BYTES,
  VERSION,
} from './sessionStorage-paths.js'
import { getSettings_DEPRECATED } from './settings/settings.js'
import { jsonParse, jsonStringify } from './slowOperations.js'
import type { ContentReplacementRecord } from './toolResultStorage.js'
// Sync fs primitives for readFileTailSync
import { closeSync, fstatSync, openSync, readSync } from 'fs'
import memoize from 'lodash-es/memoize.js'
import {
  cleanMessagesForLogging,
  getFirstMeaningfulUserMessageTextContent,
} from './sessionStorage-transcript.js'

// ─── Project class ──────────────────────────────────────────────────────────

let project: Project | null = null
let cleanupRegistered = false

// Public transcript record/removal calls are admitted in invocation order.
// `recordTranscript()` resolves after preparing/enqueuing its JSONL entries,
// not after the lazy file drain; the per-file mutation queue below then keeps
// those appends and a following tombstone in the same durable order. This also
// prevents a later same-UUID retry from racing ahead of the removal.
let transcriptMutationTail: Promise<void> = Promise.resolve()

function enqueueTranscriptMutation<T>(mutation: () => Promise<T>): Promise<T> {
  const result = transcriptMutationTail.then(mutation)
  // Keep the chain live after a fail-soft persistence error, and attach the
  // rejection handler immediately for deliberately fire-and-forget callers.
  transcriptMutationTail = result.then(
    () => undefined,
    () => undefined,
  )
  return result
}

function getProject(): Project {
  if (!project) {
    project = new Project()

    // Register flush as a cleanup handler (only once)
    if (!cleanupRegistered) {
      registerCleanup(async () => {
        // Flush queued writes first, then re-append session metadata
        // (customTitle, tag) so they always appear in the last 64KB tail
        // window. readLiteMetadata only reads the tail to extract these
        // fields — if enough messages are appended after a /rename, the
        // custom-title entry gets pushed outside the window and --resume
        // shows the auto-generated firstPrompt instead.
        await project?.flush()
        try {
          project?.reAppendSessionMetadata()
        } catch {
          // Best-effort — don't let metadata re-append crash the cleanup
        }
      })
      cleanupRegistered = true
    }
  }
  return project
}

/**
 * Reset the Project singleton's flush state for testing.
 * This ensures tests don't interfere with each other via shared counter state.
 */
export function resetProjectFlushStateForTesting(): void {
  project?._resetFlushState()
}

/**
 * Reset the entire Project singleton for testing.
 * This ensures tests with different CRABCODE_CONFIG_DIR values
 * don't share stale sessionFile paths.
 */
export function resetProjectForTesting(): void {
  project = null
  transcriptMutationTail = Promise.resolve()
}

export function setSessionFileForTesting(path: string): void {
  getProject().sessionFile = path
}

class Project {
  // Minimal cache for current session only (not all sessions)
  currentSessionTag: string | undefined
  currentSessionTitle: string | undefined
  currentSessionAgentName: string | undefined
  currentSessionAgentColor: string | undefined
  currentSessionLastPrompt: string | undefined
  currentSessionAgentSetting: string | undefined
  currentSessionMode: 'coordinator' | 'normal' | undefined
  // Tri-state: undefined = never touched (don't write), null = exited worktree,
  // object = currently in worktree. reAppendSessionMetadata writes null so
  // --resume knows the session exited (vs. crashed while inside).
  currentSessionWorktree: PersistedWorktreeSession | null | undefined
  currentSessionPrNumber: number | undefined
  currentSessionPrUrl: string | undefined
  currentSessionPrRepository: string | undefined

  sessionFile: string | null = null
  // Entries buffered while sessionFile is null. Flushed by materializeSessionFile
  // on the first user/assistant message — prevents metadata-only session files.
  private pendingEntries: Entry[] = []
  private pendingWriteCount: number = 0
  private flushResolvers: Array<() => void> = []
  // Per-file mutation queues. Appends and tombstone removals share this one
  // ordered stream so an r+/truncate removal can never race an append drain.
  private writeQueues = new Map<
    string,
    Array<
      | { kind: 'append'; entry: Entry; resolve: () => void }
      | { kind: 'remove-message'; targetUuid: UUID; resolve: () => void }
    >
  >()
  private flushTimer: ReturnType<typeof setTimeout> | null = null
  private activeDrain: Promise<void> | null = null
  private FLUSH_INTERVAL_MS = 100
  private readonly MAX_CHUNK_BYTES = 100 * 1024 * 1024

  constructor() {}

  /** @internal Reset flush/queue state for testing. */
  _resetFlushState(): void {
    this.pendingWriteCount = 0
    this.flushResolvers = []
    if (this.flushTimer) clearTimeout(this.flushTimer)
    this.flushTimer = null
    this.activeDrain = null
    this.writeQueues = new Map()
  }

  private incrementPendingWrites(): void {
    this.pendingWriteCount++
  }

  private decrementPendingWrites(): void {
    this.pendingWriteCount--
    if (this.pendingWriteCount === 0) {
      // Resolve all waiting flush promises
      for (const resolve of this.flushResolvers) {
        resolve()
      }
      this.flushResolvers = []
    }
  }

  private async trackWrite<T>(fn: () => Promise<T>): Promise<T> {
    this.incrementPendingWrites()
    try {
      return await fn()
    } finally {
      this.decrementPendingWrites()
    }
  }

  private enqueueWrite(filePath: string, entry: Entry): Promise<void> {
    return new Promise<void>(resolve => {
      let queue = this.writeQueues.get(filePath)
      if (!queue) {
        queue = []
        this.writeQueues.set(filePath, queue)
      }
      queue.push({ kind: 'append', entry, resolve })
      this.scheduleDrain()
    })
  }

  private enqueueMessageRemoval(
    filePath: string,
    targetUuid: UUID,
  ): Promise<void> {
    return new Promise<void>(resolve => {
      let queue = this.writeQueues.get(filePath)
      if (!queue) {
        queue = []
        this.writeQueues.set(filePath, queue)
      }
      queue.push({ kind: 'remove-message', targetUuid, resolve })
      this.scheduleDrain()
    })
  }

  private scheduleDrain(): void {
    if (this.flushTimer) {
      return
    }
    this.flushTimer = setTimeout(async () => {
      this.flushTimer = null
      this.activeDrain = this.drainWriteQueue()
      await this.activeDrain
      this.activeDrain = null
      // If more items arrived during drain, schedule again
      if (this.writeQueues.size > 0) {
        this.scheduleDrain()
      }
    }, this.FLUSH_INTERVAL_MS)
  }

  private async appendToFile(filePath: string, data: string): Promise<void> {
    try {
      await fsAppendFile(filePath, data, { mode: 0o600 })
    } catch {
      // Directory may not exist — some NFS-like filesystems return
      // unexpected error codes, so don't discriminate on code.
      await mkdir(dirname(filePath), { recursive: true, mode: 0o700 })
      await fsAppendFile(filePath, data, { mode: 0o600 })
    }
  }

  private async drainWriteQueue(): Promise<void> {
    for (const [filePath, queue] of this.writeQueues) {
      if (queue.length === 0) {
        continue
      }
      const batch = queue.splice(0)

      let content = ''
      const resolvers: Array<() => void> = []

      const flushAppendBatch = async (): Promise<void> => {
        if (content.length === 0) return
        await this.appendToFile(filePath, content)
        content = ''
        for (const resolve of resolvers.splice(0)) resolve()
      }

      for (const mutation of batch) {
        if (mutation.kind === 'remove-message') {
          // Preserve file order: every earlier append is durable before the
          // positional truncate, and every later append runs after it.
          await flushAppendBatch()
          await this.removeMessageFromFile(filePath, mutation.targetUuid)
          mutation.resolve()
          continue
        }

        const line = jsonStringify(mutation.entry) + '\n'

        if (content.length + line.length >= this.MAX_CHUNK_BYTES) {
          // Flush chunk and resolve its entries before starting a new one
          await flushAppendBatch()
        }

        content += line
        resolvers.push(mutation.resolve)
      }

      await flushAppendBatch()
    }

    // Clean up empty queues
    for (const [filePath, queue] of this.writeQueues) {
      if (queue.length === 0) {
        this.writeQueues.delete(filePath)
      }
    }
  }

  resetSessionFile(): void {
    this.sessionFile = null
    this.pendingEntries = []
  }

  /**
   * Re-append cached session metadata to the end of the transcript file.
   * This ensures metadata stays within the tail window that readLiteMetadata
   * reads during progressive loading.
   *
   * Called from two contexts with different file-ordering implications:
   * - During compaction (compact.ts, reactiveCompact.ts): writes metadata
   *   just before the boundary marker is emitted - these entries end up
   *   before the boundary and are recovered by scanPreBoundaryMetadata.
   * - On session exit (cleanup handler): writes metadata at EOF after all
   *   boundaries - this is what enables loadTranscriptFile's pre-compact
   *   skip to find metadata without a forward scan.
   *
   * External-writer safety for SDK-mutable fields (custom-title, tag):
   * before re-appending, refresh the cache from the tail scan window. If an
   * external process (SDK renameSession/tagSession) wrote a fresher value,
   * our stale cache absorbs it and the re-append below persists it — not
   * the stale CLI value. If no entry is in the tail (evicted, or never
   * written by the SDK), the cache is the only source of truth and is
   * re-appended as-is.
   *
   * Re-append is unconditional (even when the value is already in the
   * tail): during compaction, a title 40KB from EOF is inside the current
   * tail window but will fall out once the post-compaction session grows.
   * Skipping the re-append would defeat the purpose of this call. Fields
   * the SDK cannot touch (last-prompt, agent-*, mode, pr-link) have no
   * external-writer concern — their caches are authoritative.
   */
  reAppendSessionMetadata(skipTitleRefresh = false): void {
    if (!this.sessionFile) return
    const sessionId = getSessionId() as UUID
    if (!sessionId) return

    // One sync tail read to refresh SDK-mutable fields. Same
    // LITE_READ_BUF_SIZE window readLiteMetadata uses. Empty string on
    // failure → extract returns null → cache is the only source of truth.
    const tail = readFileTailSync(this.sessionFile)

    // Absorb any fresher SDK-written title/tag into our cache. If the SDK
    // wrote while we had the session open, our cache is stale — the tail
    // value is authoritative. If the tail has nothing (evicted or never
    // written externally), the cache stands.
    //
    // Filter with startsWith to match only top-level JSONL entries (col 0)
    // and not "type":"tag" appearing inside a nested tool_use input that
    // happens to be JSON-serialized into a message.
    const tailLines = tail.split('\n')
    if (!skipTitleRefresh) {
      const titleLine = tailLines.findLast(l =>
        l.startsWith('{"type":"custom-title"'),
      )
      if (titleLine) {
        const tailTitle = extractLastJsonStringField(titleLine, 'customTitle')
        // `!== undefined` distinguishes no-match from empty-string match.
        // renameSession rejects empty titles, but the CLI is defensive: an
        // external writer with customTitle:"" should clear the cache so the
        // re-append below skips it (instead of resurrecting a stale title).
        if (tailTitle !== undefined) {
          this.currentSessionTitle = tailTitle || undefined
        }
      }
    }
    const tagLine = tailLines.findLast(l => l.startsWith('{"type":"tag"'))
    if (tagLine) {
      const tailTag = extractLastJsonStringField(tagLine, 'tag')
      // Same: tagSession(id, null) writes `tag:""` to clear.
      if (tailTag !== undefined) {
        this.currentSessionTag = tailTag || undefined
      }
    }

    // lastPrompt is re-appended so readLiteMetadata can show what the
    // user was most recently doing. Written first so customTitle/tag/etc
    // land closer to EOF (they're the more critical fields for tail reads).
    if (this.currentSessionLastPrompt) {
      appendEntryToFile(this.sessionFile, {
        type: 'last-prompt',
        lastPrompt: this.currentSessionLastPrompt,
        sessionId,
      })
    }
    // Unconditional: cache was refreshed from tail above; re-append keeps
    // the entry at EOF so compaction-pushed content doesn't evict it.
    if (this.currentSessionTitle) {
      appendEntryToFile(this.sessionFile, {
        type: 'custom-title',
        customTitle: this.currentSessionTitle,
        sessionId,
      })
    }
    if (this.currentSessionTag) {
      appendEntryToFile(this.sessionFile, {
        type: 'tag',
        tag: this.currentSessionTag,
        sessionId,
      })
    }
    if (this.currentSessionAgentName) {
      appendEntryToFile(this.sessionFile, {
        type: 'agent-name',
        agentName: this.currentSessionAgentName,
        sessionId,
      })
    }
    if (this.currentSessionAgentColor) {
      appendEntryToFile(this.sessionFile, {
        type: 'agent-color',
        agentColor: this.currentSessionAgentColor,
        sessionId,
      })
    }
    if (this.currentSessionAgentSetting) {
      appendEntryToFile(this.sessionFile, {
        type: 'agent-setting',
        agentSetting: this.currentSessionAgentSetting,
        sessionId,
      })
    }
    if (this.currentSessionMode) {
      appendEntryToFile(this.sessionFile, {
        type: 'mode',
        mode: this.currentSessionMode,
        sessionId,
      })
    }
    if (this.currentSessionWorktree !== undefined) {
      appendEntryToFile(this.sessionFile, {
        type: 'worktree-state',
        worktreeSession: this.currentSessionWorktree,
        sessionId,
      })
    }
    if (
      this.currentSessionPrNumber !== undefined &&
      this.currentSessionPrUrl &&
      this.currentSessionPrRepository
    ) {
      appendEntryToFile(this.sessionFile, {
        type: 'pr-link',
        sessionId,
        prNumber: this.currentSessionPrNumber,
        prUrl: this.currentSessionPrUrl,
        prRepository: this.currentSessionPrRepository,
        timestamp: new Date().toISOString(),
      })
    }
  }

  async flush(): Promise<void> {
    // Cancel pending timer
    if (this.flushTimer) {
      clearTimeout(this.flushTimer)
      this.flushTimer = null
    }
    // Wait for any in-flight drain to finish
    if (this.activeDrain) {
      await this.activeDrain
    }
    // Drain anything remaining in the queues
    await this.drainWriteQueue()

    // Wait for non-queue tracked operations (e.g. removeMessageByUuid)
    if (this.pendingWriteCount === 0) {
      return
    }
    return new Promise<void>(resolve => {
      this.flushResolvers.push(resolve)
    })
  }

  /**
   * Remove a message from the transcript by UUID.
   * Used for tombstoning orphaned messages from failed streaming attempts.
   *
   * The target is almost always the most recently appended entry, so we
   * read only the tail, locate the line, and splice it out with a
   * positional write + truncate instead of rewriting the whole file.
   */
  async removeMessageByUuid(targetUuid: UUID): Promise<void> {
    return this.trackWrite(async () => {
      const sessionFile = this.sessionFile
      if (sessionFile === null) return

      await this.enqueueMessageRemoval(sessionFile, targetUuid)

      // The memoized UUID set is the in-memory authority used by the next
      // recordTranscript() dedup pass. Keeping a removed UUID here would make a
      // retry that intentionally reuses it disappear from disk as well.
      try {
        const messageSet = await getSessionMessages(getSessionId() as UUID)
        messageSet.delete(targetUuid)
      } catch {
        // Disk removal is fail-soft; cache maintenance must be as well. Drop a
        // rejected/stale memoized read so the next recording gets a fresh view.
        clearSessionMessagesCache()
      }
    })
  }

  private async removeMessageFromFile(
    filePath: string,
    targetUuid: UUID,
  ): Promise<void> {
    try {
      let fileSize = 0
      const fh = await fsOpen(filePath, 'r+')
      try {
        const { size } = await fh.stat()
        fileSize = size
        if (size === 0) return

        const chunkLen = Math.min(size, LITE_READ_BUF_SIZE)
        const tailStart = size - chunkLen
        const buf = Buffer.allocUnsafe(chunkLen)
        const { bytesRead } = await fh.read(buf, 0, chunkLen, tailStart)
        const tail = buf.subarray(0, bytesRead)

        // Entries are serialized via JSON.stringify (no key-value
        // whitespace). Search for the full `"uuid":"..."` pattern, not
        // just the bare UUID, so we do not match the same value sitting
        // in `parentUuid` of a child entry. UUIDs are pure ASCII so a
        // byte-level search is correct.
        const needle = `"uuid":"${targetUuid}"`
        const matchIdx = tail.lastIndexOf(needle)

        if (matchIdx >= 0) {
          // 0x0a never appears inside a UTF-8 multi-byte sequence, so
          // byte-scanning for line boundaries is safe even if the chunk
          // starts mid-character.
          const prevNl = tail.lastIndexOf(0x0a, matchIdx)
          // If the preceding newline is outside our chunk and we did not
          // read from the start of the file, the line is longer than the
          // window - fall through to the slow path.
          if (prevNl >= 0 || tailStart === 0) {
            const lineStart = prevNl + 1 // 0 when prevNl === -1
            const nextNl = tail.indexOf(0x0a, matchIdx + needle.length)
            const lineEnd = nextNl >= 0 ? nextNl + 1 : bytesRead

            const absLineStart = tailStart + lineStart
            const afterLen = bytesRead - lineEnd
            // Truncate first, then re-append the trailing lines. In the
            // common case (target is the last entry) afterLen is 0 and
            // this is a single ftruncate.
            await fh.truncate(absLineStart)
            if (afterLen > 0) {
              await fh.write(tail, lineEnd, afterLen, absLineStart)
            }
            return
          }
        }
      } finally {
        await fh.close()
      }

      // Slow path: target was not in the last 64KB. Rare - requires many
      // large entries to have landed between the write and the tombstone.
      if (fileSize > MAX_TOMBSTONE_REWRITE_BYTES) {
        logForDebugging(
          `Skipping tombstone removal: session file too large (${formatFileSize(fileSize)})`,
          { level: 'warn' },
        )
        return
      }
      const content = await readFile(filePath, { encoding: 'utf-8' })
      const lines = content.split('\n').filter((line: string) => {
        if (!line.trim()) return true
        try {
          const entry = jsonParse(line)
          return entry.uuid !== targetUuid
        } catch {
          return true // Keep malformed lines
        }
      })
      await writeFile(filePath, lines.join('\n'), { encoding: 'utf8' })
    } catch {
      // Silently ignore errors - the file might not exist yet
    }
  }

  /**
   * True when test env / cleanupPeriodDays=0 / --no-session-persistence /
   * CRABCODE_SKIP_PROMPT_HISTORY should suppress all transcript writes.
   * Shared guard for appendEntry and materializeSessionFile so both skip
   * consistently. The env var is set by tmuxSocket.ts so Tungsten-spawned
   * test sessions don't pollute the user's --resume list.
   */
  private shouldSkipPersistence(): boolean {
    const allowTestPersistence = isEnvTruthy(
      process.env.TEST_ENABLE_SESSION_PERSISTENCE,
    )
    return (
      (getNodeEnv() === 'test' && !allowTestPersistence) ||
      getSettings_DEPRECATED()?.cleanupPeriodDays === 0 ||
      isSessionPersistenceDisabled() ||
      isEnvTruthy(process.env.CRABCODE_SKIP_PROMPT_HISTORY)
    )
  }

  /**
   * Create the session file, write cached startup metadata, and flush
   * buffered entries. Called on the first user/assistant message.
   */
  private async materializeSessionFile(): Promise<void> {
    // Guard here too — reAppendSessionMetadata writes via appendEntryToFile
    // (not appendEntry) so it would bypass the per-entry persistence check
    // and create a metadata-only file despite --no-session-persistence.
    if (this.shouldSkipPersistence()) return
    this.ensureCurrentSessionFile()
    // mode/agentSetting are cache-only pre-materialization; write them now.
    this.reAppendSessionMetadata()
    if (this.pendingEntries.length > 0) {
      const buffered = this.pendingEntries
      this.pendingEntries = []
      for (const entry of buffered) {
        await this.appendEntry(entry)
      }
    }
  }

  async insertMessageChain(
    messages: Transcript,
    isSidechain: boolean = false,
    agentId?: string,
    startingParentUuid?: UUID | null,
    teamInfo?: { teamName?: string; agentName?: string },
  ) {
    return this.trackWrite(async () => {
      let parentUuid: UUID | null = startingParentUuid ?? null

      // First user/assistant message materializes the session file.
      // Hook progress/attachment messages alone stay buffered.
      if (
        this.sessionFile === null &&
        messages.some(m => m.type === 'user' || m.type === 'assistant')
      ) {
        await this.materializeSessionFile()
      }

      // Get current git branch once for this message chain
      let gitBranch: string | undefined
      try {
        gitBranch = await getBranch()
      } catch {
        // Not in a git repo or git command failed
        gitBranch = undefined
      }

      // Get slug if one exists for this session (used for plan files, etc.)
      const sessionId = getSessionId()
      const slug = getPlanSlugCache().get(sessionId)

      for (const message of messages) {
        const isCompactBoundary = isCompactBoundaryMessage(message)

        // For tool_result messages, use the assistant message UUID from the message
        // if available (set at creation time), otherwise fall back to sequential parent
        let effectiveParentUuid = parentUuid
        if (
          message.type === 'user' &&
          'sourceToolAssistantUUID' in message &&
          message.sourceToolAssistantUUID
        ) {
          effectiveParentUuid = message.sourceToolAssistantUUID
        }

        const transcriptMessage = {
          parentUuid: isCompactBoundary ? null : effectiveParentUuid,
          logicalParentUuid: isCompactBoundary ? parentUuid : undefined,
          isSidechain,
          teamName: teamInfo?.teamName,
          agentName: teamInfo?.agentName,
          promptId:
            message.type === 'user' ? (getPromptId() ?? undefined) : undefined,
          agentId,
          ...message,
          // Session-stamp fields MUST come after the spread. On --fork-session
          // and --resume, messages arrive as SerializedMessage (carries source
          // sessionId/cwd/etc. because removeExtraFields only strips parentUuid
          // and isSidechain). If sessionId isn't re-stamped, FRESH.jsonl ends up
          // with messages stamped sessionId=A but content-replacement entries
          // stamped sessionId=FRESH (from insertContentReplacement), and
          // loadFullLog's sessionId-keyed contentReplacements lookup misses →
          // replacement records lost → FROZEN misclassification.
          userType: getUserType(),
          entrypoint: getEntrypoint(),
          cwd: getCwd(),
          sessionId,
          version: VERSION,
          gitBranch,
          slug,
        }
        await this.appendEntry(transcriptMessage as TranscriptMessage)
        if (isChainParticipant(message)) {
          parentUuid = message.uuid
        }
      }

      // Cache this turn's user prompt for reAppendSessionMetadata —
      // the --resume picker shows what the user was last doing.
      // Overwritten every turn by design.
      if (!isSidechain) {
        const text = getFirstMeaningfulUserMessageTextContent(messages)
        if (text) {
          const flat = text.replace(/\n/g, ' ').trim()
          this.currentSessionLastPrompt =
            flat.length > 200 ? flat.slice(0, 200).trim() + '…' : flat
        }
      }
    })
  }

  async insertFileHistorySnapshot(
    messageId: UUID,
    snapshot: FileHistorySnapshot,
    isSnapshotUpdate: boolean,
  ) {
    return this.trackWrite(async () => {
      const fileHistoryMessage = {
        type: 'file-history-snapshot' as const,
        messageId,
        snapshot,
        isSnapshotUpdate,
      }
      await this.appendEntry(fileHistoryMessage)
    })
  }

  async insertQueueOperation(queueOp: QueueOperationMessage) {
    return this.trackWrite(async () => {
      await this.appendEntry(queueOp)
    })
  }

  async insertAttributionSnapshot(snapshot: AttributionSnapshotMessage) {
    return this.trackWrite(async () => {
      await this.appendEntry(snapshot)
    })
  }

  async insertContentReplacement(
    replacements: ContentReplacementRecord[],
    agentId?: AgentId,
  ) {
    return this.trackWrite(async () => {
      const entry: ContentReplacementEntry = {
        type: 'content-replacement',
        sessionId: getSessionId() as UUID,
        agentId,
        replacements,
      }
      await this.appendEntry(entry)
    })
  }

  async appendEntry(entry: Entry, sessionId: UUID = getSessionId() as UUID) {
    if (this.shouldSkipPersistence()) {
      return
    }

    const currentSessionId = getSessionId() as UUID
    const isCurrentSession = sessionId === currentSessionId

    let sessionFile: string
    if (isCurrentSession) {
      // Buffer until materializeSessionFile runs (first user/assistant message).
      if (this.sessionFile === null) {
        this.pendingEntries.push(entry)
        return
      }
      sessionFile = this.sessionFile
    } else {
      const existing = await this.getExistingSessionFile(sessionId)
      if (!existing) {
        logError(
          new Error(
            `appendEntry: session file not found for other session ${sessionId}`,
          ),
        )
        return
      }
      sessionFile = existing
    }

    // Only load current session messages if needed
    if (entry.type === 'summary') {
      // Summaries can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'custom-title') {
      // Custom titles can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'ai-title') {
      // AI titles can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'last-prompt') {
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'task-summary') {
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'tag') {
      // Tags can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'agent-name') {
      // Agent names can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'agent-color') {
      // Agent colors can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'agent-setting') {
      // Agent settings can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'pr-link') {
      // PR links can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'file-history-snapshot') {
      // File history snapshots can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'attribution-snapshot') {
      // Attribution snapshots can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'speculation-accept') {
      // Speculation accept entries can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'mode') {
      // Mode entries can always be appended
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'worktree-state') {
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'content-replacement') {
      // Content replacement records can always be appended. Subagent records
      // go to the sidechain file (for AgentTool resume); main-thread
      // records go to the session file (for /resume).
      const targetFile = entry.agentId
        ? getAgentTranscriptPath(entry.agentId)
        : sessionFile
      void this.enqueueWrite(targetFile, entry)
    } else if (entry.type === 'marble-origami-commit') {
      // Always append. Commit order matters for restore (later commits may
      // reference earlier commits' summary messages), so these must be
      // written in the order received and read back sequentially.
      void this.enqueueWrite(sessionFile, entry)
    } else if (entry.type === 'marble-origami-snapshot') {
      // Always append. Last-wins on restore — later entries supersede.
      void this.enqueueWrite(sessionFile, entry)
    } else {
      const messageSet = await getSessionMessages(sessionId)
      if (entry.type === 'queue-operation') {
        // Queue operations are always appended to the session file
        void this.enqueueWrite(sessionFile, entry)
      } else {
        // At this point, entry must be a TranscriptMessage (user/assistant/attachment/system)
        // All other entry types have been handled above
        const isAgentSidechain =
          entry.isSidechain && entry.agentId !== undefined
        const targetFile = isAgentSidechain
          ? getAgentTranscriptPath(asAgentId(entry.agentId!))
          : sessionFile

        // For message entries, check if UUID already exists in current session.
        // Skip dedup for agent sidechain LOCAL writes — they go to a separate
        // file, and fork-inherited parent messages share UUIDs with the main
        // session transcript. Deduping against the main session's set would
        // drop them, leaving the persisted sidechain transcript incomplete
        // (resume-of-fork loads a 10KB file instead of the full 85KB inherited
        // context).
        //
        const isNewUuid = !entry.uuid || !messageSet.has(entry.uuid as import('crypto').UUID)
        if (isAgentSidechain || isNewUuid) {
          // Enqueue write — appendToFile handles ENOENT by creating directories
          void this.enqueueWrite(targetFile, entry)

          if (!isAgentSidechain) {
            // messageSet is main-file-authoritative. Sidechain entries go to a
            // separate agent file — adding their UUIDs here causes recordTranscript
            // to skip them on the main thread (line ~1270), so the message is never
            // written to the main session file. The next main-thread message then
            // chains its parentUuid to a UUID that only exists in the agent file,
            // and --resume's buildConversationChain terminates at the dangling ref.
            messageSet.add(entry.uuid as import('crypto').UUID)
          }
        }
      }
    }
  }

  /**
   * Loads the sessionFile variable.
   * Do not need to create session files until they are written to.
   */
  private ensureCurrentSessionFile(): string {
    if (this.sessionFile === null) {
      this.sessionFile = getTranscriptPath()
    }

    return this.sessionFile
  }

  /**
   * Returns the session file path if it exists, null otherwise.
   * Used for writing to sessions other than the current one.
   * Caches positive results so we only stat once per session.
   */
  private existingSessionFiles = new Map<string, string>()
  private async getExistingSessionFile(
    sessionId: UUID,
  ): Promise<string | null> {
    const cached = this.existingSessionFiles.get(sessionId)
    if (cached) return cached

    const targetFile = getTranscriptPathForSession(sessionId)
    try {
      await stat(targetFile)
      this.existingSessionFiles.set(sessionId, targetFile)
      return targetFile
    } catch (e) {
      if (isFsInaccessible(e)) return null
      throw e
    }
  }

}

// ─── Record functions (public API wrapping Project) ─────────────────────────

// Filter out already-recorded messages before passing to insertMessageChain.
// Without this, after compaction messagesToKeep (same UUIDs as pre-compact
// messages) are dedup-skipped by appendEntry but still advance the parentUuid
// cursor in insertMessageChain, causing new messages to chain from pre-compact
// UUIDs instead of the post-compact summary — orphaning the compact boundary.
//
// `startingParentUuidHint`: used by useLogMessages to pass the parent from
// the previous incremental slice, avoiding an O(n) scan to rediscover it.
//
// Skip-tracking: already-recorded messages are tracked as the parent ONLY if
// they form a PREFIX (appear before any new message). This handles both cases:
//  - Growing-array callers (QueryEngine, queryHelpers, LocalMainSessionTask,
//    trajectory): recorded messages are always a prefix → tracked → correct
//    parent chain for new messages.
//  - Compaction (useLogMessages): new CB/summary appear FIRST, then recorded
//    messagesToKeep → not a prefix → not tracked → CB gets parentUuid=null
//    (correct: truncates --continue chain at compact boundary).
export function recordTranscript(
  messages: Message[],
  teamInfo?: TeamInfo,
  startingParentUuidHint?: UUID,
  allMessages?: readonly Message[],
): Promise<UUID | null> {
  return enqueueTranscriptMutation(() =>
    recordTranscriptImpl(
      messages,
      teamInfo,
      startingParentUuidHint,
      allMessages,
    ),
  )
}

async function recordTranscriptImpl(
  messages: Message[],
  teamInfo?: TeamInfo,
  startingParentUuidHint?: UUID,
  allMessages?: readonly Message[],
): Promise<UUID | null> {
  // W-SESSION-TRANSCRIPT-CORRUPTION (2026-05-31): detect (from the RAW input,
  // before cleanMessagesForLogging strips them) whether the worker seeded any
  // reconstructed-history frames. These are excluded from `cleanedMessages` by
  // isLoggableMessage so they are never persisted; the flag gates the on-disk
  // tail re-anchor below so every normal caller stays byte-identical.
  const hasReconstructed = messages.some(
    m => (m as { isReconstructedHistory?: boolean }).isReconstructedHistory === true,
  )
  const cleanedMessages = cleanMessagesForLogging(messages, allMessages)
  const sessionId = getSessionId() as UUID
  const messageSet = await getSessionMessages(sessionId)
  const newMessages: typeof cleanedMessages = []
  let startingParentUuid: UUID | undefined = startingParentUuidHint
  let seenNewMessage = false
  for (const m of cleanedMessages) {
    if (messageSet.has(m.uuid as UUID)) {
      // Only track skipped messages that form a prefix. After compaction,
      // messagesToKeep appear AFTER new CB/summary, so this skips them.
      if (!seenNewMessage && isChainParticipant(m)) {
        startingParentUuid = m.uuid as UUID
      }
    } else {
      newMessages.push(m)
      seenNewMessage = true
    }
  }
  // W-SESSION-TRANSCRIPT-CORRUPTION: in the cross-process resume case the
  // excluded reconstructed backbone leaves the new live frames with no in-array
  // parent (startingParentUuid stays null), which would orphan them and make
  // --resume's buildConversationChain truncate at the gap. Re-anchor to the
  // real on-disk tail. Gated so it is provably inert for every other caller:
  //   - `hasReconstructed`: only the worker reconstruction path sets the marker;
  //     all TUI/print/compaction callers are byte-identical.
  //   - `!newMessages.some(isCompactBoundaryMessage)`: a compaction that runs
  //     INSIDE a resumed worker still carries the marked frames at the head of
  //     mutableMessages (so `hasReconstructed` stays true for the process), but
  //     the compact boundary deliberately wants parentUuid=null to truncate
  //     --continue. Never re-anchor a write that carries a boundary.
  //   - `startingParentUuid == null && messageSet.size > 0`: only when the new
  //     frames are genuinely orphaned AND there is real on-disk history to
  //     anchor to.
  if (
    hasReconstructed &&
    newMessages.length > 0 &&
    startingParentUuid == null &&
    messageSet.size > 0 &&
    !newMessages.some(m => isCompactBoundaryMessage(m))
  ) {
    startingParentUuid =
      (await getPersistedTranscriptTailUuid(sessionId)) ?? startingParentUuid
  }
  if (newMessages.length > 0) {
    await getProject().insertMessageChain(
      newMessages,
      false,
      undefined,
      startingParentUuid,
      teamInfo,
    )
  }
  // Return the last ACTUALLY recorded chain-participant's UUID, OR the
  // prefix-tracked UUID if no new chain participants were recorded. This lets
  // callers (useLogMessages) maintain the correct parent chain even when the
  // slice is all-recorded (rewind, /resume scenarios where every message is
  // already in messageSet). Progress is skipped — it's written to the JSONL
  // but nothing chains TO it (see isChainParticipant).
  const lastRecorded = newMessages.findLast(isChainParticipant)
  return (lastRecorded?.uuid as UUID | undefined) ?? startingParentUuid ?? null
}

export async function recordSidechainTranscript(
  messages: Message[],
  agentId?: string,
  startingParentUuid?: UUID | null,
) {
  await getProject().insertMessageChain(
    cleanMessagesForLogging(messages),
    true,
    agentId,
    startingParentUuid,
  )
}

export async function recordQueueOperation(queueOp: QueueOperationMessage) {
  await getProject().insertQueueOperation(queueOp)
}

/**
 * Remove a message from the transcript by UUID.
 * Used when a tombstone is received for an orphaned message.
 */
export async function removeTranscriptMessage(targetUuid: UUID): Promise<void> {
  await enqueueTranscriptMutation(() =>
    getProject().removeMessageByUuid(targetUuid),
  )
}

export async function recordFileHistorySnapshot(
  messageId: UUID,
  snapshot: FileHistorySnapshot,
  isSnapshotUpdate: boolean,
) {
  await getProject().insertFileHistorySnapshot(
    messageId,
    snapshot,
    isSnapshotUpdate,
  )
}

export async function recordAttributionSnapshot(
  snapshot: AttributionSnapshotMessage,
) {
  await getProject().insertAttributionSnapshot(snapshot)
}

export async function recordContentReplacement(
  replacements: ContentReplacementRecord[],
  agentId?: AgentId,
) {
  await getProject().insertContentReplacement(replacements, agentId)
}

/**
 * Reset the session file pointer after switchSession/regenerateSessionId.
 * The new file is created lazily on the first user/assistant message.
 */
export async function resetSessionFilePointer() {
  getProject().resetSessionFile()
}

/**
 * Adopt the existing session file after --continue/--resume (non-fork).
 * Call after switchSession + resetSessionFilePointer + restoreSessionMetadata:
 * getTranscriptPath() now derives the resumed file's path from the switched
 * sessionId, and the cache holds the final metadata (--name title, resumed
 * mode/tag/agent).
 *
 * Setting sessionFile here — instead of waiting for materializeSessionFile
 * on the first user message — lets the exit cleanup handler's
 * reAppendSessionMetadata run (it bails when sessionFile is null). Without
 * this, `-c -n foo` + quit-before-message drops the title on the floor:
 * the in-memory cache is correct but never written. The resumed file
 * already exists on disk (we loaded from it), so this can't create an
 * orphan the way a fresh --name session would.
 *
 * skipTitleRefresh: restoreSessionMetadata populated the cache from the
 * same disk read microseconds ago, so refreshing from the tail here is a
 * no-op — unless --name was used, in which case it would clobber the fresh
 * CLI title with the stale disk value. After this write, disk == cache and
 * later calls (compaction, exit cleanup) absorb SDK writes normally.
 */
export function adoptResumedSessionFile(): void {
  const project = getProject()
  project.sessionFile = getTranscriptPath()
  project.reAppendSessionMetadata(true)
}

/**
 * Append a context-collapse commit entry to the transcript. One entry per
 * commit, in commit order. On resume these are collected into an ordered
 * array and handed to restoreFromEntries() which rebuilds the commit log.
 */
export async function recordContextCollapseCommit(commit: {
  collapseId: string
  summaryUuid: string
  summaryContent: string
  summary: string
  firstArchivedUuid: string
  lastArchivedUuid: string
}): Promise<void> {
  const sessionId = getSessionId() as UUID
  if (!sessionId) return
  await getProject().appendEntry({
    type: 'marble-origami-commit',
    sessionId,
    ...commit,
  })
}

/**
 * Snapshot the staged queue + spawn state. Written after each ctx-agent
 * spawn resolves (when staged contents may have changed). Last-wins on
 * restore — the loader keeps only the most recent snapshot entry.
 */
export async function recordContextCollapseSnapshot(snapshot: {
  staged: Array<{
    startUuid: string
    endUuid: string
    summary: string
    risk: number
    stagedAt: number
  }>
  armed: boolean
  lastSpawnTokens: number
}): Promise<void> {
  const sessionId = getSessionId() as UUID
  if (!sessionId) return
  await getProject().appendEntry({
    type: 'marble-origami-snapshot',
    sessionId,
    ...snapshot,
  })
}

export async function flushSessionStorage(): Promise<void> {
  await getProject().flush()
}

// ─── Session metadata save/restore ──────────────────────────────────────────

export async function saveCustomTitle(
  sessionId: UUID,
  customTitle: string,
  fullPath?: string,
  source: 'user' | 'auto' = 'user',
) {
  // Fall back to computed path if fullPath is not provided
  const resolvedPath = fullPath ?? getTranscriptPathForSession(sessionId)
  appendEntryToFile(resolvedPath, {
    type: 'custom-title',
    customTitle,
    sessionId,
  })
  // Cache for current session only (for immediate visibility)
  if (sessionId === getSessionId()) {
    getProject().currentSessionTitle = customTitle
  }
  logEvent('tengu_session_renamed', {
    source:
      source as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })
}

/**
 * Persist an AI-generated title to the JSONL as a distinct `ai-title` entry.
 *
 * Writing a separate entry type (vs. reusing `custom-title`) is load-bearing:
 * - Read preference: readers prefer `customTitle` field over `aiTitle`, so
 *   a user rename always wins regardless of append order.
 * - Resume safety: `loadTranscriptFile` only populates the `customTitles`
 *   Map from `custom-title` entries, so `restoreSessionMetadata` never
 *   caches an AI title and `reAppendSessionMetadata` never re-appends one
 *   at EOF — avoiding the clobber-on-resume bug where a stale AI title
 *   overwrites a mid-session user rename.
 * - CAS semantics: VS Code's `onlyIfNoCustomTitle` check scans for the
 *   `customTitle` field only, so AI can overwrite its own previous AI
 *   title but never a user title.
 * - Metrics: `tengu_session_renamed` is not fired for AI titles.
 *
 * Because the entry is never re-appended, it scrolls out of the 64KB tail
 * window once enough messages accumulate. Readers (`readLiteMetadata`,
 * `listSessionsImpl`, VS Code `fetchSessions`) fall back to scanning the
 * head buffer for `aiTitle` in that case. Both head and tail reads are
 * bounded (64KB each via `extractLastJsonStringField`), never a full scan.
 *
 * Callers with a stale-write guard (e.g., VS Code client) should prefer
 * passing `persist: false` to the SDK control request and persisting
 * through their own rename path after the guard passes, to avoid a race
 * where the AI title lands after a mid-flight user rename.
 */
export function saveAiGeneratedTitle(sessionId: UUID, aiTitle: string): void {
  appendEntryToFile(getTranscriptPathForSession(sessionId), {
    type: 'ai-title',
    aiTitle,
    sessionId,
  })
}

/**
 * Append a periodic task summary for `crabcode ps`. Unlike ai-title this is
 * not re-appended by reAppendSessionMetadata — it's a rolling snapshot of
 * what the agent is doing *now*, so staleness is fine; ps reads the most
 * recent one from the tail.
 */
export function saveTaskSummary(sessionId: UUID, summary: string): void {
  appendEntryToFile(getTranscriptPathForSession(sessionId), {
    type: 'task-summary',
    summary,
    sessionId,
    timestamp: new Date().toISOString(),
  })
}

export async function saveTag(sessionId: UUID, tag: string, fullPath?: string) {
  // Fall back to computed path if fullPath is not provided
  const resolvedPath = fullPath ?? getTranscriptPathForSession(sessionId)
  appendEntryToFile(resolvedPath, { type: 'tag', tag, sessionId })
  // Cache for current session only (for immediate visibility)
  if (sessionId === getSessionId()) {
    getProject().currentSessionTag = tag
  }
  logEvent('tengu_session_tagged', {})
}

/**
 * Link a session to a GitHub pull request.
 * This stores the PR number, URL, and repository for tracking and navigation.
 */
export async function linkSessionToPR(
  sessionId: UUID,
  prNumber: number,
  prUrl: string,
  prRepository: string,
  fullPath?: string,
): Promise<void> {
  const resolvedPath = fullPath ?? getTranscriptPathForSession(sessionId)
  appendEntryToFile(resolvedPath, {
    type: 'pr-link',
    sessionId,
    prNumber,
    prUrl,
    prRepository,
    timestamp: new Date().toISOString(),
  })
  // Cache for current session so reAppendSessionMetadata can re-write after compaction
  if (sessionId === getSessionId()) {
    const project = getProject()
    project.currentSessionPrNumber = prNumber
    project.currentSessionPrUrl = prUrl
    project.currentSessionPrRepository = prRepository
  }
  logEvent('tengu_session_linked_to_pr', { prNumber })
}

export function getCurrentSessionTag(sessionId: UUID): string | undefined {
  // Only returns tag for current session (the only one we cache)
  if (sessionId === getSessionId()) {
    return getProject().currentSessionTag
  }
  return undefined
}

export function getCurrentSessionTitle(
  sessionId: SessionId,
): string | undefined {
  // Only returns title for current session (the only one we cache)
  if (sessionId === getSessionId()) {
    return getProject().currentSessionTitle
  }
  return undefined
}

export function getCurrentSessionAgentColor(): string | undefined {
  return getProject().currentSessionAgentColor
}

/**
 * Restore session metadata into in-memory cache on resume.
 * Populates the cache so metadata is available for display (e.g. the
 * agent banner) and re-appended on session exit via reAppendSessionMetadata.
 */
export function restoreSessionMetadata(meta: {
  customTitle?: string
  tag?: string
  agentName?: string
  agentColor?: string
  agentSetting?: string
  mode?: 'coordinator' | 'normal'
  worktreeSession?: PersistedWorktreeSession | null
  prNumber?: number
  prUrl?: string
  prRepository?: string
}): void {
  const project = getProject()
  // ??= so --name (cacheSessionTitle) wins over the resumed
  // session's title. REPL.tsx clears before calling, so /resume is unaffected.
  if (meta.customTitle) project.currentSessionTitle ??= meta.customTitle
  if (meta.tag !== undefined) project.currentSessionTag = meta.tag || undefined
  if (meta.agentName) project.currentSessionAgentName = meta.agentName
  if (meta.agentColor) project.currentSessionAgentColor = meta.agentColor
  if (meta.agentSetting) project.currentSessionAgentSetting = meta.agentSetting
  if (meta.mode) project.currentSessionMode = meta.mode
  if (meta.worktreeSession !== undefined)
    project.currentSessionWorktree = meta.worktreeSession
  if (meta.prNumber !== undefined)
    project.currentSessionPrNumber = meta.prNumber
  if (meta.prUrl) project.currentSessionPrUrl = meta.prUrl
  if (meta.prRepository) project.currentSessionPrRepository = meta.prRepository
}

/**
 * Clear all cached session metadata (title, tag, agent name/color).
 * Called when /clear creates a new session so stale metadata
 * from the previous session does not leak into the new one.
 */
export function clearSessionMetadata(): void {
  const project = getProject()
  project.currentSessionTitle = undefined
  project.currentSessionTag = undefined
  project.currentSessionAgentName = undefined
  project.currentSessionAgentColor = undefined
  project.currentSessionLastPrompt = undefined
  project.currentSessionAgentSetting = undefined
  project.currentSessionMode = undefined
  project.currentSessionWorktree = undefined
  project.currentSessionPrNumber = undefined
  project.currentSessionPrUrl = undefined
  project.currentSessionPrRepository = undefined
}

/**
 * Re-append cached session metadata (custom title, tag) to the end of the
 * transcript file. Call this after compaction so the metadata stays within
 * the 16KB tail window that readLiteMetadata reads during progressive loading.
 * Without this, enough post-compaction messages can push the metadata entry
 * out of the window, causing `--resume` to show the auto-generated firstPrompt
 * instead of the user-set session name.
 */
export function reAppendSessionMetadata(): void {
  getProject().reAppendSessionMetadata()
}

export async function saveAgentName(
  sessionId: UUID,
  agentName: string,
  fullPath?: string,
  source: 'user' | 'auto' = 'user',
) {
  const resolvedPath = fullPath ?? getTranscriptPathForSession(sessionId)
  appendEntryToFile(resolvedPath, { type: 'agent-name', agentName, sessionId })
  // Cache for current session only (for immediate visibility)
  if (sessionId === getSessionId()) {
    getProject().currentSessionAgentName = agentName
    void updateSessionName(agentName)
  }
  logEvent('tengu_agent_name_set', {
    source:
      source as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })
}

export async function saveAgentColor(
  sessionId: UUID,
  agentColor: string,
  fullPath?: string,
) {
  const resolvedPath = fullPath ?? getTranscriptPathForSession(sessionId)
  appendEntryToFile(resolvedPath, {
    type: 'agent-color',
    agentColor,
    sessionId,
  })
  // Cache for current session only (for immediate visibility)
  if (sessionId === getSessionId()) {
    getProject().currentSessionAgentColor = agentColor
  }
  logEvent('tengu_agent_color_set', {})
}

/**
 * Cache the session agent setting. Written to disk by materializeSessionFile
 * on the first user message, and re-stamped by reAppendSessionMetadata on exit.
 * Cache-only here to avoid creating metadata-only session files at startup.
 */
export function saveAgentSetting(agentSetting: string): void {
  getProject().currentSessionAgentSetting = agentSetting
}

/**
 * Cache a session title set at startup (--name). Written to disk by
 * materializeSessionFile on the first user message. Cache-only here so no
 * orphan metadata-only file is created before the session ID is finalized.
 */
export function cacheSessionTitle(customTitle: string): void {
  getProject().currentSessionTitle = customTitle
}

/**
 * Cache the session mode. Written to disk by materializeSessionFile on the
 * first user message, and re-stamped by reAppendSessionMetadata on exit.
 * Cache-only here to avoid creating metadata-only session files at startup.
 */
export function saveMode(mode: 'coordinator' | 'normal'): void {
  getProject().currentSessionMode = mode
}

/**
 * Record the session's worktree state for --resume. Written to disk by
 * materializeSessionFile on the first user message and re-stamped by
 * reAppendSessionMetadata on exit. Pass null when exiting a worktree
 * so --resume knows not to cd back into it.
 */
export function saveWorktreeState(
  worktreeSession: PersistedWorktreeSession | null,
): void {
  // Strip ephemeral fields (creationDurationMs, usedSparsePaths) that callers
  // may pass via full WorktreeSession objects — TypeScript structural typing
  // allows this, but we don't want them serialized to the transcript.
  const stripped: PersistedWorktreeSession | null = worktreeSession
    ? {
        originalCwd: worktreeSession.originalCwd,
        worktreePath: worktreeSession.worktreePath,
        worktreeName: worktreeSession.worktreeName,
        worktreeBranch: worktreeSession.worktreeBranch,
        originalBranch: worktreeSession.originalBranch,
        originalHeadCommit: worktreeSession.originalHeadCommit,
        sessionId: worktreeSession.sessionId,
        tmuxSessionName: worktreeSession.tmuxSessionName,
        hookBased: worktreeSession.hookBased,
      }
    : null
  const project = getProject()
  project.currentSessionWorktree = stripped
  // Write eagerly when the file already exists (mid-session enter/exit).
  // For --worktree startup, sessionFile is null — materializeSessionFile
  // will write it on the first message via reAppendSessionMetadata.
  if (project.sessionFile) {
    appendEntryToFile(project.sessionFile, {
      type: 'worktree-state',
      worktreeSession: stripped,
      sessionId: getSessionId(),
    })
  }
}

// ─── Session ID helpers ─────────────────────────────────────────────────────

/**
 * Extracts the session ID from a log.
 * For lite logs, uses the sessionId field directly.
 * For full logs, extracts from the first message.
 */
export function getSessionIdFromLog(log: LogOption): UUID | undefined {
  // For lite logs, use the direct sessionId field
  if (log.sessionId) {
    return log.sessionId as UUID
  }
  // Fall back to extracting from first message (full logs)
  return log.messages[0]?.sessionId as UUID | undefined
}

/**
 * Checks if a log is a lite log that needs full loading.
 * Lite logs have messages: [] and sessionId set.
 */
export function isLiteLog(log: LogOption): boolean {
  return log.messages.length === 0 && log.sessionId !== undefined
}

// ─── Internal helpers ───────────────────────────────────────────────────────

/**
 * Append an entry to a session file. Creates the parent dir if missing.
 */
/* eslint-disable custom-rules/no-sync-fs -- sync callers (exit cleanup, materialize) */
export function appendEntryToFile(
  fullPath: string,
  entry: Record<string, unknown>,
): void {
  const fs = getFsImplementation()
  const line = jsonStringify(entry) + '\n'
  try {
    fs.appendFileSync(fullPath, line, { mode: 0o600 })
  } catch {
    fs.mkdirSync(dirname(fullPath), { mode: 0o700 })
    fs.appendFileSync(fullPath, line, { mode: 0o600 })
  }
}

/**
 * Sync tail read for reAppendSessionMetadata's external-writer check.
 * fstat on the already-open fd (no extra path lookup); reads the same
 * LITE_READ_BUF_SIZE window that readLiteMetadata scans. Returns empty
 * string on any error so callers fall through to unconditional behavior.
 */
function readFileTailSync(fullPath: string): string {
  let fd: number | undefined
  try {
    fd = openSync(fullPath, 'r')
    const st = fstatSync(fd)
    const tailOffset = Math.max(0, st.size - LITE_READ_BUF_SIZE)
    const buf = Buffer.allocUnsafe(
      Math.min(LITE_READ_BUF_SIZE, st.size - tailOffset),
    )
    const bytesRead = readSync(fd, buf, 0, buf.length, tailOffset)
    return buf.toString('utf8', 0, bytesRead)
  } catch {
    return ''
  } finally {
    if (fd !== undefined) {
      try {
        closeSync(fd)
      } catch {
        // closeSync can throw; swallow to preserve return '' contract
      }
    }
  }
}
/* eslint-enable custom-rules/no-sync-fs */

// ─── Session messages cache ─────────────────────────────────────────────────

/**
 * Loads a transcript and returns session-wide data. Used internally.
 */
async function loadSessionFile(sessionId: UUID) {
  const { loadTranscriptFile } = await import('./sessionStorage-transcript.js')
  const sessionFile = join(
    getSessionProjectDir() ?? getProjectDir(getOriginalCwd()),
    `${sessionId}.jsonl`,
  )
  return loadTranscriptFile(sessionFile)
}

/**
 * W-SESSION-TRANSCRIPT-CORRUPTION (2026-05-31): the uuid of the most-recent
 * persisted, non-sidechain, chain-participating message on disk for `sessionId`
 * — i.e. the real transcript tail. Used by recordTranscript to re-anchor new
 * live frames after reconstructed-history frames (the prior chain backbone)
 * have been excluded from persistence, so --resume's parentUuid chain is not
 * orphaned. Returns null when the on-disk transcript has no such frame.
 */
async function getPersistedTranscriptTailUuid(
  sessionId: UUID,
): Promise<UUID | null> {
  try {
    const { messages } = await loadSessionFile(sessionId)
    const { selectResumeLeafUuid } = await import('./sessionStorage-transcript.js')
    // selectResumeLeafUuid mirrors --resume's own leaf definition (latest
    // user/assistant frame with no user/assistant child, non-sidechain), so the
    // new live frame anchors to the exact frame --resume reconstructs as the
    // tip — a branched transcript or a late-timestamped interior/attachment/
    // system frame cannot mis-parent the chain.
    return (selectResumeLeafUuid(
      messages.values() as Iterable<{
        timestamp: string
        uuid?: unknown
        isSidechain?: boolean
        type?: unknown
        parentUuid?: unknown
      }>,
    ) as UUID | null)
  } catch {
    // Tail read is a best-effort re-anchor; a missing/unreadable transcript
    // must not break the live write. Fall back to null (root parent).
    return null
  }
}

/**
 * Gets message UUIDs for a specific session without loading all sessions.
 * Memoized to avoid re-reading the same session file multiple times.
 */
const getSessionMessages = memoize(
  async (sessionId: UUID): Promise<Set<UUID>> => {
    const { messages } = await loadSessionFile(sessionId)
    return new Set(messages.keys()) as Set<import('crypto').UUID>
  },
  (sessionId: UUID) => sessionId,
)

/**
 * Clear the memoized session messages cache.
 * Call after compaction when old message UUIDs are no longer valid.
 */
export function clearSessionMessagesCache(): void {
  getSessionMessages.cache.clear?.()
}

/**
 * Check if a message UUID exists in the session storage
 */
export async function doesMessageExistInSession(
  sessionId: UUID,
  messageUuid: UUID,
): Promise<boolean> {
  const messageSet = await getSessionMessages(sessionId)
  return messageSet.has(messageUuid)
}

// Import LogOption type used in getSessionIdFromLog/isLiteLog
import type { LogOption } from '../types/logs.js'
