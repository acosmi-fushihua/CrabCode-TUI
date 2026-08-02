/**
 * Injectable dependencies for memory Tier evidence gathering. The proxy uses
 * the local orchestrator socket and wraps bounded WebSearch/WebFetch results as
 * timestamped evidence. Failures return an explicit error, never a phantom
 * success.
 */

import { open, readdir, realpath, stat } from 'node:fs/promises'
import { createConnection, type Socket } from 'node:net'
import { basename, extname, isAbsolute, relative, sep } from 'node:path'
import { executeSearch } from '../../services/search/search.js'
import { getURLMarkdownContent, type FetchedContent } from '../../tools/WebFetchTool/utils.js'
import { isLeaderHeld } from './leaderBootstrap.js'
import type { MemoryTierToolEvidence } from './toolTypes.js'

const MEMORY_IPC_ENDPOINT_ENV = 'CRABCODE_MEMORY_IPC_ENDPOINT'

/** Tool-result write-back IPC request timeout. */
const RESULT_REQUEST_TIMEOUT_MS = 5_000

/** Max evidence body chars per item (defensive bound; tool output can be large). */
const MAX_EVIDENCE_CONTENT_CHARS = 4_000

/** Max search hits turned into evidence per query. */
const MAX_SEARCH_EVIDENCE = 10

/**
 * W-MEMORY-LIFECYCLE K10 — hard limits for the read-only watch filesystem
 * kinds (`readFile` / `listDir`). The orchestrator only emits these for
 * dream-watch (专项检测) runs anchored to a user-configured watch root; the
 * TS executor re-enforces every bound locally (never trust the frame).
 */
/** Max bytes of file content returned per `readFile` call (64 KiB). */
export const MEMORY_TIER_READ_FILE_MAX_BYTES = 64 * 1024
/** Max directory entries returned per `listDir` call. */
export const MEMORY_TIER_LIST_DIR_MAX_ENTRIES = 200
/**
 * Text-like extensions the watch `readFile` executor may read. Whitelist-only:
 * extensionless files (Makefile, Dockerfile), dotfiles, and anything
 * binary-ish are refused. `.env` is deliberately absent (secrets).
 */
export const MEMORY_TIER_READABLE_EXTENSIONS: ReadonlySet<string> = new Set([
  '.md',
  '.markdown',
  '.txt',
  '.rs',
  '.ts',
  '.tsx',
  '.js',
  '.jsx',
  '.mjs',
  '.cjs',
  '.json',
  '.toml',
  '.yaml',
  '.yml',
  '.py',
  '.go',
  '.java',
  '.kt',
  '.c',
  '.h',
  '.cc',
  '.cpp',
  '.hpp',
  '.cs',
  '.rb',
  '.php',
  '.swift',
  '.css',
  '.scss',
  '.less',
  '.html',
  '.htm',
  '.xml',
  '.sql',
  '.sh',
  '.bash',
  '.zsh',
  '.ps1',
  '.psm1',
  '.bat',
  '.cmd',
  '.ini',
  '.cfg',
  '.conf',
  '.proto',
  '.graphql',
  '.vue',
  '.svelte',
])

/**
 * Run a web search; returns evidence items (one per hit). May be empty.
 * `nowMs` is the proxy's wall-clock stamp applied to each evidence item — the
 * tool carries no timestamp of its own, so the proxy stamps it.
 */
export type RunMemoryTierWebSearch = (args: {
  query: string
  nowMs: number
  signal?: AbortSignal
}) => Promise<MemoryTierToolEvidence[]>

/** Run a web fetch; returns evidence items (typically one). May be empty. */
export type RunMemoryTierWebFetch = (args: {
  url: string
  nowMs: number
  signal?: AbortSignal
}) => Promise<MemoryTierToolEvidence[]>

/**
 * Read a text file inside a watch root (W-MEMORY-LIFECYCLE K10). Returns one
 * evidence item whose `content` is the (possibly truncated) file text. Throws
 * on any guard violation — the caller records it as the request `error`.
 */
export type RunMemoryTierReadFile = (args: {
  path: string
  root: string
  nowMs: number
}) => Promise<MemoryTierToolEvidence[]>

/**
 * List a directory inside a watch root (W-MEMORY-LIFECYCLE K10). Returns one
 * evidence item whose `content` is `JSON.stringify({entries[, truncated]})`.
 * Throws on any guard violation — the caller records it as the request
 * `error`.
 */
export type RunMemoryTierListDir = (args: {
  path: string
  root: string
  nowMs: number
}) => Promise<MemoryTierToolEvidence[]>

/** Send a one-shot tool-result write-back request to the orchestrator. */
export type SendToolCallResult = (payload: {
  req_id: string
  evidence: MemoryTierToolEvidence[]
  error?: string
}) => Promise<void>

export type MemoryTierToolProxyDeps = {
  /** Leader gate: only the leader worker executes tool calls. */
  isLeaderHeld: () => boolean
  /** Wall-clock now (ms). Injectable so tests assert a deterministic stamp. */
  nowMs: () => number
  /** Web search executor. */
  runWebSearch: RunMemoryTierWebSearch
  /** Web fetch executor. */
  runWebFetch: RunMemoryTierWebFetch
  /** Root-guarded watch file reader (K10). */
  runReadFile: RunMemoryTierReadFile
  /** Root-guarded watch directory lister (K10). */
  runListDir: RunMemoryTierListDir
  /** One-shot tool-result write-back. */
  sendToolCallResult: SendToolCallResult
}

function truncate(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max)
}

export function resolveIpcEndpointPath(): string | null {
  const endpoint = process.env[MEMORY_IPC_ENDPOINT_ENV]
  if (!endpoint) return null
  if (endpoint.startsWith('unix:')) {
    const path = endpoint.slice('unix:'.length)
    return path.length > 0 ? path : null
  }
  if (endpoint.startsWith('npipe:')) {
    // Windows named pipe (mirrors `deps.ts::resolveIpcEndpointPath` and the
    // Rust pump `memory_events_pump.rs::resolve_npipe_path`). `createConnection`
    // accepts a `\\.\pipe\name` path identically to a Unix socket — the
    // previous `return null` silently killed the Windows tool reverse bridge.
    const pipe = endpoint.slice('npipe:'.length)
    return pipe.length > 0 ? pipe : null
  }
  return null
}

/**
 * Default web search: runs the in-process search provider (`executeSearch`,
 * the same primitive `WebSearchTool` Mode B uses) and wraps each hit into an
 * evidence item. `fetched_at_ms` is stamped per-hit at execution time.
 */
export const defaultRunWebSearch: RunMemoryTierWebSearch = async args => {
  const response = await executeSearch(
    args.query,
    {},
    args.signal,
  )
  if (response === null) return [] // no search provider configured
  const fetchedAtMs = args.nowMs
  return response.results.slice(0, MAX_SEARCH_EVIDENCE).map(r => ({
    source_url: r.url,
    fetched_at_ms: fetchedAtMs,
    content: truncate(r.snippet || r.content || '', MAX_EVIDENCE_CONTENT_CHARS),
    ...(r.title ? { title: r.title } : {}),
  }))
}

/**
 * Default web fetch: fetches markdown content (`getURLMarkdownContent`, the
 * same primitive `WebFetchTool.call` uses) and wraps it into a single evidence
 * item. Redirects are surfaced as evidence content (so the orchestrator can see
 * the hop) but keep the original URL as `source_url`. `fetched_at_ms` is
 * stamped at execution time.
 */
export const defaultRunWebFetch: RunMemoryTierWebFetch = async args => {
  const abortController = new AbortController()
  if (args.signal) {
    if (args.signal.aborted) abortController.abort()
    else args.signal.addEventListener('abort', () => abortController.abort(), { once: true })
  }
  const response = await getURLMarkdownContent(args.url, abortController)
  const fetchedAtMs = args.nowMs

  if ('type' in response && response.type === 'redirect') {
    return [
      {
        source_url: args.url,
        fetched_at_ms: fetchedAtMs,
        content: truncate(
          `Redirect ${response.statusCode} → ${response.redirectUrl}`,
          MAX_EVIDENCE_CONTENT_CHARS,
        ),
      },
    ]
  }

  // Not a redirect → FetchedContent (mirrors WebFetchTool.call's cast).
  const { content: fetchedContent } = response as FetchedContent
  const content = truncate(fetchedContent ?? '', MAX_EVIDENCE_CONTENT_CHARS)
  if (content.length === 0) return []
  return [
    {
      source_url: args.url,
      fetched_at_ms: fetchedAtMs,
      content,
    },
  ]
}

/**
 * Canonicalize `targetPath` and require it to stay inside the canonicalized
 * `rootPath` (W-MEMORY-LIFECYCLE K10 containment guard). `realpath` resolves
 * symlinks first, so a link inside the root pointing outside it is rejected.
 * Containment uses `path.relative`, which is case-insensitive on Windows
 * (matching the case-insensitive filesystem), and rejects when the relative
 * path escapes (`..` first segment) or lands on another drive (absolute).
 */
async function resolveInsideWatchRoot(
  targetPath: string,
  rootPath: string,
): Promise<string> {
  const rootReal = await realpath(rootPath)
  const targetReal = await realpath(targetPath)
  const rel = relative(rootReal, targetReal)
  if (rel === '..' || rel.startsWith(`..${sep}`) || isAbsolute(rel)) {
    throw new Error(
      'memory tier fs call rejected: path resolves outside the watch root',
    )
  }
  return targetReal
}

/**
 * Default watch `readFile`: root-contained, whitelist-extension, ≤64KiB read
 * (truncated content carries an explicit marker so the orchestrator never
 * mistakes a prefix for the whole file). Reads through a bounded file handle,
 * so an oversized file never fully loads.
 */
export const defaultRunReadFile: RunMemoryTierReadFile = async args => {
  const real = await resolveInsideWatchRoot(args.path, args.root)
  const ext = extname(real).toLowerCase()
  if (!MEMORY_TIER_READABLE_EXTENSIONS.has(ext)) {
    throw new Error(
      `memory tier readFile rejected: extension not allowed (${ext || 'none'})`,
    )
  }
  const info = await stat(real)
  if (!info.isFile()) {
    throw new Error('memory tier readFile rejected: not a regular file')
  }

  const handle = await open(real, 'r')
  let text: string
  try {
    const buf = Buffer.alloc(
      Math.min(info.size, MEMORY_TIER_READ_FILE_MAX_BYTES),
    )
    const { bytesRead } = await handle.read(buf, 0, buf.length, 0)
    text = buf.subarray(0, bytesRead).toString('utf8')
  } finally {
    await handle.close()
  }

  const truncated = info.size > MEMORY_TIER_READ_FILE_MAX_BYTES
  const content = truncated
    ? `${text}\n\n[truncated: file is ${info.size} bytes, read limit ${MEMORY_TIER_READ_FILE_MAX_BYTES}]`
    : text
  return [
    {
      source_url: real,
      fetched_at_ms: args.nowMs,
      content,
      title: basename(real),
    },
  ]
}

/**
 * Default watch `listDir`: root-contained, names only (no stat fan-out),
 * capped at MEMORY_TIER_LIST_DIR_MAX_ENTRIES with a `truncated` flag. Sorted
 * for deterministic output across platforms.
 */
export const defaultRunListDir: RunMemoryTierListDir = async args => {
  const real = await resolveInsideWatchRoot(args.path, args.root)
  const names = (await readdir(real)).sort()
  const entries = names.slice(0, MEMORY_TIER_LIST_DIR_MAX_ENTRIES)
  const payload: { entries: string[]; truncated?: boolean } = { entries }
  if (names.length > MEMORY_TIER_LIST_DIR_MAX_ENTRIES) {
    payload.truncated = true
  }
  return [
    {
      source_url: real,
      fetched_at_ms: args.nowMs,
      content: JSON.stringify(payload),
      title: basename(real) || real,
    },
  ]
}

/**
 * Default tool-result write-back: one-shot UDS request to
 * `memory.tier.tool_call_result` (same shape `dispatcher/memory.rs` forwards).
 * Mirrors `deps.ts::defaultSendLlmCallResult`.
 */
export const defaultSendToolCallResult: SendToolCallResult = async payload => {
  const path = resolveIpcEndpointPath()
  if (path === null) {
    throw new Error(
      `${MEMORY_IPC_ENDPOINT_ENV} not set / unsupported transport; cannot write Tier tool result back`,
    )
  }
  const request =
    JSON.stringify({ method: 'memory.tier.tool_call_result', payload }) + '\n'

  await new Promise<void>((resolve, reject) => {
    const sock: Socket = createConnection(path)
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
      () =>
        finish(
          new Error(
            `memory.tier.tool_call_result timed out after ${RESULT_REQUEST_TIMEOUT_MS}ms`,
          ),
        ),
      RESULT_REQUEST_TIMEOUT_MS,
    )
    timer.unref?.()
    sock.on('connect', () => {
      sock.write(request)
    })
    sock.on('data', () => finish(null))
    sock.on('end', () => finish(null))
    sock.on('error', err => finish(err instanceof Error ? err : new Error(String(err))))
  })
}

export const DEFAULT_MEMORY_TIER_TOOL_PROXY_DEPS: MemoryTierToolProxyDeps = {
  // Wrapped (not a bare reference) to dodge the import cycle's TDZ — same
  // rationale as `deps.ts::DEFAULT_MEMORY_TIER_PROXY_DEPS`.
  isLeaderHeld: () => isLeaderHeld(),
  nowMs: () => Date.now(),
  runWebSearch: defaultRunWebSearch,
  runWebFetch: defaultRunWebFetch,
  runReadFile: defaultRunReadFile,
  runListDir: defaultRunListDir,
  sendToolCallResult: defaultSendToolCallResult,
}
