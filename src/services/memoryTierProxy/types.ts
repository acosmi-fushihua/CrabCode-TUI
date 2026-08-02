/**
 * Wire shapes for the memory Tier reverse-IPC LLM channel. These snake_case
 * fields mirror the Rust orchestrator structs and are the local socket
 * contract used by the TUI runtime.
 */

/** One LLM message (role + content) — orchestrator `tier::LlmMessage`. */
export type MemoryTierLlmMessage = {
  role: string
  content: string
}

/** LLM call params — orchestrator `tier::LlmCallParams` (snake_case). */
export type MemoryTierLlmCallParams = {
  temperature?: number
  max_tokens?: number
}

/**
 * Reverse-IPC LLM call request payload pushed by the orchestrator
 * (`tier::LlmCallRequestPayload`, snake_case wire).
 */
export type MemoryTierLlmCallRequestPayload = {
  req_id: string
  /** Tier discriminant, PascalCase on the wire (`Session`/`Extract`/`Dream`). */
  tier: string
  phase?: string
  messages: MemoryTierLlmMessage[]
  /** `undefined` → use the main loop model. Never a hardcoded brand literal. */
  model_hint?: string
  params: MemoryTierLlmCallParams
}

/** Token usage — orchestrator `tier::LlmUsage`. */
export type MemoryTierLlmUsage = {
  input_tokens: number
  output_tokens: number
}

/**
 * Result payload written back to the orchestrator
 * (`tier::LlmCallResultPayload`, snake_case wire). Either `response` is set
 * (success) OR `error` is set (failure) — never neither.
 */
export type MemoryTierLlmCallResultPayload = {
  req_id: string
  response?: string
  usage?: MemoryTierLlmUsage
  error?: string
}

/** Method name for the result write-back orchestrator IPC request. */
export const MEMORY_TIER_LLM_CALL_RESULT_METHOD = 'memory.tier.llm_call_result'

/** Frame name the orchestrator uses for the reverse-IPC LLM request push. */
export const MEMORY_TIER_LLM_CALL_REQUEST_NOTIFICATION =
  'memory/tier/llmCallRequest'

// ──────────────────────────────────────────────────────────────────────────
// Embedding channel (W-MEMORY-ALIVE PR-2a, 2026-07-01 — Phase B 解冻,
// 用户裁决③修订 §15-7: embedding 产生走 SDK `client.embeddings()`, 向量存储
// 与检索仍在本地 acosmi-memory-se). These mirror the orchestrator's
// snake_case wire structs verbatim
// (`se_integration.rs::EmbeddingRequestPayload` / `EmbeddingResultPayload`).
// ──────────────────────────────────────────────────────────────────────────

/**
 * Reverse-IPC embedding request payload pushed by the orchestrator
 * (`se_integration.rs::EmbeddingRequestPayload`, snake_case wire).
 * `text_keys[i]` pairs with `texts[i]` (orchestrator-side chunk keys).
 */
export type MemoryTierEmbeddingRequestPayload = {
  req_id: string
  texts: string[]
  text_keys: string[]
  /** `undefined` → TS discovers a `supports_embedding` model. Never a hardcoded brand literal. */
  model_hint?: string
}

/** One embedding vector (`se_integration.rs::EmbeddingVector`). */
export type MemoryTierEmbeddingVector = {
  values: number[]
}

/**
 * Result payload written back to the orchestrator
 * (`se_integration.rs::EmbeddingResultPayload`, snake_case wire). Success:
 * `embeddings[i]` pairs with request `texts[i]`, `dimension` > 0. Failure:
 * empty `embeddings` + `dimension: 0` + `error` set.
 */
export type MemoryTierEmbeddingResultPayload = {
  req_id: string
  embeddings: MemoryTierEmbeddingVector[]
  dimension: number
  error?: string
}

/** Method name for the embedding result write-back orchestrator IPC request. */
export const MEMORY_TIER_EMBEDDING_RESULT_METHOD =
  'memory.tier.embedding_result'

/** Frame name the orchestrator uses for the reverse-IPC embedding push. */
export const MEMORY_TIER_EMBEDDING_REQUEST_NOTIFICATION =
  'memory/tier/embeddingRequest'

// ──────────────────────────────────────────────────────────────────────────
// Rerank channel (W-MEMORY-KB-UPLIFT P1, 2026-07-17 — `memory.search` 跨
// scope 交叉编码重排). These mirror the orchestrator's snake_case wire
// structs verbatim (`se_integration.rs::RerankRequestPayload` /
// `RerankResultPayload`). SDK endpoint = `client.rerank()`, gated on
// `capabilities.supports_rerank` — never a hardcoded brand literal.
// ──────────────────────────────────────────────────────────────────────────

/**
 * Reverse-IPC rerank request payload pushed by the orchestrator
 * (`se_integration.rs::RerankRequestPayload`, snake_case wire).
 * `documents[i]` are candidate texts; result `ranking[].index` refers to
 * these positions.
 */
export type MemoryTierRerankRequestPayload = {
  req_id: string
  query: string
  documents: string[]
  /** `undefined` → TS discovers a `supports_rerank` model. Never a hardcoded brand literal. */
  model_hint?: string
}

/** One rerank ranking entry (`se_integration.rs::RerankEntry`). */
export type MemoryTierRerankEntry = {
  index: number
  score: number
}

/**
 * Result payload written back to the orchestrator
 * (`se_integration.rs::RerankResultPayload`, snake_case wire). Success:
 * `ranking` entries cover the scored candidates. Failure: empty `ranking` +
 * `error` set (the orchestrator fail-softs to its RRF fusion order).
 */
export type MemoryTierRerankResultPayload = {
  req_id: string
  ranking: MemoryTierRerankEntry[]
  error?: string
}

/** Method name for the rerank result write-back orchestrator IPC request. */
export const MEMORY_TIER_RERANK_RESULT_METHOD = 'memory.tier.rerank_result'

/** Frame name the orchestrator uses for the reverse-IPC rerank push. */
export const MEMORY_TIER_RERANK_REQUEST_NOTIFICATION =
  'memory/tier/rerankRequest'
