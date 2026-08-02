/**
 * attachments-memory.ts -- Memory-related attachment logic.
 *
 * Split from attachments.ts (3997 lines) for maintainability. Contains all
 * memory file surfacing, prefetching, deduplication, and readFileState
 * management logic.
 */
import {
  logEvent,
} from 'src/services/analytics/index.js'
import type { ToolUseContext, ToolPermissionContext } from '../Tool.js'
import { readFileInRange } from './readFileInRange.js'
import {
  getManagedAndUserConditionalRules,
  getMemoryFilesForNestedDirectory,
  getConditionalRulesForCwdLevelDirectory,
  type MemoryFileInfo,
} from './crabcodemd.js'
import { dirname, parse, relative, resolve } from 'path'
import { getCwd } from 'src/utils/cwd.js'
import { logError } from './log.js'
import { isAbortError } from './errors.js'
import type { Message } from 'src/types/message.js'
import { FILE_READ_TOOL_NAME } from 'src/tools/FileReadTool/prompt.js'
import type { FileStateCache } from './fileStateCache.js'
import {
  createChildAbortController,
} from './abortController.js'
import { getOriginalCwd } from '../bootstrap/state.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../services/analytics/growthbook.js'
import {
  hasInstructionsLoadedHook,
  executeInstructionsLoadedHooks,
  type InstructionsMemoryType,
} from './hooks.js'
import { getUserMessageText } from './messages.js'
import { getAssistantMessageText } from './messages/envelope.js'
import { isHumanTurn } from './messagePredicates.js'
import { findRelevantMemories } from '../memdir/findRelevantMemories.js'
import { memoryAge, memoryFreshnessText } from '../memdir/memoryAge.js'
import { getAutoMemPath, isAutoMemoryEnabled } from '../memdir/paths.js'
import { getAgentMemoryDir } from '../tools/AgentTool/agentMemory.js'
import type { AgentDefinition } from '../tools/AgentTool/loadAgentsDir.js'
import { pathInAllowedWorkingPath } from './permissions/filesystem.js'

import type { Attachment, MemoryPrefetch } from './attachments-types.js'
import { RELEVANT_MEMORIES_CONFIG } from './attachments-types.js'
import { extractAgentMentions } from './attachments-parse.js'
import { collectRecentSuccessfulTools } from './attachments-parse.js'

const MAX_MEMORY_LINES = 200
// Line cap alone doesn't bound size (200 x 500-char lines = 100KB).  The
// surfacer injects up to 5 files per turn via <system-reminder>, bypassing
// the per-message tool-result budget, so a tight per-file byte cap keeps
// aggregate injection bounded (5 x 4KB = 20KB/turn).  Enforced via
// readFileInRange's truncateOnByteLimit option.  Truncation means the
// most-relevant memory still surfaces: the frontmatter + opening context
// is usually what matters.
const MAX_MEMORY_BYTES = 4096
// W-MEMORY-KB-UPLIFT P2 — recall enrichment: how much of the last assistant
// text tail rides along with the user prompt as recall-query context.
const RECALL_ENRICH_ASSISTANT_TAIL_CHARS = 200

/**
 * Converts memory files to attachments, filtering out already-loaded files.
 *
 * @param memoryFiles The memory files to convert
 * @param toolUseContext The tool use context (for tracking loaded files)
 * @returns Array of nested memory attachments
 */
function isInstructionsMemoryType(
  type: MemoryFileInfo['type'],
): type is InstructionsMemoryType {
  return (
    type === 'User' ||
    type === 'Project' ||
    type === 'Local' ||
    type === 'Managed'
  )
}

/** Exported for testing -- regression guard for LRU-eviction re-injection. */
export function memoryFilesToAttachments(
  memoryFiles: MemoryFileInfo[],
  toolUseContext: ToolUseContext,
  triggerFilePath?: string,
): Attachment[] {
  const attachments: Attachment[] = []
  const shouldFireHook = hasInstructionsLoadedHook()

  for (const memoryFile of memoryFiles) {
    // Dedup: loadedNestedMemoryPaths is a non-evicting Set; readFileState
    // is a 100-entry LRU that drops entries in busy sessions, so relying
    // on it alone re-injects the same CRABCODE.md on every eviction cycle.
    if (toolUseContext.loadedNestedMemoryPaths?.has(memoryFile.path)) {
      continue
    }
    if (!toolUseContext.readFileState.has(memoryFile.path)) {
      attachments.push({
        type: 'nested_memory',
        path: memoryFile.path,
        content: memoryFile,
        displayPath: relative(getCwd(), memoryFile.path),
      })
      toolUseContext.loadedNestedMemoryPaths?.add(memoryFile.path)

      // Mark as loaded in readFileState -- this provides cross-function and
      // cross-turn dedup via the .has() check above.
      //
      // When the injected content doesn't match disk (stripped HTML comments,
      // stripped frontmatter, truncated MEMORY.md), cache the RAW disk bytes
      // with `isPartialView: true`. Edit/Write see the flag and require a real
      // Read first; getChangedFiles sees real content + undefined offset/limit
      // so mid-session change detection still works.
      toolUseContext.readFileState.set(memoryFile.path, {
        content: memoryFile.contentDiffersFromDisk
          ? (memoryFile.rawContent ?? memoryFile.content)
          : memoryFile.content,
        timestamp: Date.now(),
        offset: undefined,
        limit: undefined,
        isPartialView: memoryFile.contentDiffersFromDisk,
      })


      // Fire InstructionsLoaded hook for audit/observability (fire-and-forget)
      if (shouldFireHook && isInstructionsMemoryType(memoryFile.type)) {
        const loadReason = memoryFile.globs
          ? 'path_glob_match'
          : memoryFile.parent
            ? 'include'
            : 'nested_traversal'
        void executeInstructionsLoadedHooks(
          memoryFile.path,
          memoryFile.type,
          loadReason,
          {
            globs: memoryFile.globs,
            triggerFilePath,
            parentFilePath: memoryFile.parent,
          },
        )
      }
    }
  }

  return attachments
}

/**
 * Loads nested memory files for a given file path and returns them as attachments.
 * This function performs directory traversal to find CRABCODE.md files and conditional rules
 * that apply to the target file path.
 *
 * Processing order (must be preserved):
 * 1. Managed/User conditional rules matching targetPath
 * 2. Nested directories (CWD -> target): CRABCODE.md + unconditional + conditional rules
 * 3. CWD-level directories (root -> CWD): conditional rules only
 *
 * @param filePath The file path to get nested memory files for
 * @param toolUseContext The tool use context
 * @param appState The app state containing tool permission context
 * @returns Array of nested memory attachments
 */
async function getNestedMemoryAttachmentsForFile(
  filePath: string,
  toolUseContext: ToolUseContext,
  appState: { toolPermissionContext: ToolPermissionContext },
): Promise<Attachment[]> {
  const attachments: Attachment[] = []

  try {
    // Early return if path is not in allowed working path
    if (!pathInAllowedWorkingPath(filePath, appState.toolPermissionContext)) {
      return attachments
    }

    const processedPaths = new Set<string>()
    const originalCwd = getOriginalCwd()

    // Phase 1: Process Managed and User conditional rules
    const managedUserRules = await getManagedAndUserConditionalRules(
      filePath,
      processedPaths,
    )
    attachments.push(
      ...memoryFilesToAttachments(managedUserRules, toolUseContext, filePath),
    )

    // Phase 2: Get directories to process
    const { nestedDirs, cwdLevelDirs } = getDirectoriesToProcess(
      filePath,
      originalCwd,
    )

    const skipProjectLevel = getFeatureValue_CACHED_MAY_BE_STALE(
      'tengu_paper_halyard',
      false,
    )

    // Phase 3: Process nested directories (CWD -> target)
    // Each directory gets: CRABCODE.md + unconditional rules + conditional rules
    for (const dir of nestedDirs) {
      const memoryFiles = (
        await getMemoryFilesForNestedDirectory(dir, filePath, processedPaths)
      ).filter(
        f => !skipProjectLevel || (f.type !== 'Project' && f.type !== 'Local'),
      )
      attachments.push(
        ...memoryFilesToAttachments(memoryFiles, toolUseContext, filePath),
      )
    }

    // Phase 4: Process CWD-level directories (root -> CWD)
    // Only conditional rules (unconditional rules are already loaded eagerly)
    for (const dir of cwdLevelDirs) {
      const conditionalRules = (
        await getConditionalRulesForCwdLevelDirectory(
          dir,
          filePath,
          processedPaths,
        )
      ).filter(
        f => !skipProjectLevel || (f.type !== 'Project' && f.type !== 'Local'),
      )
      attachments.push(
        ...memoryFilesToAttachments(conditionalRules, toolUseContext, filePath),
      )
    }
  } catch (error) {
    logError(error)
  }

  return attachments
}

/**
 * Computes the directories to process for nested memory file loading.
 * Returns two lists:
 * - nestedDirs: Directories between CWD and targetPath (processed for CRABCODE.md + all rules)
 * - cwdLevelDirs: Directories from root to CWD (processed for conditional rules only)
 *
 * @param targetPath The target file path
 * @param originalCwd The original current working directory
 * @returns Object with nestedDirs and cwdLevelDirs arrays, both ordered from parent to child
 */
export function getDirectoriesToProcess(
  targetPath: string,
  originalCwd: string,
): { nestedDirs: string[]; cwdLevelDirs: string[] } {
  // Build list of directories from original CWD to targetPath's directory
  const targetDir = dirname(resolve(targetPath))
  const nestedDirs: string[] = []
  let currentDir = targetDir

  // Walk up from target directory to original CWD
  while (currentDir !== originalCwd && currentDir !== parse(currentDir).root) {
    if (currentDir.startsWith(originalCwd)) {
      nestedDirs.push(currentDir)
    }
    currentDir = dirname(currentDir)
  }

  // Reverse to get order from CWD down to target
  nestedDirs.reverse()

  // Build list of directories from root to CWD (for conditional rules only)
  const cwdLevelDirs: string[] = []
  currentDir = originalCwd

  while (currentDir !== parse(currentDir).root) {
    cwdLevelDirs.push(currentDir)
    currentDir = dirname(currentDir)
  }

  // Reverse to get order from root to CWD
  cwdLevelDirs.reverse()

  return { nestedDirs, cwdLevelDirs }
}

/**
 * Processes paths that need nested memory attachments and checks for nested CRABCODE.md files
 * Uses nestedMemoryAttachmentTriggers field from ToolUseContext
 */
export async function getNestedMemoryAttachments(
  toolUseContext: ToolUseContext,
): Promise<Attachment[]> {
  // Check triggers first -- getAppState() waits for a React render cycle,
  // and the common case is an empty trigger set.
  if (
    !toolUseContext.nestedMemoryAttachmentTriggers ||
    toolUseContext.nestedMemoryAttachmentTriggers.size === 0
  ) {
    return []
  }

  const appState = toolUseContext.getAppState()
  const attachments: Attachment[] = []

  for (const filePath of toolUseContext.nestedMemoryAttachmentTriggers) {
    const nestedAttachments = await getNestedMemoryAttachmentsForFile(
      filePath,
      toolUseContext,
      appState,
    )
    attachments.push(...nestedAttachments)
  }

  toolUseContext.nestedMemoryAttachmentTriggers.clear()

  return attachments
}

async function getRelevantMemoryAttachments(
  input: string,
  recallQuery: string,
  agents: AgentDefinition[],
  readFileState: FileStateCache,
  recentTools: readonly string[],
  signal: AbortSignal,
  alreadySurfaced: ReadonlySet<string>,
): Promise<Attachment[]> {
  // If an agent is @-mentioned, search only its memory dir (isolation).
  // Mentions come from the RAW user input — the enriched recallQuery may
  // contain assistant text and must not steer dir selection.
  const memoryDirs = extractAgentMentions(input).flatMap(mention => {
    const agentType = mention.replace('agent-', '')
    const agentDef = agents.find(def => def.agentType === agentType)
    return agentDef?.memory
      ? [getAgentMemoryDir(agentType, agentDef.memory)]
      : []
  })
  const dirs = memoryDirs.length > 0 ? memoryDirs : [getAutoMemPath()]

  const allResults = await Promise.all(
    dirs.map(dir =>
      findRelevantMemories(
        recallQuery,
        dir,
        signal,
        recentTools,
        alreadySurfaced,
      ).catch(() => []),
    ),
  )
  // alreadySurfaced is filtered inside the selector so the helper spends its
  // 5-slot budget on fresh candidates; readFileState catches files the
  // model read via FileReadTool. The redundant alreadySurfaced check here
  // is a belt-and-suspenders guard (multi-dir results may re-introduce a
  // path the selector filtered in a different dir).
  const selected = allResults
    .flat()
    .filter(m => !readFileState.has(m.path) && !alreadySurfaced.has(m.path))
    .slice(0, 5)

  const memories = await readMemoriesForSurfacing(selected, signal)

  if (memories.length === 0) {
    return []
  }
  return [{ type: 'relevant_memories' as const, memories }]
}

/**
 * Scan messages for past relevant_memories attachments.  Returns both the
 * set of surfaced paths (for selector de-dup) and cumulative byte count
 * (for session-total throttle).  Scanning messages rather than tracking
 * in toolUseContext means compact naturally resets both -- old attachments
 * are gone from the compacted transcript, so re-surfacing is valid again.
 */
export function collectSurfacedMemories(messages: ReadonlyArray<Message>): {
  paths: Set<string>
  totalBytes: number
} {
  const paths = new Set<string>()
  let totalBytes = 0
  for (const m of messages) {
    if (m.type === 'attachment' && m.attachment.type === 'relevant_memories') {
      for (const mem of m.attachment.memories) {
        paths.add(mem.path)
        totalBytes += mem.content.length
      }
    }
  }
  return { paths, totalBytes }
}

/**
 * Reads a set of relevance-ranked memory files for injection as
 * <system-reminder> attachments. Enforces both MAX_MEMORY_LINES and
 * MAX_MEMORY_BYTES via readFileInRange's truncateOnByteLimit option.
 * Truncation surfaces partial
 * content with a note rather than dropping the file -- findRelevantMemories
 * already picked this as most-relevant, so the frontmatter + opening context
 * is worth surfacing even if later lines are cut.
 *
 * Exported for direct testing without mocking the ranker + GB gates.
 */
export async function readMemoriesForSurfacing(
  selected: ReadonlyArray<{ path: string; mtimeMs: number; scope?: string }>,
  signal?: AbortSignal,
): Promise<
  Array<{
    path: string
    content: string
    mtimeMs: number
    header: string
    limit?: number
  }>
> {
  const results = await Promise.all(
    selected.map(async ({ path: filePath, mtimeMs, scope }) => {
      try {
        const result = await readFileInRange(
          filePath,
          0,
          MAX_MEMORY_LINES,
          MAX_MEMORY_BYTES,
          signal,
          { truncateOnByteLimit: true },
        )
        const truncated =
          result.totalLines > MAX_MEMORY_LINES || result.truncatedByBytes
        const content = truncated
          ? result.content +
            `\n\n> This memory file was truncated (${result.truncatedByBytes ? `${MAX_MEMORY_BYTES} byte limit` : `first ${MAX_MEMORY_LINES} lines`}). Use the ${FILE_READ_TOOL_NAME} tool to view the complete file at: ${filePath}`
          : result.content
        return {
          path: filePath,
          content,
          mtimeMs,
          header: memoryHeader(filePath, mtimeMs, scope),
          limit: truncated ? result.lineCount : undefined,
        }
      } catch {
        return null
      }
    }),
  )
  return results.filter(r => r !== null)
}

/**
 * Source label for a relevant-memory block, keyed on the retrieval scope the
 * hit came from (W-MEMORY-LIFECYCLE K4/K9). Unknown/absent scopes (including
 * the orchestrator's pre-multi-scope `private`/`team` values) render as plain
 * project memory — the pre-K9 header text.
 */
function memoryScopeName(scope?: string): string {
  if (scope === 'global') return 'Global memory'
  if (scope === 'knowledge') return 'Knowledge base'
  return 'Memory'
}

/**
 * Header string for a relevant-memory block.  Exported so messages.ts
 * can fall back for resumed sessions where the stored header is missing
 * (the fallback omits `scope` and renders the plain project-memory label).
 */
export function memoryHeader(
  path: string,
  mtimeMs: number,
  scope?: string,
): string {
  const staleness = memoryFreshnessText(mtimeMs)
  const label = memoryScopeName(scope)
  return staleness
    ? `${staleness}\n\n${label}: ${path}:`
    : `${label} (saved ${memoryAge(mtimeMs)}): ${path}:`
}

/**
 * Starts the relevant memory search as an async prefetch.
 * Extracts the last real user prompt from messages (skipping isMeta system
 * injections) and kicks off a non-blocking search. Returns a Disposable
 * handle with settlement tracking. Bound with `using` in query.ts.
 */
export function startRelevantMemoryPrefetch(
  messages: ReadonlyArray<Message>,
  toolUseContext: ToolUseContext,
): MemoryPrefetch | undefined {
  // W-MEMORY-DATA-COMPLETION A2 / D-A2 (2026-06-20): recall (SE-backed
  // memory.search prefetch) is ON by default for memory-enabled users, gated on
  // `isAutoMemoryEnabled()` alone. It is deliberately NO LONGER gated on
  // `tengu_moth_copse` — that flag is COMPOUND: it also drives `skip_index`
  // (save-prompt mode) and `filterInjectedMemoryFiles`, which STRIPS
  // MEMORY.md/AutoMem/TeamMem from the system-prompt injection. Reusing it to
  // turn recall on would have silently removed the MEMORY.md injection channel —
  // the exact index the dream auto-promote loop writes into. Recall enablement
  // and save-prompt mode are orthogonal, so they no longer share a flag here.
  // Opt out via the standard memory switches (CRABCODE_DISABLE_AUTO_MEMORY /
  // settings.autoMemoryEnabled=false / --bare); the prefetch also self-degrades
  // to empty when the orchestrator SE is unavailable (e.g. CLI).
  if (!isAutoMemoryEnabled()) {
    return undefined
  }

  const lastUserMessage = messages.findLast(m => m.type === 'user' && !m.isMeta)
  if (!lastUserMessage) {
    return undefined
  }

  const input = getUserMessageText(lastUserMessage)
  // Single-word prompts lack enough context for meaningful term extraction
  if (!input || !/\s/.test(input.trim())) {
    return undefined
  }

  const surfaced = collectSurfacedMemories(messages)
  if (surfaced.totalBytes >= RELEVANT_MEMORIES_CONFIG.MAX_SESSION_BYTES) {
    return undefined
  }

  // W-MEMORY-KB-UPLIFT P2 (2026-07-17) — recall query enrichment: contextual
  // follow-ups ("把这个存进知识库" / "again but for X") carry their referent in
  // the PREVIOUS assistant turn, not in the user text. Append the tail of the
  // last assistant text (bounded) to the recall query only; agent-mention dir
  // selection and all gates above stay on the raw user input.
  const lastAssistantText = messages.reduceRight<string | null>(
    (found, m) => found ?? getAssistantMessageText(m),
    null,
  )
  const assistantTail = lastAssistantText
    ?.trim()
    .slice(-RECALL_ENRICH_ASSISTANT_TAIL_CHARS)
  const recallQuery = assistantTail ? `${input}\n${assistantTail}` : input

  // Chained to the turn-level abort so user Escape cancels the sideQuery
  // immediately, not just on [Symbol.dispose] when queryLoop exits.
  const controller = createChildAbortController(toolUseContext.abortController)
  const firedAt = Date.now()
  const promise = getRelevantMemoryAttachments(
    input,
    recallQuery,
    toolUseContext.options.agentDefinitions.activeAgents,
    toolUseContext.readFileState,
    collectRecentSuccessfulTools(messages, lastUserMessage),
    controller.signal,
    surfaced.paths,
  ).catch(e => {
    if (!isAbortError(e)) {
      logError(e)
    }
    return []
  })

  const handle: MemoryPrefetch = {
    promise,
    settledAt: null,
    consumedOnIteration: -1,
    [Symbol.dispose]() {
      controller.abort()
      logEvent('tengu_memdir_prefetch_collected', {
        hidden_by_first_iteration:
          handle.settledAt !== null && handle.consumedOnIteration === 0,
        consumed_on_iteration: handle.consumedOnIteration,
        latency_ms: (handle.settledAt ?? Date.now()) - firedAt,
      })
    },
  }
  void promise.finally(() => {
    handle.settledAt = Date.now()
  })
  return handle
}

/**
 * Filters prefetched memory attachments to exclude memories the model already
 * has in context via FileRead/Write/Edit tool calls (any iteration this turn)
 * or a previous turn's memory surfacing -- both tracked in the cumulative
 * readFileState. Survivors are then marked in readFileState so subsequent
 * turns won't re-surface them.
 *
 * The mark-after-filter ordering is load-bearing: readMemoriesForSurfacing
 * used to write to readFileState during the prefetch, which meant the filter
 * saw every prefetch-selected path as "already in context" and dropped them
 * all (self-referential filter). Deferring the write to here, after the
 * filter runs, breaks that cycle while still deduping against tool calls
 * from any iteration.
 */
export function filterDuplicateMemoryAttachments(
  attachments: Attachment[],
  readFileState: FileStateCache,
): Attachment[] {
  return attachments
    .map(attachment => {
      if (attachment.type !== 'relevant_memories') return attachment
      const filtered = attachment.memories.filter(
        m => !readFileState.has(m.path),
      )
      for (const m of filtered) {
        readFileState.set(m.path, {
          content: m.content,
          timestamp: m.mtimeMs,
          offset: undefined,
          limit: m.limit,
        })
      }
      return filtered.length > 0 ? { ...attachment, memories: filtered } : null
    })
    .filter((a): a is Attachment => a !== null)
}

/**
 * Gets nested memory attachments for a file path opened in the IDE.
 * Re-exported for use in getOpenedFileFromIDE in the main orchestrator.
 */
export { getNestedMemoryAttachmentsForFile }
