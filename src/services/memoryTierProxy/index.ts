/**
 * Memory Tier reverse-IPC proxy. One lease holder consumes local orchestrator
 * requests, performs bounded LLM/embedding work, and writes results back over
 * the same socket. A module-local FIFO prevents duplicate or concurrent Tier
 * work from overspending quota.
 */

import {
  DEFAULT_MEMORY_TIER_PROXY_DEPS,
  type MemoryTierProxyDeps,
} from './deps.js'
import {
  resolveMemoryOutputLanguage,
  withMemoryLanguageDirective,
} from './language.js'
import {
  MEMORY_TIER_EMBEDDING_REQUEST_NOTIFICATION,
  MEMORY_TIER_LLM_CALL_REQUEST_NOTIFICATION,
  MEMORY_TIER_RERANK_REQUEST_NOTIFICATION,
  type MemoryTierEmbeddingRequestPayload,
  type MemoryTierEmbeddingResultPayload,
  type MemoryTierLlmCallRequestPayload,
  type MemoryTierLlmCallResultPayload,
  type MemoryTierRerankRequestPayload,
  type MemoryTierRerankResultPayload,
} from './types.js'

/** Default in-flight Tier-call concurrency budget. */
const DEFAULT_MAX_CONCURRENT = 1

function logStderr(msg: string): void {
  process.stderr.write(`[memory-tier-proxy] ${msg}\n`)
}

/**
 * Minimal FIFO concurrency limiter (no external `p-limit` dependency). Caps
 * `limit` simultaneous in-flight tasks; excess tasks queue and run FIFO as
 * slots free.
 */
function createLimiter(limit: number): <T>(task: () => Promise<T>) => Promise<T> {
  let active = 0
  const queue: (() => void)[] = []

  const next = (): void => {
    if (active >= limit) return
    const run = queue.shift()
    if (run) run()
  }

  return <T>(task: () => Promise<T>): Promise<T> =>
    new Promise<T>((resolve, reject) => {
      const run = (): void => {
        active += 1
        task()
          .then(resolve, reject)
          .finally(() => {
            active -= 1
            next()
          })
      }
      queue.push(run)
      next()
    })
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

/**
 * Parse an inbound orchestrator events frame into an LLM call request payload,
 * or `null` if the frame is not a (well-formed) `memory/tier/llmCallRequest`.
 *
 * The orchestrator pushes `{ notification: <name>, payload: <snake_case> }`
 * (see `memory_events_pump.rs::classify_frame`). We read fields defensively so
 * a malformed frame is skipped rather than crashing the subscriber.
 */
export function parseLlmCallRequestFrame(
  frame: unknown,
): MemoryTierLlmCallRequestPayload | null {
  if (!isRecord(frame)) return null
  if (frame.notification !== MEMORY_TIER_LLM_CALL_REQUEST_NOTIFICATION) return null
  const payload = frame.payload
  if (!isRecord(payload)) return null

  const reqId = payload.req_id
  const tier = payload.tier
  const messages = payload.messages
  if (typeof reqId !== 'string' || reqId.length === 0) return null
  if (typeof tier !== 'string') return null
  if (!Array.isArray(messages)) return null

  const parsedMessages: { role: string; content: string }[] = []
  for (const m of messages) {
    if (!isRecord(m)) return null
    if (typeof m.role !== 'string' || typeof m.content !== 'string') return null
    parsedMessages.push({ role: m.role, content: m.content })
  }

  const params = isRecord(payload.params) ? payload.params : {}

  return {
    req_id: reqId,
    tier,
    ...(typeof payload.phase === 'string' ? { phase: payload.phase } : {}),
    messages: parsedMessages,
    ...(typeof payload.model_hint === 'string' && payload.model_hint.length > 0
      ? { model_hint: payload.model_hint }
      : {}),
    params: {
      ...(typeof params.temperature === 'number'
        ? { temperature: params.temperature }
        : {}),
      ...(typeof params.max_tokens === 'number'
        ? { max_tokens: params.max_tokens }
        : {}),
    },
  }
}

/**
 * Parse an inbound orchestrator events frame into an embedding request
 * payload, or `null` if the frame is not a (well-formed)
 * `memory/tier/embeddingRequest`. Same defensive posture as
 * `parseLlmCallRequestFrame`.
 */
export function parseEmbeddingRequestFrame(
  frame: unknown,
): MemoryTierEmbeddingRequestPayload | null {
  if (!isRecord(frame)) return null
  if (frame.notification !== MEMORY_TIER_EMBEDDING_REQUEST_NOTIFICATION) {
    return null
  }
  const payload = frame.payload
  if (!isRecord(payload)) return null

  const reqId = payload.req_id
  const texts = payload.texts
  const textKeys = payload.text_keys
  if (typeof reqId !== 'string' || reqId.length === 0) return null
  if (!Array.isArray(texts) || !texts.every(t => typeof t === 'string')) {
    return null
  }
  if (
    !Array.isArray(textKeys) ||
    !textKeys.every(k => typeof k === 'string') ||
    textKeys.length !== texts.length
  ) {
    return null
  }

  return {
    req_id: reqId,
    texts: texts as string[],
    text_keys: textKeys as string[],
    ...(typeof payload.model_hint === 'string' && payload.model_hint.length > 0
      ? { model_hint: payload.model_hint }
      : {}),
  }
}

/**
 * Handle a single parsed embedding request (W-MEMORY-ALIVE PR-2a): run the
 * SDK embedding call and write the result back. Exported for unit tests.
 * Never throws — failures are written back as empty embeddings + `{ error }`
 * so the orchestrator's pending `embed()` resolves honestly and the Rust
 * side degrades to lexical-only retrieval.
 */
export async function handleEmbeddingRequest(
  request: MemoryTierEmbeddingRequestPayload,
  deps: MemoryTierProxyDeps,
): Promise<void> {
  let result: MemoryTierEmbeddingResultPayload
  try {
    const { embeddings, dimension } = await deps.runEmbedding({
      texts: request.texts,
      ...(request.model_hint !== undefined
        ? { modelHint: request.model_hint }
        : {}),
    })
    if (embeddings.length !== request.texts.length || dimension <= 0) {
      result = {
        req_id: request.req_id,
        embeddings: [],
        dimension: 0,
        error: `embedding executor returned ${embeddings.length} vectors (dim=${dimension}) for ${request.texts.length} texts`,
      }
    } else {
      result = {
        req_id: request.req_id,
        embeddings: embeddings.map(values => ({ values })),
        dimension,
      }
    }
  } catch (err) {
    result = {
      req_id: request.req_id,
      embeddings: [],
      dimension: 0,
      error: err instanceof Error ? err.message : String(err),
    }
  }

  try {
    await deps.sendEmbeddingResult(result)
  } catch (err) {
    logStderr(
      `embedding result write-back failed for req_id=${request.req_id}: ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

/**
 * Parse an inbound orchestrator events frame into a rerank request payload,
 * or `null` if the frame is not a (well-formed) `memory/tier/rerankRequest`
 * (W-MEMORY-KB-UPLIFT P1). Same defensive posture as the other parsers.
 */
export function parseRerankRequestFrame(
  frame: unknown,
): MemoryTierRerankRequestPayload | null {
  if (!isRecord(frame)) return null
  if (frame.notification !== MEMORY_TIER_RERANK_REQUEST_NOTIFICATION) {
    return null
  }
  const payload = frame.payload
  if (!isRecord(payload)) return null

  const reqId = payload.req_id
  const query = payload.query
  const documents = payload.documents
  if (typeof reqId !== 'string' || reqId.length === 0) return null
  if (typeof query !== 'string') return null
  if (!Array.isArray(documents) || !documents.every(d => typeof d === 'string')) {
    return null
  }

  return {
    req_id: reqId,
    query,
    documents: documents as string[],
    ...(typeof payload.model_hint === 'string' && payload.model_hint.length > 0
      ? { model_hint: payload.model_hint }
      : {}),
  }
}

/**
 * Handle a single parsed rerank request (W-MEMORY-KB-UPLIFT P1): run the SDK
 * rerank call and write the ranking back. Exported for unit tests. Never
 * throws — failures are written back as empty ranking + `{ error }` so the
 * orchestrator's pending round-trip resolves honestly and `memory.search`
 * fail-softs to its RRF fusion order.
 */
export async function handleRerankRequest(
  request: MemoryTierRerankRequestPayload,
  deps: MemoryTierProxyDeps,
): Promise<void> {
  let result: MemoryTierRerankResultPayload
  try {
    const { ranking } = await deps.runRerank({
      query: request.query,
      documents: request.documents,
      ...(request.model_hint !== undefined
        ? { modelHint: request.model_hint }
        : {}),
    })
    result = {
      req_id: request.req_id,
      ranking: ranking.filter(
        entry =>
          Number.isInteger(entry.index) &&
          entry.index >= 0 &&
          entry.index < request.documents.length &&
          Number.isFinite(entry.score),
      ),
    }
  } catch (err) {
    result = {
      req_id: request.req_id,
      ranking: [],
      error: err instanceof Error ? err.message : String(err),
    }
  }

  try {
    await deps.sendRerankResult(result)
  } catch (err) {
    logStderr(
      `rerank result write-back failed for req_id=${request.req_id}: ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

/**
 * Handle a single parsed request: run the LLM call and write the result back.
 * Exported for unit tests. Never throws — failures are written back as
 * `{ error }` (an empty / failed call MUST NOT be reported as success).
 */
export async function handleLlmCallRequest(
  request: MemoryTierLlmCallRequestPayload,
  deps: MemoryTierProxyDeps,
): Promise<void> {
  const model = request.model_hint ?? deps.getMainLoopModel()
  // W-MEMORY-SYNERGY W3 (2026-07-16, RC-5) — 唯一执行点注入输出语言指令，
  // 覆盖全部 Tier/想象/报告 LLM 调用的行文语言（结构模板语言归 Rust，
  // 分工见 ./language.ts 模块头）。
  const language = (
    deps.resolveOutputLanguage ?? resolveMemoryOutputLanguage
  )()
  const messages = withMemoryLanguageDirective(request.messages, language)
  let result: MemoryTierLlmCallResultPayload
  try {
    const { text, usage } = await deps.runLlmCall({
      model,
      messages,
      ...(request.params.temperature !== undefined
        ? { temperature: request.params.temperature }
        : {}),
      ...(request.params.max_tokens !== undefined
        ? { maxTokens: request.params.max_tokens }
        : {}),
    })
    if (typeof text !== 'string' || text.length === 0) {
      // Empty completion (e.g. sideQuery NetworkError fallback returns []).
      // Treat as failure — must not be reported as success.
      result = {
        req_id: request.req_id,
        error: 'memory tier LLM call returned empty content',
      }
    } else {
      result = {
        req_id: request.req_id,
        response: text,
        ...(usage ? { usage } : {}),
      }
    }
  } catch (err) {
    result = {
      req_id: request.req_id,
      error: err instanceof Error ? err.message : String(err),
    }
  }

  try {
    await deps.sendLlmCallResult(result)
  } catch (err) {
    logStderr(
      `result write-back failed for req_id=${request.req_id}: ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

type ProxyHandle = {
  dispose: () => void
}

let installedHandle: ProxyHandle | null = null

export type StartMemoryTierProxyOptions = {
  deps?: MemoryTierProxyDeps
  maxConcurrent?: number
}

/**
 * Install the memory Tier proxy subscription. Idempotent — a second call while
 * a subscription is live is a no-op (returns the existing handle). The caller
 * (`memory-bootstrap.ts`) installs this only on the leader worker, but the
 * per-frame leader gate is also checked so a lease lost mid-flight stops new
 * executions immediately.
 */
export function startMemoryTierProxy(
  options: StartMemoryTierProxyOptions = {},
): ProxyHandle {
  if (installedHandle) return installedHandle

  const deps = options.deps ?? DEFAULT_MEMORY_TIER_PROXY_DEPS
  const limit = createLimiter(options.maxConcurrent ?? DEFAULT_MAX_CONCURRENT)

  const unsubscribe = deps.subscribeToOrchestratorEvents(frame => {
    // Leader gate: only the lease holder executes. Checked per-frame so a
    // lease handed off mid-run stops new executions immediately.
    if (!deps.isLeaderHeld()) return
    const llmRequest = parseLlmCallRequestFrame(frame)
    if (llmRequest !== null) {
      void limit(() => handleLlmCallRequest(llmRequest, deps))
      return
    }
    const embeddingRequest = parseEmbeddingRequestFrame(frame)
    if (embeddingRequest !== null) {
      void limit(() => handleEmbeddingRequest(embeddingRequest, deps))
      return
    }
    // W-MEMORY-KB-UPLIFT P1 — rerank runs OUTSIDE the FIFO limiter: it sits
    // on the interactive search critical path with a 3s orchestrator budget,
    // and must never queue behind a 30s dream LLM call sharing the limit=1
    // background lane. One in-flight rerank per query, cheap by construction.
    const rerankRequest = parseRerankRequestFrame(frame)
    if (rerankRequest !== null) {
      void handleRerankRequest(rerankRequest, deps)
    }
  })

  const handle: ProxyHandle = {
    dispose: () => {
      unsubscribe()
      if (installedHandle === handle) installedHandle = null
    },
  }
  installedHandle = handle
  return handle
}

/** Tear down the proxy subscription (idempotent). */
export function stopMemoryTierProxy(): void {
  installedHandle?.dispose()
  installedHandle = null
}

/** Test-only: is a subscription currently installed? */
export function _testOnly_isProxyInstalled(): boolean {
  return installedHandle !== null
}

// ──────────────────────────────────────────────────────────────────────────
// Embedding channel (`memory/tier/embeddingRequest`) — LIVE since
// W-MEMORY-ALIVE PR-2a (2026-07-01). The old blocker (`§15-7 forbids an SDK
// embedding caller`) was revised by 用户裁决③: embedding *production* goes
// through SDK `client.embeddings()` (2.10, `supports_embedding` capability
// discovery, zero hardcoded slugs); vector *storage and retrieval* stay in
// the local Rust `acosmi-memory-se` engine. See `handleEmbeddingRequest`
// above + `deps.ts::defaultRunEmbedding`.
// ──────────────────────────────────────────────────────────────────────────
