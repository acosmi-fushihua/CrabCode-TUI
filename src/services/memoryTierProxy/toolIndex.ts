/**
 * Memory Tier reverse-IPC evidence-gathering proxy.
 *
 * Companion to the LLM channel (`./index.ts`). The Rust memory orchestrator
 * drives Tier-3 imagination but delegates external tool execution (web search /
 * web fetch) to the TypeScript runtime. This proxy
 * is the missing TS consumer of the `memory/tier/toolCallRequest` frame: for
 * each request it (leader-gated) runs `WebSearchTool` / `WebFetchTool` building
 * blocks, wraps the output into traceable evidence (with `source_url` +
 * `fetched_at_ms`), and writes it back via `memory.tier.tool_call_result`.
 *
 * The frame includes two read-only filesystem kinds for dream-watch (专项检测)
 * code-understanding evidence:
 * `readFile` / `listDir`, root-guarded and budget-capped in `./toolDeps.ts`
 * (≤ MAX_FS_CALLS_PER_REQUEST combined per frame here). Unknown kinds still
 * drop the whole frame.
 *
 * # Leader gating
 *
 * Same as the LLM channel: only the memory leader executes, so the same
 * `req_id` is never run twice (which would double-spend search quota AND race
 * two write-backs).
 *
 * # Concurrency budget
 *
 * A tiny module-local FIFO limiter (default 1) caps in-flight evidence-gathering
 * runs. It deliberately uses a separate limiter from AgentTool because memory
 * Tier work is its own low-rate background lane.
 */

import type { SubscribeToOrchestratorEvents } from './deps.js'
import { defaultSubscribeToOrchestratorEvents } from './deps.js'
import {
  DEFAULT_MEMORY_TIER_TOOL_PROXY_DEPS,
  type MemoryTierToolProxyDeps,
} from './toolDeps.js'
import {
  MEMORY_TIER_TOOL_CALL_REQUEST_NOTIFICATION,
  type MemoryTierToolCall,
  type MemoryTierToolCallRequestPayload,
  type MemoryTierToolEvidence,
} from './toolTypes.js'

/** Default in-flight evidence-gathering concurrency budget. */
const DEFAULT_MAX_CONCURRENT = 1

/**
 * W-MEMORY-LIFECYCLE K10 — max `readFile` + `listDir` calls executed per
 * request frame (combined). Excess calls are refused with a recorded error
 * instead of executed, bounding what a single watch run can pull off disk.
 */
export const MAX_FS_CALLS_PER_REQUEST = 8

function logStderr(msg: string): void {
  process.stderr.write(`[memory-tier-tool-proxy] ${msg}\n`)
}

/**
 * Minimal FIFO concurrency limiter (no external `p-limit` dependency). Caps
 * `limit` simultaneous in-flight tasks; excess queue and run FIFO. Mirrors
 * `./index.ts::createLimiter`.
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
 * Parse an inbound orchestrator events frame into a tool-call request payload,
 * or `null` if the frame is not a (well-formed) `memory/tier/toolCallRequest`.
 * Reads fields defensively so a malformed frame is skipped, not crashed on.
 */
export function parseToolCallRequestFrame(
  frame: unknown,
): MemoryTierToolCallRequestPayload | null {
  if (!isRecord(frame)) return null
  if (frame.notification !== MEMORY_TIER_TOOL_CALL_REQUEST_NOTIFICATION) return null
  const payload = frame.payload
  if (!isRecord(payload)) return null

  const reqId = payload.req_id
  const tier = payload.tier
  const calls = payload.calls
  if (typeof reqId !== 'string' || reqId.length === 0) return null
  if (typeof tier !== 'string') return null
  if (!Array.isArray(calls)) return null

  const parsedCalls: MemoryTierToolCall[] = []
  for (const c of calls) {
    if (!isRecord(c)) return null
    const kind = c.kind
    // Tolerant of camel / snake / Pascal (mirrors the orchestrator pump).
    // readFile / listDir are the K10 watch filesystem kinds; any OTHER kind
    // still drops the whole frame (unknown-kind behavior unchanged).
    let normKind: MemoryTierToolCall['kind'] | null = null
    if (kind === 'webSearch' || kind === 'web_search' || kind === 'WebSearch')
      normKind = 'webSearch'
    else if (kind === 'webFetch' || kind === 'web_fetch' || kind === 'WebFetch')
      normKind = 'webFetch'
    else if (kind === 'readFile' || kind === 'read_file' || kind === 'ReadFile')
      normKind = 'readFile'
    else if (kind === 'listDir' || kind === 'list_dir' || kind === 'ListDir')
      normKind = 'listDir'
    if (normKind === null) return null
    parsedCalls.push({
      kind: normKind,
      ...(typeof c.query === 'string' ? { query: c.query } : {}),
      ...(typeof c.url === 'string' ? { url: c.url } : {}),
      ...(typeof c.path === 'string' ? { path: c.path } : {}),
      ...(typeof c.root === 'string' ? { root: c.root } : {}),
    })
  }

  return { req_id: reqId, tier, calls: parsedCalls }
}

/**
 * Run one tool call → evidence list. Dispatches on `kind`. Never throws: on
 * failure it rethrows nothing but returns `[]` and the outer handler records
 * the error per-request. (We let it throw HERE so `handleToolCallRequest` can
 * attribute the error string; see below.)
 */
async function runOneCall(
  call: MemoryTierToolCall,
  deps: MemoryTierToolProxyDeps,
  nowMs: number,
): Promise<MemoryTierToolEvidence[]> {
  if (call.kind === 'webSearch') {
    const query = call.query
    if (typeof query !== 'string' || query.length === 0) return []
    return deps.runWebSearch({ query, nowMs })
  }
  if (call.kind === 'webFetch') {
    const url = call.url
    if (typeof url !== 'string' || url.length === 0) return []
    return deps.runWebFetch({ url, nowMs })
  }
  // readFile / listDir (W-MEMORY-LIFECYCLE K10): unlike the web kinds, a
  // missing field is an observable error rather than a silent skip — the fs
  // kinds are security-guarded and the orchestrator must see the refusal.
  const { path, root } = call
  if (
    typeof path !== 'string' ||
    path.length === 0 ||
    typeof root !== 'string' ||
    root.length === 0
  ) {
    throw new Error(`memory tier ${call.kind} call missing path/root`)
  }
  return call.kind === 'readFile'
    ? deps.runReadFile({ path, root, nowMs })
    : deps.runListDir({ path, root, nowMs })
}

/**
 * Handle a single parsed request: run each tool call, aggregate evidence, and
 * write the result back. Exported for unit tests. Never throws — a tool failure
 * is written back as `{ evidence: [], error }` (a failed gather MUST NOT be
 * reported as a phantom success).
 *
 * Per-call timestamping: the `fetched_at_ms` stamp is applied INSIDE the
 * `runWebSearch` / `runWebFetch` impls at execution time (the tools carry no
 * timestamp of their own).
 */
export async function handleToolCallRequest(
  request: MemoryTierToolCallRequestPayload,
  deps: MemoryTierToolProxyDeps,
): Promise<void> {
  const evidence: MemoryTierToolEvidence[] = []
  let error: string | undefined
  // Stamp once per request so all evidence in a batch shares the gather time.
  const nowMs = deps.nowMs()
  // W-MEMORY-LIFECYCLE K10: per-frame filesystem budget. readFile + listDir
  // calls beyond MAX_FS_CALLS_PER_REQUEST are refused (recorded as an error,
  // never executed); web kinds are not counted against it.
  let fsCalls = 0

  for (const call of request.calls) {
    try {
      if (call.kind === 'readFile' || call.kind === 'listDir') {
        fsCalls += 1
        if (fsCalls > MAX_FS_CALLS_PER_REQUEST) {
          throw new Error(
            `memory tier fs call budget exceeded (max ${MAX_FS_CALLS_PER_REQUEST} readFile/listDir calls per request)`,
          )
        }
      }
      const items = await runOneCall(call, deps, nowMs)
      evidence.push(...items)
    } catch (err) {
      // First error wins for the aggregate `error` field, but we keep any
      // evidence already gathered from prior calls (partial success is honest).
      const msg = err instanceof Error ? err.message : String(err)
      error = error ?? msg
    }
  }

  try {
    await deps.sendToolCallResult({
      req_id: request.req_id,
      evidence,
      ...(error !== undefined ? { error } : {}),
    })
  } catch (err) {
    logStderr(
      `tool result write-back failed for req_id=${request.req_id}: ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

type ProxyHandle = {
  dispose: () => void
}

let installedHandle: ProxyHandle | null = null

export type StartMemoryTierToolProxyOptions = {
  deps?: MemoryTierToolProxyDeps
  /** Orchestrator events subscription (defaults to the shared UDS subscriber). */
  subscribeToOrchestratorEvents?: SubscribeToOrchestratorEvents
  maxConcurrent?: number
}

/**
 * Install the memory Tier tool-evidence proxy subscription. Idempotent — a
 * second call while a subscription is live is a no-op. The caller installs this
 * only on the leader worker, but the per-frame leader gate is also checked so a
 * lease lost mid-flight stops new executions immediately.
 */
export function startMemoryTierToolProxy(
  options: StartMemoryTierToolProxyOptions = {},
): ProxyHandle {
  if (installedHandle) return installedHandle

  const deps = options.deps ?? DEFAULT_MEMORY_TIER_TOOL_PROXY_DEPS
  const subscribe =
    options.subscribeToOrchestratorEvents ?? defaultSubscribeToOrchestratorEvents
  const limit = createLimiter(options.maxConcurrent ?? DEFAULT_MAX_CONCURRENT)

  const unsubscribe = subscribe(frame => {
    // Leader gate: only the lease holder executes. Checked per-frame so a lease
    // handed off mid-run stops new executions immediately.
    if (!deps.isLeaderHeld()) return
    const request = parseToolCallRequestFrame(frame)
    if (request === null) return
    void limit(() => handleToolCallRequest(request, deps))
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

/** Tear down the tool proxy subscription (idempotent). */
export function stopMemoryTierToolProxy(): void {
  installedHandle?.dispose()
  installedHandle = null
}

/** Test-only: is a subscription currently installed? */
export function _testOnly_isToolProxyInstalled(): boolean {
  return installedHandle !== null
}
