import { z } from 'zod/v4'

import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import {
  getAutoMemPath,
  getGlobalMemDir,
  getGlobalMemStateDir,
  getKnowledgeDir,
  getKnowledgeStateDir,
} from '../../memdir/paths.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { errorMessage } from '../../utils/errors.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { logForDebugging } from '../../utils/debug.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import { DESCRIPTION, MEMORY_SEARCH_TOOL_NAME, PROMPT } from './prompt.js'

const DEFAULT_TOP_K = 5
const MAX_TOP_K = 20
const SEARCH_TIMEOUT_MS = 2_000

const inputSchema = lazySchema(() =>
  z.object({
    query: z
      .string()
      .min(1)
      .describe('Keywords to search saved memory notes for (multilingual).'),
    top_k: z
      .number()
      .int()
      .positive()
      .max(MAX_TOP_K)
      .optional()
      .describe(`Max results to return (default ${DEFAULT_TOP_K}).`),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

const outputSchema = lazySchema(() =>
  z.object({
    // `available: false` distinguishes "memory service not running in this
    // environment" (e.g. CLI, no orchestrator) from a genuine empty result.
    available: z.boolean(),
    results: z.array(
      z.object({
        path: z.string(),
        name: z.string().nullable(),
        score: z.number(),
        snippet: z.string().nullable(),
        scope: z.string().nullable(),
        type: z.string().nullable(),
      }),
    ),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>

export type Output = z.infer<OutputSchema>
type Hit = Output['results'][number]

/** Parse the orchestrator's snake_case `memory.search` result envelope. */
function parseHits(response: unknown): Hit[] {
  if (response === null || typeof response !== 'object') {
    return []
  }
  const results = (response as { results?: unknown }).results
  if (!Array.isArray(results)) {
    return []
  }
  const hits: Hit[] = []
  for (const raw of results) {
    if (raw === null || typeof raw !== 'object') {
      continue
    }
    const r = raw as Record<string, unknown>
    const path = typeof r.source_path === 'string' ? r.source_path : null
    if (!path) {
      continue
    }
    hits.push({
      path,
      name: typeof r.name === 'string' ? r.name : null,
      score: typeof r.score === 'number' ? r.score : 0,
      snippet: typeof r.snippet === 'string' ? r.snippet : null,
      scope: typeof r.scope === 'string' ? r.scope : null,
      type: typeof r.memory_type === 'string' ? r.memory_type : null,
    })
  }
  return hits
}

/**
 * W-MEMORY-DATA-COMPLETION A3 — read-only memory search tool. Lets the model
 * actively query its saved memory notes (the orchestrator SE, BM25F lexical
 * recall) on top of the passive per-turn recall prefetch. READ-ONLY by design:
 * writing memory stays on the automatic pipeline (gate / lock / leader),
 * never a model-callable tool. Feature-gated (`MEMORY_SEARCH_TOOL`, default ON
 * since W-MEMORY-LIFECYCLE K7 2026-07-09). Searches span the per-project
 * memory root, the user-global memory root, and the personal knowledge base
 * (K4/K9 multi-scope retrieval). Degrades to `available: false` when the
 * orchestrator is unreachable (e.g. CLI) instead of erroring.
 */
export const MemorySearchTool = buildTool({
  name: MEMORY_SEARCH_TOOL_NAME,
  searchHint: 'search saved memory notes',
  maxResultSizeChars: 20_000,
  isConcurrencySafe() {
    return true
  },
  isReadOnly() {
    return true
  },
  toAutoClassifierInput(input) {
    return input.query ?? ''
  },
  async description() {
    return DESCRIPTION
  },
  async prompt() {
    return PROMPT
  },
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  async call(input) {
    const { query, top_k } = input
    // No orchestrator SE in this environment (e.g. CLI) — honest signal, not
    // an error (the model should not retry).
    if (!memoryBridgeIpc.isAvailable()) {
      return { data: { available: false, results: [] } }
    }
    try {
      const response = await memoryBridgeIpc.send(
        'memory.search',
        {
          memory_dir: getAutoMemPath(),
          query,
          top_k: top_k ?? DEFAULT_TOP_K,
          mode: 'hybrid',
          // W-MEMORY-LIFECYCLE K4/K9: search the per-project root, the
          // user-global memory root, and the personal knowledge base. Dirs are
          // resolved TS-side; each root carries its own SE state dir.
          scopes: ['project', 'global', 'knowledge'],
          global_memory_dir: getGlobalMemDir(),
          global_state_dir: getGlobalMemStateDir(),
          knowledge_dir: getKnowledgeDir(),
          knowledge_state_dir: getKnowledgeStateDir(),
          // W-MEMORY-KB-UPLIFT P0 — this tool IS the explicit search surface,
          // so `injection: manual` knowledge entries are visible here; the
          // passive per-turn recall (findRelevantMemories) omits the flag and
          // never surfaces them.
          include_manual: true,
        },
        { timeout_ms: SEARCH_TIMEOUT_MS },
      )
      return { data: { available: true, results: parseHits(response) } }
    } catch (e) {
      // Warming-up SE / timeout / IPC hiccup → fail-soft empty.
      logForDebugging(
        `[MemorySearchTool] memory.search failed: ${errorMessage(e)}`,
        { level: 'warn' },
      )
      return { data: { available: true, results: [] } }
    }
  },
  userFacingName() {
    return 'Search memory'
  },
  renderToolUseMessage(input) {
    return input.query ? `Searching memory for "${input.query}"` : 'Searching memory'
  },
  renderToolResultMessage(output) {
    if (!output.available) {
      return 'Memory service is not enabled in this environment'
    }
    const n = output.results.length
    return n === 0
      ? 'No relevant memories found'
      : `Found ${n} relevant memor${n === 1 ? 'y' : 'ies'}`
  },
  mapToolResultToToolResultBlockParam(content, toolUseID) {
    if (!content.available) {
      return {
        tool_use_id: toolUseID,
        type: 'tool_result',
        content:
          'Memory service is not enabled in this environment (no saved-memory search available here).',
      }
    }
    if (content.results.length === 0) {
      return {
        tool_use_id: toolUseID,
        type: 'tool_result',
        content: 'No relevant memories found.',
      }
    }
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: jsonStringify(content.results),
    }
  },
} satisfies ToolDef<InputSchema, Output>)
