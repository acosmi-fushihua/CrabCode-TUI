/**
 * Wire shapes for the memory Tier reverse-IPC evidence-gathering channel.
 *
 * The Rust memory orchestrator (`acosmi-memory-orchestrator`) drives Tier-3
 * imagination's confidence pipeline, while external tools (web search / web
 * fetch) execute in the TypeScript runtime. The orchestrator emits a
 * `memory/tier/toolCallRequest`
 * reverse-IPC frame; the leader worker runs WebSearchTool / WebFetchTool and
 * writes the gathered evidence back via a `memory.tier.tool_call_result`
 * orchestrator IPC request.
 *
 * These types mirror the orchestrator's wire structs
 * (`tier::tier3_imagination::{ToolCallRequestPayload, ToolCallResultPayload,
 * ToolEvidence}` in
 * `libs/acosmi-memory/acosmi-memory-orchestrator/src/tier/tier3_imagination.rs`)
 * AND the protocol `MemoryTier{ToolCall,ToolCallRequestNotification,
 * ToolEvidence,ToolCallResultParams}`. The proxy talks to the orchestrator UDS
 * directly over the local orchestrator socket.
 *
 * `kind` is camelCase on the wire (`"webSearch"` / `"webFetch"` /
 * `"readFile"` / `"listDir"`); the request payload is snake_case (`req_id`);
 * evidence is snake_case (`source_url` / `fetched_at_ms`). The orchestrator
 * pump tolerates both camel and snake `kind` but emits camelCase, so we match
 * camelCase here.
 *
 * Two read-only filesystem kinds support the 专项检测 (dream-watch) evidence
 * lane: `readFile` and `listDir`, each
 * carrying `path` + `root`. The TS executors enforce hard limits (canonical
 * path inside `root`, text-only extensions, ≤64KiB per file, ≤200 dir
 * entries, ≤8 fs calls per request frame). Result encoding on the evidence
 * wire: `readFile` puts the (possibly truncated) file text in
 * `evidence.content`; `listDir` puts `JSON.stringify({entries[, truncated]})`
 * there. Unknown kinds still drop the whole frame.
 */

/** Tool kind discriminant (camelCase wire). */
export type MemoryTierToolKind = 'webSearch' | 'webFetch' | 'readFile' | 'listDir'

/** One tool call request — orchestrator `ToolCall` / protocol `MemoryTierToolCall`. */
export type MemoryTierToolCall = {
  kind: MemoryTierToolKind
  /** Search query (`webSearch` only). */
  query?: string
  /** Target URL (`webFetch` only). */
  url?: string
  /** Absolute file (`readFile`) or directory (`listDir`) path to inspect. */
  path?: string
  /** Watch root the canonicalized `path` must stay within (`readFile` / `listDir`). */
  root?: string
}

/**
 * Reverse-IPC tool-call request payload pushed by the orchestrator
 * (`ToolCallRequestPayload`, snake_case wire).
 */
export type MemoryTierToolCallRequestPayload = {
  req_id: string
  /** Tier discriminant, PascalCase on the wire (always `Dream` for imagination). */
  tier: string
  calls: MemoryTierToolCall[]
}

/**
 * One gathered evidence item (reliable + traceable). Mirrors orchestrator
 * `ToolEvidence` / protocol `MemoryTierToolEvidence` (snake_case wire).
 *
 * `fetched_at_ms` is stamped by THIS proxy at execution time — the underlying
 * tools carry no timestamp of their own.
 */
export type MemoryTierToolEvidence = {
  source_url: string
  fetched_at_ms: number
  content: string
  title?: string
}

/**
 * Result payload written back to the orchestrator (`ToolCallResultPayload`,
 * snake_case wire). On failure `evidence` is `[]` and `error` is set; an empty
 * `evidence` list alone is NOT a failure (there may genuinely be no hits).
 */
export type MemoryTierToolCallResultPayload = {
  req_id: string
  evidence: MemoryTierToolEvidence[]
  error?: string
}

/** Method name for the tool-result write-back orchestrator IPC request. */
export const MEMORY_TIER_TOOL_CALL_RESULT_METHOD = 'memory.tier.tool_call_result'

/** Frame name the orchestrator uses for the reverse-IPC tool request push. */
export const MEMORY_TIER_TOOL_CALL_REQUEST_NOTIFICATION =
  'memory/tier/toolCallRequest'
