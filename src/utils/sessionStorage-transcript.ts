/**
 * sessionStorage-transcript.ts — Transcript loading, chain building,
 * conversation reconstruction, JSONL parsing, and message filtering.
 *
 * Split from sessionStorage.ts (refactor, no logic changes).
 * Depends on: sessionStorage-paths
 */
import { feature } from './featurePolyfill.js'
import type { UUID } from 'crypto'
import { readFile, stat } from 'fs/promises'
import { logEvent } from 'src/services/analytics/index.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../services/analytics/growthbook.js'
import { getActiveBuiltInCommandNames } from './builtInCommandNamesProvider.js'
import { COMMAND_NAME_TAG, TICK_TAG } from '../constants/xml.js'
import { REPL_TOOL_NAME } from '../tools/REPLTool/constants.js'
import { type AgentId } from '../types/ids.js'
import type { AttributionSnapshotMessage } from '../types/logs.js'
import {
  type ContentReplacementEntry,
  type ContextCollapseCommitEntry,
  type ContextCollapseSnapshotEntry,
  type Entry,
  type FileHistorySnapshotMessage,
  type LogOption,
  type PersistedWorktreeSession,
  type SerializedMessage,
  sortLogs,
  type TranscriptMessage,
} from '../types/logs.js'
import type {
  AssistantMessage,
  Message,
  SystemCompactBoundaryMessage,
} from '../types/message.js'
import type { ContentReplacementRecord } from './toolResultStorage.js'
import type { FileHistorySnapshot } from './fileHistory.js'
import { isEnvTruthy } from './envUtils.js'
import { logError } from './log.js'
import { logForDebugging } from './debug.js'
import { extractTag, isCompactBoundaryMessage } from './messages.js'
import { parseJSONL } from './json.js'
import { jsonParse, jsonStringify } from './slowOperations.js'
import {
  extractJsonStringField,
  extractLastJsonStringField,
  LITE_READ_BUF_SIZE,
  readHeadAndTail,
  readTranscriptForLoad,
  SKIP_PRECOMPACT_THRESHOLD,
} from './sessionStoragePortable.js'
import {
  type Transcript,
  isTranscriptMessage,
  isLegacyProgressEntry,
  getUserType,
  SKIP_FIRST_PROMPT_PATTERN,
} from './sessionStorage-paths.js'

// ─── Message filtering ──────────────────────────────────────────────────────

// Exported so useLogMessages can sync-compute the last loggable uuid
// without awaiting recordTranscript's return value (race-free hint tracking).
export function isLoggableMessage(m: Message): boolean {
  if (m.type === 'progress') return false
  // W-SESSION-TRANSCRIPT-CORRUPTION (2026-05-31): reconstructed history frames
  // (extractRuntimeHistoryMessages / turnsToMessages) are model-context
  // injection seeded into QueryEngine.mutableMessages, NOT new conversation
  // content. They carry synthetic `runtime-*` uuids that the uuid-only
  // recordTranscript/appendEntry dedup cannot reconcile with the real live
  // frames already on disk, so persisting them duplicated every continuous
  // turn (live REAL frame + RT reconstruction next turn) and stamped 1970 on
  // interrupted turns. Skipping them here (cleanMessagesForLogging's predicate)
  // keeps them out of the project transcript while they still feed the LLM.
  // recordTranscript re-anchors the new live frames' parentUuid to the on-disk
  // tail so --resume's chain is not orphaned by the excluded backbone.
  if ((m as { isReconstructedHistory?: boolean }).isReconstructedHistory === true) {
    return false
  }
  // IMPORTANT: We deliberately filter out most attachments for non-ants because
  // they have sensitive info for training that we don't want exposed to the public.
  // When enabled, we allow hook_additional_context through since it contains
  // user-configured hook output that is useful for session context on resume.
  if (m.type === 'attachment' && getUserType() !== 'ant') {
    if (
      m.attachment.type === 'hook_additional_context' &&
      isEnvTruthy(process.env.CRABCODE_SAVE_HOOK_ADDITIONAL_CONTEXT)
    ) {
      return true
    }
    return false
  }
  return true
}

function collectReplIds(messages: readonly Message[]): Set<string> {
  const ids = new Set<string>()
  for (const m of messages) {
    if (m.type === 'assistant' && Array.isArray(m.message.content)) {
      for (const b of m.message.content) {
        if (b.type === 'tool_use' && b.name === REPL_TOOL_NAME) {
          ids.add(b.id)
        }
      }
    }
  }
  return ids
}

/**
 * For external users, make REPL invisible in the persisted transcript: strip
 * REPL tool_use/tool_result pairs and promote isVirtual messages to real. On
 * --resume the model then sees a coherent native-tool-call history (assistant
 * called Bash, got result, called Read, got result) without the REPL wrapper.
 * Ant transcripts keep the wrapper so /share training data sees REPL usage.
 *
 * replIds is pre-collected from the FULL session array, not the slice being
 * transformed — recordTranscript receives incremental slices where the REPL
 * tool_use (earlier render) and its tool_result (later render, after async
 * execution) land in separate calls. A fresh per-call Set would miss the id
 * and leave an orphaned tool_result on disk.
 */
function transformMessagesForExternalTranscript(
  messages: Transcript,
  replIds: Set<string>,
): Transcript {
  return messages.flatMap((m): Transcript => {
    if (m.type === 'assistant' && Array.isArray(m.message.content)) {
      const content = m.message.content
      const hasRepl = content.some(
        b => b.type === 'tool_use' && b.name === REPL_TOOL_NAME,
      )
      const filtered = hasRepl
        ? content.filter(
            b => !(b.type === 'tool_use' && b.name === REPL_TOOL_NAME),
          )
        : content
      if (filtered.length === 0) return []
      if (m.isVirtual) {
        const { isVirtual: _omit, ...rest } = m
        return [{ ...rest, message: { ...m.message, content: filtered } }]
      }
      if (filtered !== content) {
        return [{ ...m, message: { ...m.message, content: filtered } }]
      }
      return [m]
    }
    if (m.type === 'user' && Array.isArray(m.message.content)) {
      const content = m.message.content
      const hasRepl = content.some(
        b => b.type === 'tool_result' && replIds.has(b.tool_use_id),
      )
      const filtered = hasRepl
        ? content.filter(
            b => !(b.type === 'tool_result' && replIds.has(b.tool_use_id)),
          )
        : content
      if (filtered.length === 0) return []
      if (m.isVirtual) {
        const { isVirtual: _omit, ...rest } = m
        return [{ ...rest, message: { ...m.message, content: filtered } }]
      }
      if (filtered !== content) {
        return [{ ...m, message: { ...m.message, content: filtered } }]
      }
      return [m]
    }
    // string-content user, system, attachment
    if ('isVirtual' in m && m.isVirtual) {
      const { isVirtual: _omit, ...rest } = m
      return [rest]
    }
    return [m]
  })
}

export function cleanMessagesForLogging(
  messages: Message[],
  allMessages: readonly Message[] = messages,
): Transcript {
  const filtered = messages.filter(isLoggableMessage) as Transcript
  return getUserType() !== 'ant'
    ? transformMessagesForExternalTranscript(
        filtered,
        collectReplIds(allMessages),
      )
    : filtered
}

export function removeExtraFields(
  transcript: TranscriptMessage[],
): SerializedMessage[] {
  return transcript.map(m => {
    const { isSidechain, parentUuid, ...serializedMessage } = m
    return serializedMessage
  })
}

// ─── First prompt extraction ─────────────────────────────────────────────────

function extractFirstPrompt(transcript: TranscriptMessage[]): string {
  const textContent = getFirstMeaningfulUserMessageTextContent(transcript)
  if (textContent) {
    let result = textContent.replace(/\n/g, ' ').trim()

    // Store a reasonably long version for display-time truncation
    // The actual truncation will be applied at display time based on terminal width
    if (result.length > 200) {
      result = result.slice(0, 200).trim() + '…'
    }

    return result
  }

  return 'No prompt'
}

/**
 * Gets the last user message that was processed (i.e., before any non-user message appears).
 * Used to determine if a session has valid user interaction.
 */
export function getFirstMeaningfulUserMessageTextContent<T extends Message>(
  transcript: T[],
): string | undefined {
  for (const msg of transcript) {
    if (msg.type !== 'user' || msg.isMeta) continue
    // Skip compact summary messages - they should not be treated as the first prompt
    if ('isCompactSummary' in msg && msg.isCompactSummary) continue

    const content = msg.message?.content
    if (!content) continue

    // Collect all text values. For array content (common in VS Code where
    // IDE metadata tags come before the user's actual prompt), iterate all
    // text blocks so we don't miss the real prompt hidden behind
    // <ide_selection>/<ide_opened_file> blocks.
    const texts: string[] = []
    if (typeof content === 'string') {
      texts.push(content)
    } else if (Array.isArray(content)) {
      for (const block of content) {
        if (block.type === 'text' && block.text) {
          texts.push(block.text)
        }
      }
    }

    for (const textContent of texts) {
      if (!textContent) continue

      const commandNameTag = extractTag(textContent, COMMAND_NAME_TAG)
      if (commandNameTag) {
        const commandName = commandNameTag.replace(/^\//, '')

        // If it's a built-in command, then it's unlikely to provide
        // meaningful context (e.g. `/model best`)
        if (getActiveBuiltInCommandNames().has(commandName)) {
          continue
        } else {
          // Otherwise, for custom commands, then keep it only if it has
          // arguments (e.g. `/review reticulate splines`)
          const commandArgs = extractTag(textContent, 'command-args')?.trim()
          if (!commandArgs) {
            continue
          }
          // Return clean formatted command instead of raw XML
          return `${commandNameTag} ${commandArgs}`
        }
      }

      // Format bash input with ! prefix (as user typed it). Checked before
      // the generic XML skip so bash-mode sessions get a meaningful title.
      const bashInput = extractTag(textContent, 'bash-input')
      if (bashInput) {
        return `! ${bashInput}`
      }

      // Skip non-meaningful messages (local command output, hook output,
      // autonomous tick prompts, task notifications, pure IDE metadata tags)
      if (SKIP_FIRST_PROMPT_PATTERN.test(textContent)) {
        continue
      }

      return textContent
    }
  }
  return undefined
}

// ─── Conversation chain building ─────────────────────────────────────────────

/**
 * O(n) single-pass: find the message with the latest timestamp matching a predicate.
 * Replaces the `[...values].filter(pred).sort((a,b) => Date(b)-Date(a))[0]` pattern
 * which is O(n log n) + 2n Date allocations.
 */
export function findLatestMessage<T extends { timestamp: string }>(
  messages: Iterable<T>,
  predicate: (m: T) => boolean,
): T | undefined {
  let latest: T | undefined
  let maxTime = -Infinity
  for (const m of messages) {
    if (!predicate(m)) continue
    const t = Date.parse(m.timestamp)
    if (t > maxTime) {
      maxTime = t
      latest = m
    }
  }
  return latest
}

/**
 * W-SESSION-TRANSCRIPT-CORRUPTION (2026-05-31): the uuid of the chain tip — the
 * latest-timestamp USER/ASSISTANT frame that no other user/assistant frame
 * parents to (a "leaf"), excluding sidechain. This is exactly the leaf the
 * --resume path picks (see `hasUserAssistantChild` in buildConversationChain),
 * so re-anchoring new live frames to it after the reconstructed backbone is
 * excluded keeps the persisted chain identical to what --resume reconstructs.
 * Pure (no IO) so it is directly unit-testable. Returns null when no
 * user/assistant leaf exists.
 */
export function selectResumeLeafUuid(
  messages: Iterable<{
    timestamp: string
    uuid?: unknown
    isSidechain?: boolean
    type?: unknown
    parentUuid?: unknown
  }>,
): string | null {
  // Two passes so the input may be any Iterable (Map.values() is single-use):
  const buffered = Array.from(messages)
  const hasUserAssistantChild = new Set<string>()
  for (const m of buffered) {
    // Sidechain (sub-agent) frames live in a separate file and must not count
    // as children of main-thread frames — otherwise a sidechain parented to the
    // real leaf would wrongly demote it to a non-leaf.
    if (
      m.isSidechain !== true &&
      typeof m.parentUuid === 'string' &&
      (m.type === 'user' || m.type === 'assistant')
    ) {
      hasUserAssistantChild.add(m.parentUuid)
    }
  }
  const leaf = findLatestMessage(buffered, m =>
    m.isSidechain !== true &&
    typeof m.uuid === 'string' &&
    (m.type === 'user' || m.type === 'assistant') &&
    !hasUserAssistantChild.has(m.uuid as string),
  )
  return (leaf?.uuid as string | undefined) ?? null
}

/**
 * Builds a conversation chain from a leaf message to root
 * @param messages Map of all messages
 * @param leafMessage The leaf message to start from
 * @returns Array of messages from root to leaf
 */
export function buildConversationChain(
  messages: Map<string, TranscriptMessage>,
  leafMessage: TranscriptMessage,
): TranscriptMessage[] {
  const transcript: TranscriptMessage[] = []
  const seen = new Set<string>()
  let currentMsg: TranscriptMessage | undefined = leafMessage
  while (currentMsg) {
    if (seen.has(currentMsg.uuid!)) {
      logError(
        new Error(
          `Cycle detected in parentUuid chain at message ${currentMsg.uuid}. Returning partial transcript.`,
        ),
      )
      logEvent('tengu_chain_parent_cycle', {})
      break
    }
    seen.add(currentMsg.uuid!)
    transcript.push(currentMsg)
    currentMsg = currentMsg.parentUuid
      ? messages.get(currentMsg.parentUuid)
      : undefined
  }
  transcript.reverse()
  return recoverOrphanedParallelToolResults(messages, transcript, seen)
}

/**
 * Post-pass for buildConversationChain: recover sibling assistant blocks and
 * tool_results that the single-parent walk orphaned.
 *
 * Streaming (crabcode.ts:~2024) emits one AssistantMessage per content_block_stop
 * — N parallel tool_uses → N messages, distinct uuid, same message.id. Each
 * tool_result's sourceToolAssistantUUID points to its own one-block assistant,
 * so insertMessageChain's override (line ~894) writes each TR's parentUuid to a
 * DIFFERENT assistant. The topology is a DAG; the walk above is a linked-list
 * traversal and keeps only one branch.
 *
 * Two loss modes observed in production (both fixed here):
 *   1. Sibling assistant orphaned: walk goes prev→asstA→TR_A→next, drops asstB
 *      (same message.id, chained off asstA) and TR_B.
 *   2. Progress-fork (legacy, pre-#23537): each tool_use asst had a progress
 *      child (continued the write chain) AND a TR child. Walk followed
 *      progress; TRs were dropped. No longer written (progress removed from
 *      transcript persistence), but old transcripts still have this shape.
 *
 * Read-side fix: the write topology is already on disk for old transcripts;
 * this recovery pass handles them.
 */
function recoverOrphanedParallelToolResults(
  messages: Map<string, TranscriptMessage>,
  chain: TranscriptMessage[],
  seen: Set<string>,
): TranscriptMessage[] {
  type ChainAssistant = Extract<TranscriptMessage, { type: 'assistant' }>
  const chainAssistants = chain.filter(
    (m): m is ChainAssistant => m.type === 'assistant',
  )
  if (chainAssistants.length === 0) return chain

  // Anchor = last on-chain member of each sibling group. chainAssistants is
  // already in chain order, so later iterations overwrite → last wins.
  const anchorByMsgId = new Map<string, ChainAssistant>()
  for (const a of chainAssistants) {
    if (a.message.id) anchorByMsgId.set(a.message.id, a)
  }

  // O(n) precompute: sibling groups and TR index.
  // TRs indexed by parentUuid — insertMessageChain:~894 already wrote that
  // as the srcUUID, and --fork-session strips srcUUID but keeps parentUuid.
  const siblingsByMsgId = new Map<string, TranscriptMessage[]>()
  const toolResultsByAsst = new Map<string, TranscriptMessage[]>()
  for (const m of messages.values()) {
    if (m.type === 'assistant' && m.message.id) {
      const group = siblingsByMsgId.get(m.message.id)
      if (group) group.push(m)
      else siblingsByMsgId.set(m.message.id, [m])
    } else if (
      m.type === 'user' &&
      m.parentUuid &&
      Array.isArray(m.message.content) &&
      m.message.content.some(b => b.type === 'tool_result')
    ) {
      const group = toolResultsByAsst.get(m.parentUuid)
      if (group) group.push(m)
      else toolResultsByAsst.set(m.parentUuid, [m])
    }
  }

  // For each message.id group touching the chain: collect off-chain siblings,
  // then off-chain TRs for ALL members. Splice right after the last on-chain
  // member so the group stays contiguous for normalizeMessagesForAPI's merge
  // and every TR lands after its tool_use.
  const processedGroups = new Set<string>()
  const inserts = new Map<string, TranscriptMessage[]>()
  let recoveredCount = 0
  for (const asst of chainAssistants) {
    const msgId = asst.message.id
    if (!msgId || processedGroups.has(msgId)) continue
    processedGroups.add(msgId)

    const group = siblingsByMsgId.get(msgId) ?? [asst]
    const orphanedSiblings = group.filter(s => !seen.has(s.uuid!))
    const orphanedTRs: TranscriptMessage[] = []
    for (const member of group) {
      const trs = toolResultsByAsst.get(member.uuid!)
      if (!trs) continue
      for (const tr of trs) {
        if (!seen.has(tr.uuid!)) orphanedTRs.push(tr)
      }
    }
    if (orphanedSiblings.length === 0 && orphanedTRs.length === 0) continue

    // Timestamp sort keeps content-block / completion order; stable-sort
    // preserves JSONL write order on ties.
    orphanedSiblings.sort((a, b) => a.timestamp.localeCompare(b.timestamp))
    orphanedTRs.sort((a, b) => a.timestamp.localeCompare(b.timestamp))

    const anchor = anchorByMsgId.get(msgId)!
    const recovered = [...orphanedSiblings, ...orphanedTRs]
    for (const r of recovered) seen.add(r.uuid!)
    recoveredCount += recovered.length
    inserts.set(anchor.uuid!, recovered)
  }

  if (recoveredCount === 0) return chain
  logEvent('tengu_chain_parallel_tr_recovered', {
    recovered_count: recoveredCount,
  })

  const result: TranscriptMessage[] = []
  for (const m of chain) {
    result.push(m)
    const toInsert = inserts.get(m.uuid!)
    if (toInsert) result.push(...toInsert)
  }
  return result
}

/**
 * Find the latest turn_duration checkpoint in the reconstructed chain and
 * compare its recorded messageCount against the chain's position at that
 * point. Emits tengu_resume_consistency_delta for BigQuery monitoring of
 * write→load round-trip drift — the class of bugs where snip/compact/
 * parallel-TR operations mutate in-memory but the parentUuid walk on disk
 * reconstructs a different set (adamr-20260320-165831: 397K displayed →
 * 1.65M actual on resume).
 *
 * delta > 0: resume loaded MORE than in-session (the usual failure mode)
 * delta < 0: resume loaded FEWER (chain truncation — #22453 class)
 * delta = 0: round-trip consistent
 *
 * Called from loadConversationForResume — fires once per resume, not on
 * /share or log-listing chain rebuilds.
 */
export function checkResumeConsistency(chain: Message[]): void {
  for (let i = chain.length - 1; i >= 0; i--) {
    const m = chain[i]!
    if (m.type !== 'system' || m.subtype !== 'turn_duration') continue
    const expected = m.messageCount
    if (expected === undefined) return
    // `i` is the 0-based index of the checkpoint in the reconstructed chain.
    // The checkpoint was appended AFTER messageCount messages, so its own
    // position should be messageCount (i.e., i === expected).
    const actual = i
    logEvent('tengu_resume_consistency_delta', {
      expected,
      actual,
      delta: actual - expected,
      chain_length: chain.length,
      checkpoint_age_entries: chain.length - 1 - i,
    })
    return
  }
}

// ─── Preserved segment relinks ───────────────────────────────────────────────

/**
 * Splice the preserved segment back into the chain after compaction.
 *
 * Preserved messages exist in the JSONL with their ORIGINAL pre-compact
 * parentUuids (recordTranscript dedup-skipped them — can't rewrite).
 * The internal chain (keep[i+1]→keep[i]) is intact; only endpoints need
 * patching: head→anchor, and anchor's other children→tail. Anchor is the
 * last summary for suffix-preserving, boundary itself for prefix-preserving.
 *
 * Only the LAST seg-boundary is relinked — earlier segs were summarized
 * into it. Everything physically before the absolute-last boundary (except
 * preservedUuids) is deleted, which handles all multi-boundary shapes
 * without special-casing.
 *
 * Mutates the Map in place.
 */
function applyPreservedSegmentRelinks(
  messages: Map<string, TranscriptMessage>,
): void {
  type Seg = NonNullable<
    SystemCompactBoundaryMessage['compactMetadata']['preservedSegment']
  >

  // Find the absolute-last boundary and the last seg-boundary (can differ:
  // manual /compact after reactive compact → seg is stale).
  let lastSeg: Seg | undefined
  let lastSegBoundaryIdx = -1
  let absoluteLastBoundaryIdx = -1
  const entryIndex = new Map<string, number>()
  let i = 0
  for (const entry of messages.values()) {
    entryIndex.set(entry.uuid!, i)
    if (isCompactBoundaryMessage(entry)) {
      absoluteLastBoundaryIdx = i
      const seg = entry.compactMetadata?.preservedSegment
      if (seg) {
        lastSeg = seg
        lastSegBoundaryIdx = i
      }
    }
    i++
  }
  // No seg anywhere → no-op. findUnresolvedToolUse etc. read the full map.
  if (!lastSeg) return

  // Seg stale (no-seg boundary came after): skip relink, still prune at
  // absolute — otherwise the stale preserved chain becomes a phantom leaf.
  const segIsLive = lastSegBoundaryIdx === absoluteLastBoundaryIdx

  // Stable reason codes for the fail-soft telemetry below. Keep numeric values
  // append-only so dashboards can distinguish old clients without logging the
  // untrusted UUID metadata itself.
  const RELINK_FAILURE = {
    malformedMetadata: 1,
    brokenTailWalk: 2,
    missingAnchor: 3,
    anchorBeforeBoundary: 4,
    anchorInsideSegment: 5,
    preservedEntryAfterBoundary: 6,
  } as const
  const logBrokenRelink = (reason: number, walkSteps: number): void => {
    logEvent('tengu_relink_walk_broken', {
      reason,
      tailInTranscript:
        typeof lastSeg.tailUuid === 'string' &&
        messages.has(lastSeg.tailUuid),
      headInTranscript:
        typeof lastSeg.headUuid === 'string' &&
        messages.has(lastSeg.headUuid),
      anchorInTranscript:
        typeof lastSeg.anchorUuid === 'string' &&
        messages.has(lastSeg.anchorUuid),
      walkSteps,
      transcriptSize: messages.size,
    })
  }

  // Validate tail→head BEFORE mutating so malformed metadata is a true
  // no-op (walk stops at headUuid, doesn't need the relink to run first).
  const preservedUuids = new Set<string>()
  if (segIsLive) {
    if (
      typeof lastSeg.headUuid !== 'string' ||
      lastSeg.headUuid.length === 0 ||
      typeof lastSeg.anchorUuid !== 'string' ||
      lastSeg.anchorUuid.length === 0 ||
      typeof lastSeg.tailUuid !== 'string' ||
      lastSeg.tailUuid.length === 0
    ) {
      logBrokenRelink(RELINK_FAILURE.malformedMetadata, 0)
      return
    }

    const walkSeen = new Set<string>()
    let cur = messages.get(lastSeg.tailUuid)
    let reachedHead = false
    while (cur && !walkSeen.has(cur.uuid!)) {
      walkSeen.add(cur.uuid!)
      preservedUuids.add(cur.uuid!)
      if (cur.uuid === lastSeg.headUuid) {
        reachedHead = true
        break
      }
      cur = cur.parentUuid ? messages.get(cur.parentUuid) : undefined
    }
    if (!reachedHead) {
      // tail→head walk broke — a UUID in the preserved segment isn't in the
      // transcript. Returning here skips the prune below, so resume loads
      // the full pre-compact history. Known cause: mid-turn-yielded
      // attachment pushed to mutableMessages but never recordTranscript'd
      // (SDK subprocess restarted before next turn's qe:420 flush).
      logBrokenRelink(RELINK_FAILURE.brokenTailWalk, walkSeen.size)
      return
    }

    // The endpoint walk alone is insufficient: a corrupt anchor can still
    // make us prune the original history and then attach the preserved head
    // to a missing/soon-to-be-pruned parent. The anchor is always the latest
    // boundary itself (prefix-preserving) or a summary written after it
    // (suffix-preserving). Every preserved message was already on disk before
    // the boundary. Validate all of those invariants before any Map mutation.
    const anchorIdx = entryIndex.get(lastSeg.anchorUuid)
    if (anchorIdx === undefined) {
      logBrokenRelink(RELINK_FAILURE.missingAnchor, walkSeen.size)
      return
    }
    if (preservedUuids.has(lastSeg.anchorUuid)) {
      logBrokenRelink(RELINK_FAILURE.anchorInsideSegment, walkSeen.size)
      return
    }
    if (anchorIdx < absoluteLastBoundaryIdx) {
      logBrokenRelink(RELINK_FAILURE.anchorBeforeBoundary, walkSeen.size)
      return
    }
    if (
      [...preservedUuids].some(uuid => {
        const idx = entryIndex.get(uuid)
        return idx === undefined || idx >= absoluteLastBoundaryIdx
      })
    ) {
      logBrokenRelink(
        RELINK_FAILURE.preservedEntryAfterBoundary,
        walkSeen.size,
      )
      return
    }
  }

  if (segIsLive) {
    const head = messages.get(lastSeg.headUuid)
    if (head) {
      messages.set(lastSeg.headUuid, {
        ...head,
        parentUuid: lastSeg.anchorUuid as UUID,
      })
    }
    // Tail-splice: anchor's other children → tail. No-op if already pointing
    // at tail (the useLogMessages race case).
    for (const [uuid, msg] of messages) {
      if (msg.parentUuid === lastSeg.anchorUuid && uuid !== lastSeg.headUuid) {
        messages.set(uuid, { ...msg, parentUuid: lastSeg.tailUuid as UUID })
      }
    }
    // Zero stale usage: on-disk input_tokens reflect pre-compact context
    // (~190K) — stripStaleUsage only patched in-memory copies that were
    // dedup-skipped. Without this, resume → immediate autocompact spiral.
    for (const uuid of preservedUuids) {
      const msg = messages.get(uuid)
      if (msg?.type !== 'assistant') continue
      messages.set(uuid, {
        ...msg,
        message: {
          ...msg.message,
          usage: {
            ...msg.message.usage,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
          },
        },
      })
    }
  }

  // Prune everything physically before the absolute-last boundary that
  // isn't preserved. preservedUuids empty when !segIsLive → full prune.
  const toDelete: string[] = []
  for (const [uuid] of messages) {
    const idx = entryIndex.get(uuid)
    if (
      idx !== undefined &&
      idx < absoluteLastBoundaryIdx &&
      !preservedUuids.has(uuid)
    ) {
      toDelete.push(uuid)
    }
  }
  for (const uuid of toDelete) messages.delete(uuid)
}

// ─── Snip removals ───────────────────────────────────────────────────────────

/**
 * Delete messages that Snip executions removed from the in-memory array,
 * and relink parentUuid across the gaps.
 *
 * Unlike compact_boundary which truncates a prefix, snip removes
 * middle ranges. The JSONL is append-only, so removed messages stay on disk
 * and the surviving messages' parentUuid chains walk through them. Without
 * this filter, buildConversationChain reconstructs the full unsnipped history
 * and resume immediately PTLs (adamr-20260320-165831: 397K displayed → 1.65M
 * actual).
 *
 * Deleting alone is not enough: the surviving message AFTER a removed range
 * has parentUuid pointing INTO the gap. buildConversationChain would hit
 * messages.get(undefined) and stop, orphaning everything before the gap. So
 * after delete we relink: for each survivor with a dangling parentUuid, walk
 * backward through the removed region's own parent links to the first
 * non-removed ancestor.
 *
 * The boundary records removedUuids at execution time so we can replay the
 * exact removal on load. Older boundaries without removedUuids are skipped —
 * resume loads their pre-snip history (the pre-fix behavior).
 *
 * Mutates the Map in place.
 */
function applySnipRemovals(messages: Map<string, TranscriptMessage>): void {
  // Structural check — snipMetadata only exists on the boundary subtype.
  // Avoids the subtype literal which is in excluded-strings.txt
  // (HISTORY_SNIP is ant-only; the literal must not leak into external builds).
  type WithSnipMeta = { snipMetadata?: { removedUuids?: UUID[] } }
  const toDelete = new Set<string>()
  for (const entry of messages.values()) {
    const removedUuids = (entry as WithSnipMeta).snipMetadata?.removedUuids
    if (!removedUuids) continue
    for (const uuid of removedUuids) toDelete.add(uuid)
  }
  if (toDelete.size === 0) return

  // Capture each to-delete entry's own parentUuid BEFORE deleting so we can
  // walk backward through contiguous removed ranges. Entries not in the Map
  // (already absent, e.g. from a prior compact_boundary prune) contribute no
  // link; the relink walk will stop at the gap and pick up null (chain-root
  // behavior — same as if compact truncated there, which it did).
  const deletedParent = new Map<string, string | null>()
  let removedCount = 0
  for (const uuid of toDelete) {
    const entry = messages.get(uuid)
    if (!entry) continue
    deletedParent.set(uuid, entry.parentUuid)
    messages.delete(uuid)
    removedCount++
  }

  // Relink survivors with dangling parentUuid. Walk backward through
  // deletedParent until we hit a UUID not in toDelete (or null). Path
  // compression: after resolving, seed the map with the resolved link so
  // subsequent survivors sharing the same chain segment don't re-walk.
  const resolve = (start: string): string | null => {
    const path: string[] = []
    let cur: string | null | undefined = start
    while (cur && toDelete.has(cur)) {
      path.push(cur)
      cur = deletedParent.get(cur)
      if (cur === undefined) {
        cur = null
        break
      }
    }
    for (const p of path) deletedParent.set(p, cur)
    return cur
  }
  let relinkedCount = 0
  for (const [uuid, msg] of messages) {
    if (!msg.parentUuid || !toDelete.has(msg.parentUuid)) continue
    messages.set(uuid, { ...msg, parentUuid: resolve(msg.parentUuid) as UUID | null })
    relinkedCount++
  }

  logEvent('tengu_snip_resume_filtered', {
    removed_count: removedCount,
    relinked_count: relinkedCount,
  })
}

// ─── File history / attribution snapshot chains ──────────────────────────────

/**
 * Builds a file history snapshot chain from the conversation
 */
function buildFileHistorySnapshotChain(
  fileHistorySnapshots: Map<string, FileHistorySnapshotMessage>,
  conversation: TranscriptMessage[],
): FileHistorySnapshot[] {
  const snapshots: FileHistorySnapshot[] = []
  // messageId → last index in snapshots[] for O(1) update lookup
  const indexByMessageId = new Map<string, number>()
  for (const message of conversation) {
    const snapshotMessage = fileHistorySnapshots.get(message.uuid!)
    if (!snapshotMessage) {
      continue
    }
    const { snapshot, isSnapshotUpdate } = snapshotMessage
    const existingIndex = isSnapshotUpdate
      ? indexByMessageId.get(snapshot.messageId)
      : undefined
    if (existingIndex === undefined) {
      indexByMessageId.set(snapshot.messageId, snapshots.length)
      snapshots.push(snapshot)
    } else {
      snapshots[existingIndex] = snapshot
    }
  }
  return snapshots
}

/**
 * Builds an attribution snapshot chain from the conversation.
 * Unlike file history snapshots, attribution snapshots are returned in full
 * because they use generated UUIDs (not message UUIDs) and represent
 * cumulative state that should be restored on session resume.
 */
function buildAttributionSnapshotChain(
  attributionSnapshots: Map<string, AttributionSnapshotMessage>,
  _conversation: TranscriptMessage[],
): AttributionSnapshotMessage[] {
  // Return all attribution snapshots - they will be merged during restore
  return Array.from(attributionSnapshots.values())
}

// ─── Visible message counting ────────────────────────────────────────────────

/**
 * Checks if a user message has visible content (text or image, not just tool_result).
 * Tool results are displayed as part of collapsed groups, not as standalone messages.
 * Also excludes meta messages which are not shown to the user.
 */
function hasVisibleUserContent(message: TranscriptMessage): boolean {
  if (message.type !== 'user') return false

  // Meta messages are not shown to the user
  if (message.isMeta) return false

  const content = message.message?.content
  if (!content) return false

  // String content is always visible
  if (typeof content === 'string') {
    return content.trim().length > 0
  }

  // Array content: check for text or image blocks (not tool_result)
  if (Array.isArray(content)) {
    return content.some(
      block =>
        block.type === 'text' ||
        block.type === 'image' ||
        block.type === 'document',
    )
  }

  return false
}

/**
 * Checks if an assistant message has visible text content (not just tool_use blocks).
 * Tool uses are displayed as grouped/collapsed UI elements, not as standalone messages.
 */
function hasVisibleAssistantContent(message: TranscriptMessage): boolean {
  if (message.type !== 'assistant') return false

  const content = message.message?.content
  if (!content || !Array.isArray(content)) return false

  // Check for text block (not just tool_use/thinking blocks)
  return content.some(
    block =>
      block.type === 'text' &&
      typeof block.text === 'string' &&
      block.text.trim().length > 0,
  )
}

/**
 * Counts visible messages that would appear as conversation turns in the UI.
 * Excludes:
 * - System, attachment, and progress messages
 * - User messages with isMeta flag (hidden from user)
 * - User messages that only contain tool_result blocks (displayed as collapsed groups)
 * - Assistant messages that only contain tool_use blocks (displayed as collapsed groups)
 */
function countVisibleMessages(transcript: TranscriptMessage[]): number {
  let count = 0
  for (const message of transcript) {
    switch (message.type) {
      case 'user':
        // Count user messages with visible content (text, image, not just tool_result or meta)
        if (hasVisibleUserContent(message)) {
          count++
        }
        break
      case 'assistant':
        // Count assistant messages with text content (not just tool_use)
        if (hasVisibleAssistantContent(message)) {
          count++
        }
        break
      case 'attachment':
      case 'system':
      case 'progress':
        // These message types are not counted as visible conversation turns
        break
    }
  }
  return count
}

// ─── convertToLogOption ──────────────────────────────────────────────────────

export function convertToLogOption(
  transcript: TranscriptMessage[],
  value: number = 0,
  summary?: string,
  customTitle?: string,
  fileHistorySnapshots?: FileHistorySnapshot[],
  tag?: string,
  fullPath?: string,
  attributionSnapshots?: AttributionSnapshotMessage[],
  agentSetting?: string,
  contentReplacements?: ContentReplacementRecord[],
): LogOption {
  const lastMessage = transcript.at(-1)!
  const firstMessage = transcript[0]!

  // Get the first user message for the prompt
  const firstPrompt = extractFirstPrompt(transcript)

  // Create timestamps from message timestamps
  const created = new Date(firstMessage.timestamp)
  const modified = new Date(lastMessage.timestamp)

  return {
    date: lastMessage.timestamp,
    messages: removeExtraFields(transcript),
    fullPath,
    value,
    created,
    modified,
    firstPrompt,
    messageCount: countVisibleMessages(transcript),
    isSidechain: firstMessage.isSidechain,
    teamName: firstMessage.teamName,
    agentName: firstMessage.agentName,
    agentSetting,
    leafUuid: lastMessage.uuid as UUID,
    summary,
    customTitle,
    tag,
    fileHistorySnapshots: fileHistorySnapshots,
    attributionSnapshots: attributionSnapshots,
    contentReplacements,
    gitBranch: lastMessage.gitBranch,
    projectPath: firstMessage.cwd,
  }
}

// ─── Pre-boundary metadata scanning ─────────────────────────────────────────

/**
 * Metadata entry types that can appear before a compact boundary but must
 * still be loaded (they're session-scoped, not message-scoped).
 * Kept as raw JSON string markers for cheap line filtering during streaming.
 */
const METADATA_TYPE_MARKERS = [
  '"type":"summary"',
  '"type":"custom-title"',
  '"type":"tag"',
  '"type":"agent-name"',
  '"type":"agent-color"',
  '"type":"agent-setting"',
  '"type":"mode"',
  '"type":"worktree-state"',
  '"type":"pr-link"',
]
const METADATA_MARKER_BUFS = METADATA_TYPE_MARKERS.map(m => Buffer.from(m))
// Longest marker is 22 bytes; +1 for leading `{` = 23.
const METADATA_PREFIX_BOUND = 25

// null = carry spans whole chunk. Skips concat when carry provably isn't
// a metadata line (markers sit at byte 1 after `{`).
function resolveMetadataBuf(
  carry: Buffer | null,
  chunkBuf: Buffer,
): Buffer | null {
  if (carry === null || carry.length === 0) return chunkBuf
  if (carry.length < METADATA_PREFIX_BOUND) {
    return Buffer.concat([carry, chunkBuf])
  }
  if (carry[0] === 0x7b /* { */) {
    for (const m of METADATA_MARKER_BUFS) {
      if (carry.compare(m, 0, m.length, 1, 1 + m.length) === 0) {
        return Buffer.concat([carry, chunkBuf])
      }
    }
  }
  const firstNl = chunkBuf.indexOf(0x0a)
  return firstNl === -1 ? null : chunkBuf.subarray(firstNl + 1)
}

/**
 * Lightweight forward scan of [0, endOffset) collecting only metadata-entry lines.
 * Uses raw Buffer chunks and byte-level marker matching — no readline, no per-line
 * string conversion for the ~99% of lines that are message content.
 *
 * Fast path: if a chunk contains zero markers (the common case — metadata entries
 * are <50 per session), the entire chunk is skipped without line splitting.
 */
async function scanPreBoundaryMetadata(
  filePath: string,
  endOffset: number,
): Promise<string[]> {
  const { createReadStream } = await import('fs')
  const NEWLINE = 0x0a

  const stream = createReadStream(filePath, { end: endOffset - 1 })
  const metadataLines: string[] = []
  let carry: Buffer | null = null

  for await (const chunk of stream) {
    const chunkBuf = chunk as Buffer
    const buf = resolveMetadataBuf(carry, chunkBuf)
    if (buf === null) {
      carry = null
      continue
    }

    // Fast path: most chunks contain zero metadata markers. Skip line splitting.
    let hasAnyMarker = false
    for (const m of METADATA_MARKER_BUFS) {
      if (buf.includes(m)) {
        hasAnyMarker = true
        break
      }
    }

    if (hasAnyMarker) {
      let lineStart = 0
      let nl = buf.indexOf(NEWLINE)
      while (nl !== -1) {
        // Bounded marker check: only look within this line's byte range
        for (const m of METADATA_MARKER_BUFS) {
          const mIdx = buf.indexOf(m, lineStart)
          if (mIdx !== -1 && mIdx < nl) {
            metadataLines.push(buf.toString('utf-8', lineStart, nl))
            break
          }
        }
        lineStart = nl + 1
        nl = buf.indexOf(NEWLINE, lineStart)
      }
      carry = buf.subarray(lineStart)
    } else {
      // No markers in this chunk — just preserve the incomplete trailing line
      const lastNl = buf.lastIndexOf(NEWLINE)
      carry = lastNl >= 0 ? buf.subarray(lastNl + 1) : buf
    }

    // Guard against quadratic carry growth for pathological huge lines
    // (e.g., a 10 MB tool-output line with no newline). Real metadata entries
    // are <1 KB, so if carry exceeds this we're mid-message-content — drop it.
    if (carry.length > 64 * 1024) carry = null
  }

  // Final incomplete line (no trailing newline at endOffset)
  if (carry !== null && carry.length > 0) {
    for (const m of METADATA_MARKER_BUFS) {
      if (carry.includes(m)) {
        metadataLines.push(carry.toString('utf-8'))
        break
      }
    }
  }

  return metadataLines
}

// ─── walkChainBeforeParse ────────────────────────────────────────────────────

/**
 * Disambiguate multiple `"uuid":"<36>","timestamp":"` matches in one line by
 * finding the one at JSON nesting depth 1. String-aware brace counter:
 * `{`/`}` inside string values don't count; `\"` and `\\` inside strings are
 * handled. Candidates is sorted ascending (the scan loop produces them in
 * byte order). Returns the first depth-1 candidate, or the last candidate if
 * none are at depth 1 (shouldn't happen for well-formed JSONL — depth-1 is
 * where the top-level object's fields live).
 *
 * Only called when ≥2 suffix matches exist (agent_progress with a nested
 * Message, or mcpMeta with a coincidentally-suffixed object). Cost is
 * O(max(candidates) - lineStart) — one forward byte pass, stopping at the
 * first depth-1 hit.
 */
function pickDepthOneUuidCandidate(
  buf: Buffer,
  lineStart: number,
  candidates: number[],
): number {
  const QUOTE = 0x22
  const BACKSLASH = 0x5c
  const OPEN_BRACE = 0x7b
  const CLOSE_BRACE = 0x7d
  let depth = 0
  let inString = false
  let escapeNext = false
  let ci = 0
  for (let i = lineStart; ci < candidates.length; i++) {
    if (i === candidates[ci]) {
      if (depth === 1 && !inString) return candidates[ci]!
      ci++
    }
    const b = buf[i]!
    if (escapeNext) {
      escapeNext = false
    } else if (inString) {
      if (b === BACKSLASH) escapeNext = true
      else if (b === QUOTE) inString = false
    } else if (b === QUOTE) inString = true
    else if (b === OPEN_BRACE) depth++
    else if (b === CLOSE_BRACE) depth--
  }
  return candidates.at(-1)!
}

/**
 * Byte-level pre-filter that excises dead fork branches before parseJSONL.
 *
 * Every rewind/ctrl-z leaves an orphaned chain branch in the append-only
 * JSONL forever. buildConversationChain walks parentUuid from the latest leaf
 * and discards everything else, but by then parseJSONL has already paid to
 * JSON.parse all of it.
 */
function walkChainBeforeParse(buf: Buffer): Buffer {
  const NEWLINE = 0x0a
  const OPEN_BRACE = 0x7b
  const QUOTE = 0x22
  const PARENT_PREFIX = Buffer.from('{"parentUuid":')
  const UUID_KEY = Buffer.from('"uuid":"')
  const SIDECHAIN_TRUE = Buffer.from('"isSidechain":true')
  const UUID_LEN = 36
  const TS_SUFFIX = Buffer.from('","timestamp":"')
  const TS_SUFFIX_LEN = TS_SUFFIX.length
  const PREFIX_LEN = PARENT_PREFIX.length
  const KEY_LEN = UUID_KEY.length

  // Stride-3 flat index of transcript messages: [lineStart, lineEnd, parentStart].
  // parentStart is the byte offset of the parent uuid's first char, or -1 for null.
  // Metadata lines (summary, mode, file-history-snapshot, etc.) go in metaRanges
  // unfiltered - they lack the parentUuid prefix and downstream needs all of them.
  const msgIdx: number[] = []
  const metaRanges: number[] = []
  const uuidToSlot = new Map<string, number>()

  let pos = 0
  const len = buf.length
  while (pos < len) {
    const nl = buf.indexOf(NEWLINE, pos)
    const lineEnd = nl === -1 ? len : nl + 1
    if (
      lineEnd - pos > PREFIX_LEN &&
      buf[pos] === OPEN_BRACE &&
      buf.compare(PARENT_PREFIX, 0, PREFIX_LEN, pos, pos + PREFIX_LEN) === 0
    ) {
      // `{"parentUuid":null,` or `{"parentUuid":"<36 chars>",`
      const parentStart =
        buf[pos + PREFIX_LEN] === QUOTE ? pos + PREFIX_LEN + 1 : -1
      let firstAny = -1
      let suffix0 = -1
      let suffixN: number[] | undefined
      let from = pos
      for (;;) {
        const next = buf.indexOf(UUID_KEY, from)
        if (next < 0 || next >= lineEnd) break
        if (firstAny < 0) firstAny = next
        const after = next + KEY_LEN + UUID_LEN
        if (
          after + TS_SUFFIX_LEN <= lineEnd &&
          buf.compare(
            TS_SUFFIX,
            0,
            TS_SUFFIX_LEN,
            after,
            after + TS_SUFFIX_LEN,
          ) === 0
        ) {
          if (suffix0 < 0) suffix0 = next
          else (suffixN ??= [suffix0]).push(next)
        }
        from = next + KEY_LEN
      }
      const uk = suffixN
        ? pickDepthOneUuidCandidate(buf, pos, suffixN)
        : suffix0 >= 0
          ? suffix0
          : firstAny
      if (uk >= 0) {
        const uuidStart = uk + KEY_LEN
        // UUIDs are pure ASCII so latin1 avoids UTF-8 decode overhead.
        const uuid = buf.toString('latin1', uuidStart, uuidStart + UUID_LEN)
        uuidToSlot.set(uuid, msgIdx.length)
        msgIdx.push(pos, lineEnd, parentStart)
      } else {
        metaRanges.push(pos, lineEnd)
      }
    } else {
      metaRanges.push(pos, lineEnd)
    }
    pos = lineEnd
  }

  // Leaf = last non-sidechain entry.
  let leafSlot = -1
  for (let i = msgIdx.length - 3; i >= 0; i -= 3) {
    const sc = buf.indexOf(SIDECHAIN_TRUE, msgIdx[i]!)
    if (sc === -1 || sc >= msgIdx[i + 1]!) {
      leafSlot = i
      break
    }
  }
  if (leafSlot < 0) return buf

  // Walk parentUuid to root.
  const seen = new Set<number>()
  const chain = new Set<number>()
  let chainBytes = 0
  let slot: number | undefined = leafSlot
  while (slot !== undefined) {
    if (seen.has(slot)) break
    seen.add(slot)
    chain.add(msgIdx[slot]!)
    chainBytes += msgIdx[slot + 1]! - msgIdx[slot]!
    const parentStart = msgIdx[slot + 2]!
    if (parentStart < 0) break
    const parent = buf.toString('latin1', parentStart, parentStart + UUID_LEN)
    slot = uuidToSlot.get(parent)
  }

  // Gate on bytes: only stitch if we would drop at least half the buffer.
  if (len - chainBytes < len >> 1) return buf

  // Merge chain entries with metadata in original file order.
  const parts: Buffer[] = []
  let m = 0
  for (let i = 0; i < msgIdx.length; i += 3) {
    const start = msgIdx[i]!
    while (m < metaRanges.length && metaRanges[m]! < start) {
      parts.push(buf.subarray(metaRanges[m]!, metaRanges[m + 1]!))
      m += 2
    }
    if (chain.has(start)) {
      parts.push(buf.subarray(start, msgIdx[i + 1]!))
    }
  }
  while (m < metaRanges.length) {
    parts.push(buf.subarray(metaRanges[m]!, metaRanges[m + 1]!))
    m += 2
  }
  return Buffer.concat(parts)
}

// ─── loadTranscriptFile ──────────────────────────────────────────────────────

/**
 * Loads all messages, summaries, and file history snapshots from a transcript file.
 * Returns the messages, summaries, custom titles, tags, file history snapshots, and attribution snapshots.
 */
export async function loadTranscriptFile(
  filePath: string,
  opts?: { keepAllLeaves?: boolean; sourceBuffer?: Buffer },
): Promise<{
  messages: Map<string, TranscriptMessage>
  summaries: Map<string, string>
  customTitles: Map<string, string>
  tags: Map<string, string>
  agentNames: Map<string, string>
  agentColors: Map<string, string>
  agentSettings: Map<string, string>
  prNumbers: Map<string, number>
  prUrls: Map<string, string>
  prRepositories: Map<string, string>
  modes: Map<string, string>
  worktreeStates: Map<string, PersistedWorktreeSession | null>
  fileHistorySnapshots: Map<string, FileHistorySnapshotMessage>
  attributionSnapshots: Map<string, AttributionSnapshotMessage>
  contentReplacements: Map<string, ContentReplacementRecord[]>
  agentContentReplacements: Map<AgentId, ContentReplacementRecord[]>
  contextCollapseCommits: ContextCollapseCommitEntry[]
  contextCollapseSnapshot: ContextCollapseSnapshotEntry | undefined
  leafUuids: Set<string>
}> {
  const messages = new Map<string, TranscriptMessage>()
  const summaries = new Map<string, string>()
  const customTitles = new Map<string, string>()
  const tags = new Map<string, string>()
  const agentNames = new Map<string, string>()
  const agentColors = new Map<string, string>()
  const agentSettings = new Map<string, string>()
  const prNumbers = new Map<string, number>()
  const prUrls = new Map<string, string>()
  const prRepositories = new Map<string, string>()
  const modes = new Map<string, string>()
  const worktreeStates = new Map<string, PersistedWorktreeSession | null>()
  const fileHistorySnapshots = new Map<string, FileHistorySnapshotMessage>()
  const attributionSnapshots = new Map<string, AttributionSnapshotMessage>()
  const contentReplacements = new Map<string, ContentReplacementRecord[]>()
  const agentContentReplacements = new Map<
    AgentId,
    ContentReplacementRecord[]
  >()
  // Array, not Map — commit order matters (nested collapses).
  const contextCollapseCommits: ContextCollapseCommitEntry[] = []
  // Last-wins — later entries supersede.
  let contextCollapseSnapshot: ContextCollapseSnapshotEntry | undefined

  try {
    // Mutation callers can pin parsing to one already-read file snapshot.
    // Ordinary resume/read callers omit this and retain the established
    // compact-boundary streaming optimization below.
    let buf: Buffer | null =
      opts?.sourceBuffer === undefined
        ? null
        : Buffer.from(opts.sourceBuffer)
    let metadataLines: string[] | null = null
    let hasPreservedSegment = false
    if (
      buf === null &&
      !isEnvTruthy(process.env.CRABCODE_DISABLE_PRECOMPACT_SKIP)
    ) {
      const { size } = await stat(filePath)
      if (size > SKIP_PRECOMPACT_THRESHOLD) {
        const scan = await readTranscriptForLoad(filePath, size)
        buf = scan.postBoundaryBuf
        hasPreservedSegment = scan.hasPreservedSegment
        if (scan.boundaryStartOffset > 0) {
          metadataLines = await scanPreBoundaryMetadata(
            filePath,
            scan.boundaryStartOffset,
          )
        }
      }
    }
    buf ??= await readFile(filePath)
    if (
      !opts?.keepAllLeaves &&
      !hasPreservedSegment &&
      !isEnvTruthy(process.env.CRABCODE_DISABLE_PRECOMPACT_SKIP) &&
      buf.length > SKIP_PRECOMPACT_THRESHOLD
    ) {
      buf = walkChainBeforeParse(buf)
    }

    // First pass: process metadata-only lines collected during the boundary scan.
    if (metadataLines && metadataLines.length > 0) {
      const metaEntries = parseJSONL<Entry>(
        Buffer.from(metadataLines.join('\n')),
      )
      for (const entry of metaEntries) {
        if (entry.type === 'summary' && entry.leafUuid) {
          summaries.set(entry.leafUuid, entry.summary)
        } else if (entry.type === 'custom-title' && entry.sessionId) {
          customTitles.set(entry.sessionId, entry.customTitle)
        } else if (entry.type === 'tag' && entry.sessionId) {
          tags.set(entry.sessionId, entry.tag)
        } else if (entry.type === 'agent-name' && entry.sessionId) {
          agentNames.set(entry.sessionId, entry.agentName)
        } else if (entry.type === 'agent-color' && entry.sessionId) {
          agentColors.set(entry.sessionId, entry.agentColor)
        } else if (entry.type === 'agent-setting' && entry.sessionId) {
          agentSettings.set(entry.sessionId, entry.agentSetting)
        } else if (entry.type === 'mode' && entry.sessionId) {
          modes.set(entry.sessionId, entry.mode)
        } else if (entry.type === 'worktree-state' && entry.sessionId) {
          worktreeStates.set(entry.sessionId, entry.worktreeSession)
        } else if (entry.type === 'pr-link' && entry.sessionId) {
          prNumbers.set(entry.sessionId, entry.prNumber)
          prUrls.set(entry.sessionId, entry.prUrl)
          prRepositories.set(entry.sessionId, entry.prRepository)
        }
      }
    }

    const entries = parseJSONL<Entry>(buf)

    // Bridge map for legacy progress entries
    const progressBridge = new Map<string, string | null>()

    for (const entry of entries) {
      if (isLegacyProgressEntry(entry)) {
        const parent = entry.parentUuid
        progressBridge.set(
          entry.uuid,
          parent && progressBridge.has(parent)
            ? (progressBridge.get(parent) ?? null)
            : parent,
        )
        continue
      }
      if (isTranscriptMessage(entry)) {
        if (entry.parentUuid && progressBridge.has(entry.parentUuid)) {
          entry.parentUuid = (progressBridge.get(entry.parentUuid) ?? null) as UUID | null
        }
        messages.set(entry.uuid!, entry)
        if (isCompactBoundaryMessage(entry)) {
          contextCollapseCommits.length = 0
          contextCollapseSnapshot = undefined
        }
      } else if (entry.type === 'summary' && entry.leafUuid) {
        summaries.set(entry.leafUuid, entry.summary)
      } else if (entry.type === 'custom-title' && entry.sessionId) {
        customTitles.set(entry.sessionId, entry.customTitle)
      } else if (entry.type === 'tag' && entry.sessionId) {
        tags.set(entry.sessionId, entry.tag)
      } else if (entry.type === 'agent-name' && entry.sessionId) {
        agentNames.set(entry.sessionId, entry.agentName)
      } else if (entry.type === 'agent-color' && entry.sessionId) {
        agentColors.set(entry.sessionId, entry.agentColor)
      } else if (entry.type === 'agent-setting' && entry.sessionId) {
        agentSettings.set(entry.sessionId, entry.agentSetting)
      } else if (entry.type === 'mode' && entry.sessionId) {
        modes.set(entry.sessionId, entry.mode)
      } else if (entry.type === 'worktree-state' && entry.sessionId) {
        worktreeStates.set(entry.sessionId, entry.worktreeSession)
      } else if (entry.type === 'pr-link' && entry.sessionId) {
        prNumbers.set(entry.sessionId, entry.prNumber)
        prUrls.set(entry.sessionId, entry.prUrl)
        prRepositories.set(entry.sessionId, entry.prRepository)
      } else if (entry.type === 'file-history-snapshot') {
        fileHistorySnapshots.set(entry.messageId, entry)
      } else if (entry.type === 'attribution-snapshot') {
        attributionSnapshots.set(entry.messageId, entry)
      } else if (entry.type === 'content-replacement') {
        if (entry.agentId) {
          const existing = agentContentReplacements.get(entry.agentId) ?? []
          agentContentReplacements.set(entry.agentId, existing)
          existing.push(...entry.replacements)
        } else {
          const existing = contentReplacements.get(entry.sessionId) ?? []
          contentReplacements.set(entry.sessionId, existing)
          existing.push(...entry.replacements)
        }
      } else if (entry.type === 'marble-origami-commit') {
        contextCollapseCommits.push(entry)
      } else if (entry.type === 'marble-origami-snapshot') {
        contextCollapseSnapshot = entry
      }
    }
  } catch {
    // File doesn't exist or can't be read
  }

  applyPreservedSegmentRelinks(messages)
  applySnipRemovals(messages)

  // Compute leaf UUIDs once at load time
  const allMessages = [...messages.values()]

  // Standard leaf computation using parent relationships
  const parentUuids = new Set<string>(
    allMessages
      .map(msg => msg.parentUuid)
      .filter((uuid): uuid is NonNullable<typeof uuid> => uuid !== null),
  )

  // Find all terminal messages (messages with no children)
  const terminalMessages = allMessages.filter(msg => !parentUuids.has(msg.uuid!))

  const leafUuids = new Set<string>()
  let hasCycle = false

  if (getFeatureValue_CACHED_MAY_BE_STALE('tengu_pebble_leaf_prune', false)) {
    const hasUserAssistantChild = new Set<string>()
    for (const msg of allMessages) {
      if (msg.parentUuid && (msg.type === 'user' || msg.type === 'assistant')) {
        hasUserAssistantChild.add(msg.parentUuid)
      }
    }

    for (const terminal of terminalMessages) {
      const seen = new Set<string>()
      let current: TranscriptMessage | undefined = terminal
      while (current) {
        if (seen.has(current.uuid!)) {
          hasCycle = true
          break
        }
        seen.add(current.uuid!)
        if (current.type === 'user' || current.type === 'assistant') {
          if (!hasUserAssistantChild.has(current.uuid!)) {
            leafUuids.add(current.uuid!)
          }
          break
        }
        current = current.parentUuid
          ? messages.get(current.parentUuid)
          : undefined
      }
    }
  } else {
    for (const terminal of terminalMessages) {
      const seen = new Set<string>()
      let current: TranscriptMessage | undefined = terminal
      while (current) {
        if (seen.has(current.uuid!)) {
          hasCycle = true
          break
        }
        seen.add(current.uuid!)
        if (current.type === 'user' || current.type === 'assistant') {
          leafUuids.add(current.uuid!)
          break
        }
        current = current.parentUuid
          ? messages.get(current.parentUuid)
          : undefined
      }
    }
  }

  if (hasCycle) {
    logEvent('tengu_transcript_parent_cycle', {})
  }

  return {
    messages,
    summaries,
    customTitles,
    tags,
    agentNames,
    agentColors,
    agentSettings,
    prNumbers,
    prUrls,
    prRepositories,
    modes,
    worktreeStates,
    fileHistorySnapshots,
    attributionSnapshots,
    contentReplacements,
    agentContentReplacements,
    contextCollapseCommits,
    contextCollapseSnapshot,
    leafUuids,
  }
}

// ─── loadTranscriptFromFile (JSON + JSONL) ───────────────────────────────────

/**
 * Loads a transcript from a JSON or JSONL file and converts it to LogOption format
 * @param filePath Path to the transcript file (.json or .jsonl)
 * @returns LogOption containing the transcript messages
 * @throws Error if file doesn't exist or contains invalid data
 */
export async function loadTranscriptFromFile(
  filePath: string,
): Promise<LogOption> {
  if (filePath.endsWith('.jsonl')) {
    const {
      messages,
      summaries,
      customTitles,
      tags,
      fileHistorySnapshots,
      attributionSnapshots,
      contextCollapseCommits,
      contextCollapseSnapshot,
      leafUuids,
      contentReplacements,
      worktreeStates,
    } = await loadTranscriptFile(filePath)

    if (messages.size === 0) {
      throw new Error('No messages found in JSONL file')
    }

    // Find the most recent leaf message using pre-computed leaf UUIDs
    const leafMessage = findLatestMessage(messages.values(), msg =>
      leafUuids.has(msg.uuid!),
    )

    if (!leafMessage) {
      throw new Error('No valid conversation chain found in JSONL file')
    }

    // Build the conversation chain backwards from leaf to root
    const transcript = buildConversationChain(messages, leafMessage)

    const summary = summaries.get(leafMessage.uuid!)
    const customTitle = customTitles.get(leafMessage.sessionId as UUID)
    const tag = tags.get(leafMessage.sessionId as UUID)
    const sessionId = leafMessage.sessionId as UUID
    return {
      ...convertToLogOption(
        transcript,
        0,
        summary,
        customTitle,
        buildFileHistorySnapshotChain(fileHistorySnapshots, transcript),
        tag,
        filePath,
        buildAttributionSnapshotChain(attributionSnapshots, transcript),
        undefined,
        contentReplacements.get(sessionId) ?? [],
      ),
      contextCollapseCommits: contextCollapseCommits.filter(
        e => e.sessionId === sessionId,
      ),
      contextCollapseSnapshot:
        contextCollapseSnapshot?.sessionId === sessionId
          ? contextCollapseSnapshot
          : undefined,
      worktreeSession: worktreeStates.has(sessionId)
        ? worktreeStates.get(sessionId)
        : undefined,
    }
  }

  // json log files
  const content = await readFile(filePath, { encoding: 'utf-8' })
  let parsed: unknown

  try {
    parsed = jsonParse(content)
  } catch (error) {
    throw new Error(`Invalid JSON in transcript file: ${error}`)
  }

  let msgArr: TranscriptMessage[]

  if (Array.isArray(parsed)) {
    msgArr = parsed
  } else if (parsed && typeof parsed === 'object' && 'messages' in parsed) {
    if (!Array.isArray(parsed.messages)) {
      throw new Error('Transcript messages must be an array')
    }
    msgArr = parsed.messages
  } else {
    throw new Error(
      'Transcript must be an array of messages or an object with a messages array',
    )
  }

  return convertToLogOption(
    msgArr,
    0,
    undefined,
    undefined,
    undefined,
    undefined,
    filePath,
  )
}

// ─── Lite metadata extraction (for session listing) ──────────────────────────

export type LiteMetadata = {
  firstPrompt: string
  gitBranch?: string
  isSidechain: boolean
  projectPath?: string
  teamName?: string
  customTitle?: string
  summary?: string
  tag?: string
  agentSetting?: string
  prNumber?: number
  prUrl?: string
  prRepository?: string
}

/**
 * Reads the first and last ~64KB of a JSONL file and extracts lite metadata.
 *
 * Head (first 64KB): isSidechain, projectPath, teamName, firstPrompt.
 * Tail (last 64KB): customTitle, tag, PR link, latest gitBranch.
 *
 * Accepts a shared buffer to avoid per-file allocation overhead.
 */
export async function readLiteMetadata(
  filePath: string,
  fileSize: number,
  buf: Buffer,
): Promise<LiteMetadata> {
  const { head, tail } = await readHeadAndTail(filePath, fileSize, buf)
  if (!head) return { firstPrompt: '', isSidechain: false }

  // Extract stable metadata from the first line via string search.
  const isSidechain =
    head.includes('"isSidechain":true') || head.includes('"isSidechain": true')
  const projectPath = extractJsonStringField(head, 'cwd')
  const teamName = extractJsonStringField(head, 'teamName')
  const agentSetting = extractJsonStringField(head, 'agentSetting')

  const firstPrompt =
    extractLastJsonStringField(tail, 'lastPrompt') ||
    extractFirstPromptFromChunk(head) ||
    extractJsonStringFieldPrefix(head, 'content', 200) ||
    extractJsonStringFieldPrefix(head, 'text', 200) ||
    ''

  const customTitle =
    extractLastJsonStringField(tail, 'customTitle') ??
    extractLastJsonStringField(head, 'customTitle') ??
    extractLastJsonStringField(tail, 'aiTitle') ??
    extractLastJsonStringField(head, 'aiTitle')
  const summary = extractLastJsonStringField(tail, 'summary')
  const tag = extractLastJsonStringField(tail, 'tag')
  const gitBranch =
    extractLastJsonStringField(tail, 'gitBranch') ??
    extractJsonStringField(head, 'gitBranch')

  // PR link fields — prNumber is a number not a string, so try both
  const prUrl = extractLastJsonStringField(tail, 'prUrl')
  const prRepository = extractLastJsonStringField(tail, 'prRepository')
  let prNumber: number | undefined
  const prNumStr = extractLastJsonStringField(tail, 'prNumber')
  if (prNumStr) {
    prNumber = parseInt(prNumStr, 10) || undefined
  }
  if (!prNumber) {
    const prNumMatch = tail.lastIndexOf('"prNumber":')
    if (prNumMatch >= 0) {
      const afterColon = tail.slice(prNumMatch + 11, prNumMatch + 25)
      const num = parseInt(afterColon.trim(), 10)
      if (num > 0) prNumber = num
    }
  }

  return {
    firstPrompt,
    gitBranch,
    isSidechain,
    projectPath,
    teamName,
    customTitle,
    summary,
    tag,
    agentSetting,
    prNumber,
    prUrl,
    prRepository,
  }
}

/**
 * Scans a chunk of text for the first meaningful user prompt.
 */
function extractFirstPromptFromChunk(chunk: string): string {
  let start = 0
  let hasTickMessages = false
  let firstCommandFallback = ''
  while (start < chunk.length) {
    const newlineIdx = chunk.indexOf('\n', start)
    const line =
      newlineIdx >= 0 ? chunk.slice(start, newlineIdx) : chunk.slice(start)
    start = newlineIdx >= 0 ? newlineIdx + 1 : chunk.length

    if (!line.includes('"type":"user"') && !line.includes('"type": "user"')) {
      continue
    }
    if (line.includes('"tool_result"')) continue
    if (line.includes('"isMeta":true') || line.includes('"isMeta": true'))
      continue

    try {
      const entry = jsonParse(line) as Record<string, unknown>
      if (entry.type !== 'user') continue

      const message = entry.message as Record<string, unknown> | undefined
      if (!message) continue

      const content = message.content
      const texts: string[] = []
      if (typeof content === 'string') {
        texts.push(content)
      } else if (Array.isArray(content)) {
        for (const block of content) {
          const b = block as Record<string, unknown>
          if (b.type === 'text' && typeof b.text === 'string') {
            texts.push(b.text as string)
          }
        }
      }

      for (const text of texts) {
        if (!text) continue

        let result = text.replace(/\n/g, ' ').trim()

        const commandNameTag = extractTag(result, COMMAND_NAME_TAG)
        if (commandNameTag) {
          const name = commandNameTag.replace(/^\//, '')
          const commandArgs = extractTag(result, 'command-args')?.trim() || ''
          if (getActiveBuiltInCommandNames().has(name) || !commandArgs) {
            if (!firstCommandFallback) {
              firstCommandFallback = commandNameTag
            }
            continue
          }
          return commandArgs
            ? `${commandNameTag} ${commandArgs}`
            : commandNameTag
        }

        // Format bash input with ! prefix before the generic XML skip
        const bashInput = extractTag(result, 'bash-input')
        if (bashInput) return `! ${bashInput}`

        if (SKIP_FIRST_PROMPT_PATTERN.test(result)) {
          if (
            (feature('PROACTIVE') || feature('KAIROS')) &&
            result.startsWith(`<${TICK_TAG}>`)
          )
            hasTickMessages = true
          continue
        }
        if (result.length > 200) {
          result = result.slice(0, 200).trim() + '…'
        }
        return result
      }
    } catch {
      continue
    }
  }
  if (firstCommandFallback) return firstCommandFallback
  if ((feature('PROACTIVE') || feature('KAIROS')) && hasTickMessages)
    return 'Proactive session'
  return ''
}

/**
 * Like extractJsonStringField but returns the first `maxLen` characters of the
 * value even when the closing quote is missing (truncated buffer).
 */
function extractJsonStringFieldPrefix(
  text: string,
  key: string,
  maxLen: number,
): string {
  const patterns = [`"${key}":"`, `"${key}": "`]
  for (const pattern of patterns) {
    const idx = text.indexOf(pattern)
    if (idx < 0) continue

    const valueStart = idx + pattern.length
    let i = valueStart
    let collected = 0
    while (i < text.length && collected < maxLen) {
      if (text[i] === '\\') {
        i += 2 // skip escaped char
        collected++
        continue
      }
      if (text[i] === '"') break
      i++
      collected++
    }
    const raw = text.slice(valueStart, i)
    return raw.replace(/\\n/g, ' ').replace(/\\t/g, ' ').trim()
  }
  return ''
}
