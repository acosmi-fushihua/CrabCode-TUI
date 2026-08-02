import { stat } from 'fs/promises'

import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import { logForDebugging } from '../utils/debug.js'
import { errorMessage } from '../utils/errors.js'
import {
  getGlobalMemDir,
  getGlobalMemStateDir,
  getKnowledgeDir,
  getKnowledgeStateDir,
} from './paths.js'

export type RelevantMemory = {
  path: string
  mtimeMs: number
  /**
   * Retrieval scope the hit came from (`project` / `global` / `knowledge`,
   * W-MEMORY-LIFECYCLE K4/K9). Absent when the orchestrator predates
   * multi-scope search — callers must treat that as project-scoped.
   */
  scope?: string
}

/**
 * W-MEMORY-DATA-COMPLETION A2.2 (2026-06-20) — request more than the caller's
 * 5-slot budget so the downstream `alreadySurfaced` / `readFileState` filtering
 * has fresh candidates to fall back on.
 */
const RECALL_TOP_K = 10

/**
 * Lexical recall is a fast local IPC (BM25F over the orchestrator SE), but bound
 * it generously — it runs as a non-blocking prefetch, never on the hot path.
 */
const RECALL_TIMEOUT_MS = 2_000

type RawSearchHit = { source_path: string; scope?: string }

/**
 * Find memory files relevant to a query via the orchestrator SE (`memory.search`
 * — charabia-tokenized BM25F lexical recall, the hybrid retrieval floor).
 *
 * Returns absolute file paths + mtime of the most relevant memories (caller
 * slices to 5). `mtimeMs` is statted per hit so callers can surface freshness to
 * the main model. `alreadySurfaced` filters paths shown in prior turns.
 *
 * W-MEMORY-DATA-COMPLETION A2 root cause: this used to scan markdown headers and
 * spend a per-turn LLM `sideQuery` to *select* relevant files — a recall path
 * that never touched the SE (so the real index + multilingual tokenizer were
 * dead weight) and burned model quota every turn. Routing recall through
 * `memory.search` unifies recall with the indexing/retrieval engine AND removes
 * the per-turn LLM call (lexical search is local) → §15 #7 contract honoured,
 * lower quota, no two-implementation drift.
 *
 * `recentTools` is retained for caller/signature compatibility but unused: it
 * existed to steer the LLM selector away from tool-reference noise; lexical BM25F
 * has no such failure mode.
 */
export async function findRelevantMemories(
  query: string,
  memoryDir: string,
  signal: AbortSignal,
  _recentTools: readonly string[] = [],
  alreadySurfaced: ReadonlySet<string> = new Set(),
): Promise<RelevantMemory[]> {
  // CLI / not-yet-initialised: no orchestrator SE → degrade to empty (the
  // MEMORY.md system-prompt injection still carries memory in that case).
  if (!memoryBridgeIpc.isAvailable()) {
    return []
  }

  let hits: RawSearchHit[]
  try {
    const response = await memoryBridgeIpc.send(
      'memory.search',
      {
        memory_dir: memoryDir,
        query,
        top_k: RECALL_TOP_K,
        mode: 'hybrid',
        // W-MEMORY-LIFECYCLE K4/K9: recall spans the per-project root, the
        // user-global memory root, and the personal knowledge base. The dirs
        // are resolved TS-side (the orchestrator has no session context for
        // the user-global base) and each root carries its own SE state dir.
        scopes: ['project', 'global', 'knowledge'],
        global_memory_dir: getGlobalMemDir(),
        global_state_dir: getGlobalMemStateDir(),
        knowledge_dir: getKnowledgeDir(),
        knowledge_state_dir: getKnowledgeStateDir(),
      },
      { timeout_ms: RECALL_TIMEOUT_MS },
    )
    hits = parseSearchHits(response)
  } catch (e) {
    if (signal.aborted) {
      return []
    }
    // Warming-up SE / timeout / IPC hiccup must never break a turn.
    logForDebugging(`[memdir] memory.search recall failed: ${errorMessage(e)}`, {
      level: 'warn',
    })
    return []
  }

  // Hits arrive score-sorted from the orchestrator. De-dupe + drop already
  // surfaced paths, then stat for the freshness header.
  const seen = new Set<string>()
  const out: RelevantMemory[] = []
  for (const hit of hits) {
    const path = hit.source_path
    if (alreadySurfaced.has(path) || seen.has(path)) {
      continue
    }
    seen.add(path)
    out.push({
      path,
      mtimeMs: await statMtimeMs(path),
      ...(hit.scope !== undefined ? { scope: hit.scope } : {}),
    })
  }
  return out
}

/** Parse the orchestrator's snake_case `memory.search` result envelope. */
function parseSearchHits(response: unknown): RawSearchHit[] {
  if (response === null || typeof response !== 'object') {
    return []
  }
  const results = (response as { results?: unknown }).results
  if (!Array.isArray(results)) {
    return []
  }
  const hits: RawSearchHit[] = []
  for (const raw of results) {
    if (raw !== null && typeof raw === 'object') {
      const { source_path: sourcePath, scope } = raw as {
        source_path?: unknown
        scope?: unknown
      }
      if (typeof sourcePath === 'string' && sourcePath.length > 0) {
        hits.push({
          source_path: sourcePath,
          ...(typeof scope === 'string' && scope.length > 0 ? { scope } : {}),
        })
      }
    }
  }
  return hits
}

/** mtime in ms (0 on stat failure — header degrades gracefully). */
async function statMtimeMs(path: string): Promise<number> {
  try {
    return (await stat(path)).mtimeMs
  } catch {
    return 0
  }
}
