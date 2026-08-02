/**
 * Injectable dependencies for the memory Tier reverse-IPC proxy. External
 * effects remain replaceable in tests, while production talks directly to the
 * local memory orchestrator through CRABCODE_MEMORY_IPC_ENDPOINT.
 */

import { createConnection, type Socket } from 'node:net'
import { sideQuery as defaultSideQuery } from '../../utils/sideQuery.js'
import { getAcosmiClient } from '../../services/acosmi/client.js'
import { getMainLoopModel } from '../../utils/model/model.js'
import { isLeaderHeld } from './leaderBootstrap.js'
import {
  MEMORY_TIER_EMBEDDING_RESULT_METHOD,
  MEMORY_TIER_LLM_CALL_RESULT_METHOD,
  MEMORY_TIER_RERANK_RESULT_METHOD,
  type MemoryTierEmbeddingResultPayload,
  type MemoryTierLlmCallResultPayload,
  type MemoryTierRerankEntry,
  type MemoryTierRerankResultPayload,
} from './types.js'

const MEMORY_IPC_ENDPOINT_ENV = 'CRABCODE_MEMORY_IPC_ENDPOINT'

/** Result write-back IPC request timeout. */
const RESULT_REQUEST_TIMEOUT_MS = 5_000

/**
 * Subscribe to the orchestrator events long-connection. The handler is invoked
 * once per inbound JSON frame line. Returns a disposer that tears the
 * subscription down (closes the socket / clears retry timers). Implementations
 * MUST be idempotent on the disposer.
 */
export type SubscribeToOrchestratorEvents = (
  onFrame: (frame: unknown) => void,
) => () => void

/** Send a one-shot result write-back request to the orchestrator. */
export type SendLlmCallResult = (
  payload: MemoryTierLlmCallResultPayload,
) => Promise<void>

/** Run the SDK-backed LLM call for a Tier request; returns assistant text. */
export type RunMemoryTierLlmCall = (args: {
  model: string
  messages: { role: string; content: string }[]
  temperature?: number
  maxTokens?: number
  signal?: AbortSignal
}) => Promise<{ text: string; usage?: { input_tokens: number; output_tokens: number } }>

/** Send a one-shot embedding-result write-back request to the orchestrator. */
export type SendEmbeddingResult = (
  payload: MemoryTierEmbeddingResultPayload,
) => Promise<void>

/**
 * Run the SDK-backed embedding call for a batch of texts (W-MEMORY-ALIVE
 * PR-2a). Returns one vector per input text (index-aligned) plus the common
 * dimension. Throws on failure — the caller writes back an honest `{ error }`
 * (the Rust side then degrades to lexical-only retrieval, never fakes).
 */
export type RunMemoryTierEmbedding = (args: {
  texts: string[]
  modelHint?: string
}) => Promise<{ embeddings: number[][]; dimension: number }>

/** Send a one-shot rerank-result write-back request to the orchestrator. */
export type SendRerankResult = (
  payload: MemoryTierRerankResultPayload,
) => Promise<void>

/**
 * Run the SDK-backed rerank call for a query + candidate documents
 * (W-MEMORY-KB-UPLIFT P1). Returns ranking entries (candidate index +
 * relevance score). Throws on failure — the caller writes back an honest
 * `{ error }` (the orchestrator then fail-softs to its RRF fusion order).
 */
export type RunMemoryTierRerank = (args: {
  query: string
  documents: string[]
  modelHint?: string
}) => Promise<{ ranking: MemoryTierRerankEntry[] }>

export type MemoryTierProxyDeps = {
  /** Leader gate: only the leader worker executes Tier calls. */
  isLeaderHeld: () => boolean
  /** Default model slug when the request carries no `model_hint`. */
  getMainLoopModel: () => string
  /**
   * W3 (2026-07-16, RC-5) — 记忆产物行文语言解析（省略 = 生产默认
   * `resolveMemoryOutputLanguage`，读 memoryLanguage/uiLanguage 配置）。
   * OPTIONAL：既有测试构造的 deps 对象零波及。
   */
  resolveOutputLanguage?: () => import('./language.js').MemoryOutputLanguage
  /** Persistent orchestrator events subscription. */
  subscribeToOrchestratorEvents: SubscribeToOrchestratorEvents
  /** One-shot result write-back. */
  sendLlmCallResult: SendLlmCallResult
  /** SDK-backed LLM call. */
  runLlmCall: RunMemoryTierLlmCall
  /** One-shot embedding-result write-back. */
  sendEmbeddingResult: SendEmbeddingResult
  /** SDK-backed embedding call. */
  runEmbedding: RunMemoryTierEmbedding
  /** One-shot rerank-result write-back (W-MEMORY-KB-UPLIFT P1). */
  sendRerankResult: SendRerankResult
  /** SDK-backed rerank call (W-MEMORY-KB-UPLIFT P1). */
  runRerank: RunMemoryTierRerank
}

const SUBSCRIBE_REQUEST_LINE = JSON.stringify({ method: 'memory.events.subscribe' }) + '\n'

export function resolveIpcEndpointPath(): string | null {
  const endpoint = process.env[MEMORY_IPC_ENDPOINT_ENV]
  if (!endpoint) return null
  if (endpoint.startsWith('unix:')) {
    const path = endpoint.slice('unix:'.length)
    return path.length > 0 ? path : null
  }
  if (endpoint.startsWith('npipe:')) {
    // Windows named pipe. The orchestrator emits `npipe:<\\.\pipe\name>` and
    // its Rust pump strips the prefix identically (memory_events_pump.rs
    // resolve_npipe_path). `createConnection(<pipe name>)` connects to a
    // Windows named pipe just like a Unix-socket path, so the connect /
    // reconnect logic below is platform-agnostic — the previous `return null`
    // silently killed the Windows memory Tier reverse bridge.
    const pipe = endpoint.slice('npipe:'.length)
    return pipe.length > 0 ? pipe : null
  }
  return null
}

/**
 * Default events subscription: opens a persistent UDS connection, sends the
 * subscribe handshake, and forwards each newline-delimited frame to `onFrame`.
 * Reconnects with exponential backoff (1s → 30s cap) on disconnect — mirrors
 * `memory_events_pump.rs::run_pump_loop`. No-op when the endpoint is unset.
 */
export const defaultSubscribeToOrchestratorEvents: SubscribeToOrchestratorEvents = (
  onFrame,
) => {
  const path = resolveIpcEndpointPath()
  let disposed = false
  let socket: Socket | null = null
  let retryTimer: ReturnType<typeof setTimeout> | null = null
  let backoffMs = 1_000

  if (path === null) {
    // Endpoint unset / unsupported transport → no-op subscription.
    return () => {}
  }

  const scheduleReconnect = (): void => {
    if (disposed) return
    retryTimer = setTimeout(connect, backoffMs)
    retryTimer.unref?.()
    backoffMs = Math.min(backoffMs * 2, 30_000)
  }

  const connect = (): void => {
    if (disposed) return
    let buffer = ''
    const sock = createConnection(path)
    socket = sock
    sock.on('connect', () => {
      backoffMs = 1_000
      sock.write(SUBSCRIBE_REQUEST_LINE)
    })
    sock.on('data', chunk => {
      buffer += Buffer.isBuffer(chunk) ? chunk.toString('utf8') : String(chunk)
      let newlineIndex: number
      while ((newlineIndex = buffer.indexOf('\n')) >= 0) {
        const rawLine = buffer.slice(0, newlineIndex).trim()
        buffer = buffer.slice(newlineIndex + 1)
        if (!rawLine) continue
        try {
          onFrame(JSON.parse(rawLine))
        } catch {
          // Unparseable frame — skip (mirrors pump FrameDecision::Unparseable).
        }
      }
    })
    sock.on('error', () => {
      // Swallow — `close` schedules the reconnect.
    })
    sock.on('close', () => {
      socket = null
      scheduleReconnect()
    })
  }

  connect()

  return () => {
    if (disposed) return
    disposed = true
    if (retryTimer !== null) {
      clearTimeout(retryTimer)
      retryTimer = null
    }
    socket?.destroy()
    socket = null
  }
}

/**
 * Default result write-back: one-shot UDS request to
 * `memory.tier.llm_call_result` (same shape `dispatcher/memory.rs` forwards).
 * Rejects on timeout / connection error so the caller can log; the orchestrator
 * pending oneshot resolves on delivery.
 */
async function sendOneShotResult(method: string, payload: unknown): Promise<void> {
  const path = resolveIpcEndpointPath()
  if (path === null) {
    throw new Error(
      `${MEMORY_IPC_ENDPOINT_ENV} not set / unsupported transport; cannot write Tier result back`,
    )
  }
  const request = JSON.stringify({ method, payload }) + '\n'

  await new Promise<void>((resolve, reject) => {
    const sock = createConnection(path)
    let settled = false
    const finish = (err: Error | null): void => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      sock.destroy()
      if (err) reject(err)
      else resolve()
    }
    const timer = setTimeout(
      () => finish(new Error(`${method} timed out after ${RESULT_REQUEST_TIMEOUT_MS}ms`)),
      RESULT_REQUEST_TIMEOUT_MS,
    )
    timer.unref?.()
    sock.on('connect', () => {
      sock.write(request)
    })
    // Orchestrator replies `{"ok":true}` then keeps/closes; either way the
    // write succeeded once we get any data or a clean end.
    sock.on('data', () => finish(null))
    sock.on('end', () => finish(null))
    sock.on('error', err => finish(err instanceof Error ? err : new Error(String(err))))
  })
}

export const defaultSendLlmCallResult: SendLlmCallResult = async payload =>
  sendOneShotResult(MEMORY_TIER_LLM_CALL_RESULT_METHOD, payload)

/**
 * Default embedding-result write-back: one-shot UDS request to
 * `memory.tier.embedding_result` (the orchestrator delivers it to the pending
 * `se_integration::embed()` oneshot by `req_id`).
 */
export const defaultSendEmbeddingResult: SendEmbeddingResult = async payload =>
  sendOneShotResult(MEMORY_TIER_EMBEDDING_RESULT_METHOD, payload)

/**
 * Default rerank-result write-back: one-shot UDS request to
 * `memory.tier.rerank_result` (the orchestrator delivers it to the pending
 * `se_integration::rerank_round_trip()` oneshot by `req_id`).
 */
export const defaultSendRerankResult: SendRerankResult = async payload =>
  sendOneShotResult(MEMORY_TIER_RERANK_RESULT_METHOD, payload)

/**
 * Default SDK-backed LLM call via `sideQuery`. `skipSystemPromptPrefix: true`
 * because the orchestrator already assembled the full message list per its
 * Tier policy (the Tier prompt template is an in-orchestrator Rust constant,
 * not the TUI system prompt). `querySource:
 * 'memory_tier'` keeps this OUT of `FOREGROUND_529_RETRY_SOURCES` (fail-fast
 * 529).
 */
export const defaultRunLlmCall: RunMemoryTierLlmCall = async args => {
  const resp = await defaultSideQuery({
    querySource: 'memory_tier',
    model: args.model,
    skipSystemPromptPrefix: true,
    messages: args.messages.map(m => ({
      role: m.role === 'assistant' ? 'assistant' : 'user',
      content: m.content,
    })) as never,
    ...(args.maxTokens !== undefined ? { max_tokens: args.maxTokens } : {}),
    ...(args.temperature !== undefined ? { temperature: args.temperature } : {}),
    ...(args.signal !== undefined ? { signal: args.signal } : {}),
  })

  const text = Array.isArray(resp.content)
    ? resp.content
        .filter((b): b is { type: 'text'; text: string } => b?.type === 'text')
        .map(b => b.text)
        .join('')
    : ''

  return {
    text,
    ...(resp.usage
      ? {
          usage: {
            input_tokens: resp.usage.input_tokens ?? 0,
            output_tokens: resp.usage.output_tokens ?? 0,
          },
        }
      : {}),
  }
}

/**
 * Embedding-model discovery cache. Models are
 * discovered dynamically via `client.listModels()` +
 * `capabilities.supports_embedding`, never a hardcoded slug. TTL'd so a batch
 * of index upserts doesn't hammer the gateway;
 * short enough that a newly-enabled embedding model is picked up promptly.
 */
const EMBEDDING_MODELS_TTL_MS = 5 * 60_000
let embeddingModelsCache: { at: number; slugs: string[] } | null = null

async function discoverEmbeddingModelSlugs(): Promise<string[]> {
  const now = Date.now()
  if (
    embeddingModelsCache &&
    now - embeddingModelsCache.at < EMBEDDING_MODELS_TTL_MS
  ) {
    return embeddingModelsCache.slugs
  }
  const client = await getAcosmiClient()
  const models = await client.listModels()
  const slugs = models
    .filter(
      m => m.isEnabled !== false && m.capabilities?.supports_embedding === true,
    )
    // ModelID semantics use the slug (`modelId`); UUID (`id`) is a fallback.
    .map(m => m.modelId ?? m.id)
    .filter((s): s is string => typeof s === 'string' && s.length > 0)
  embeddingModelsCache = { at: now, slugs }
  return slugs
}

/** Test-only: reset the embedding-model discovery cache. */
export function _testOnly_resetEmbeddingModelCache(): void {
  embeddingModelsCache = null
}

/**
 * Rerank-model discovery cache. Models are discovered
 * dynamically via `client.listModels()` + `capabilities.supports_rerank` —
 * never a hardcoded slug. Same TTL rationale as the
 * embedding cache.
 */
let rerankModelsCache: { at: number; slugs: string[] } | null = null

async function discoverRerankModelSlugs(): Promise<string[]> {
  const now = Date.now()
  if (rerankModelsCache && now - rerankModelsCache.at < EMBEDDING_MODELS_TTL_MS) {
    return rerankModelsCache.slugs
  }
  const client = await getAcosmiClient()
  const models = await client.listModels()
  const slugs = models
    .filter(
      m => m.isEnabled !== false && m.capabilities?.supports_rerank === true,
    )
    // ModelID semantics use the slug (`modelId`); UUID (`id`) is a fallback.
    .map(m => m.modelId ?? m.id)
    .filter((s): s is string => typeof s === 'string' && s.length > 0)
  rerankModelsCache = { at: now, slugs }
  return slugs
}

/** Test-only: reset the rerank-model discovery cache. */
export function _testOnly_resetRerankModelCache(): void {
  rerankModelsCache = null
}

/**
 * Default SDK-backed embedding call: discovers a `supports_embedding` managed
 * model (unless the request pinned one via `model_hint`), calls
 * `client.embeddings()`, and returns index-aligned vectors. Throws with an
 * honest message when no embedding model is available — the proxy writes that
 * back as `{ error }` and the Rust side falls back to lexical-only retrieval.
 */
export const defaultRunEmbedding: RunMemoryTierEmbedding = async args => {
  const slugs = args.modelHint
    ? [args.modelHint]
    : await discoverEmbeddingModelSlugs()
  const model = slugs[0]
  if (!model) {
    throw new Error(
      'no managed model with supports_embedding=true is available',
    )
  }
  const client = await getAcosmiClient()
  const resp = await client.embeddings(model, { input: args.texts })
  const data = [...(resp.data ?? [])].sort(
    (a, b) => (a.index ?? 0) - (b.index ?? 0),
  )
  const embeddings = data.map(d => d.embedding)
  if (
    embeddings.length !== args.texts.length ||
    embeddings.some(e => !Array.isArray(e) || e.length === 0)
  ) {
    throw new Error(
      `embedding response mismatch: ${embeddings.length} vectors for ${args.texts.length} texts`,
    )
  }
  const dimension = embeddings[0]!.length
  if (embeddings.some(e => e.length !== dimension)) {
    throw new Error('embedding response mismatch: inconsistent dimensions')
  }
  return { embeddings, dimension }
}

/**
 * Default SDK-backed rerank call: discovers a `supports_rerank` managed model
 * (unless the request pinned one via `model_hint`), calls `client.rerank()`,
 * and returns `{index, score}` entries. Throws with an honest message when no
 * rerank model is available — the proxy writes that back as `{ error }` and
 * the orchestrator keeps its RRF fusion order.
 */
export const defaultRunRerank: RunMemoryTierRerank = async args => {
  const slugs = args.modelHint
    ? [args.modelHint]
    : await discoverRerankModelSlugs()
  const model = slugs[0]
  if (!model) {
    throw new Error('no managed model with supports_rerank=true is available')
  }
  const client = await getAcosmiClient()
  const resp = await client.rerank(model, {
    query: args.query,
    documents: args.documents,
  })
  const ranking = (resp.results ?? [])
    .filter(
      r =>
        Number.isInteger(r.index) &&
        r.index >= 0 &&
        r.index < args.documents.length &&
        Number.isFinite(r.relevance_score),
    )
    .map(r => ({ index: r.index, score: r.relevance_score }))
  return { ranking }
}

export const DEFAULT_MEMORY_TIER_PROXY_DEPS: MemoryTierProxyDeps = {
  // Wrapped (not a bare reference) to dodge the import cycle's TDZ: this
  // module and `memory-bootstrap` import each other, so capturing the function
  // value at module-eval time could read it before its declaration is live.
  // The arrow defers the read to call time, by which point both modules are
  // fully evaluated.
  isLeaderHeld: () => isLeaderHeld(),
  getMainLoopModel,
  subscribeToOrchestratorEvents: defaultSubscribeToOrchestratorEvents,
  sendLlmCallResult: defaultSendLlmCallResult,
  runLlmCall: defaultRunLlmCall,
  sendEmbeddingResult: defaultSendEmbeddingResult,
  runEmbedding: defaultRunEmbedding,
  sendRerankResult: defaultSendRerankResult,
  runRerank: defaultRunRerank,
}
