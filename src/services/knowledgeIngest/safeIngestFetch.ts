/**
 * W-MEMORY-KB-UPLIFT P4 (2026-07-17) — hardened URL fetch for knowledge
 * ingestion. This is a THIN reuse layer over the battle-tested WebFetch SSRF
 * stack (决策 D7 — no second implementation):
 *
 *   - `assertWebFetchUrlAllowed` — DNS-free pre-checks (scheme allowlist,
 *     no embedded credentials, metadata-hostname / IP-literal blocklists),
 *     re-run on EVERY redirect hop;
 *   - `webFetchGuardedLookup` — connect-time DNS validation rejecting any
 *     resolved address in a blocked range (closes the rebinding window);
 *     wired only when no proxy handles the target (proxy resolves DNS
 *     itself — mirrors WebFetchTool);
 *   - manual redirects, ≤3 hops, each hop fully re-validated;
 *   - 10 MB content cap + 30 s timeout + text-only content-type allowlist
 *     (ingestion targets documents, never binaries).
 */
import axios from 'axios'
import { getWebFetchUserAgent } from '../../utils/http.js'
import { getProxyUrl, shouldBypassProxy } from '../../utils/proxy.js'
import {
  assertWebFetchUrlAllowed,
  webFetchGuardedLookup,
} from '../../tools/WebFetchTool/utils.js'

const MAX_REDIRECT_HOPS = 3
const FETCH_TIMEOUT_MS = 30_000
const MAX_CONTENT_BYTES = 10 * 1024 * 1024

/** Text-document content types ingestion accepts (prefix match, pre-`;`). */
const ALLOWED_CONTENT_TYPE_PREFIXES = [
  'text/html',
  'application/xhtml+xml',
  'text/plain',
  'text/markdown',
]

export type IngestFetchResult = {
  /** URL after redirects (each hop re-validated). */
  finalUrl: string
  /** Normalized content type (pre-`;`, lowercase). */
  contentType: string
  body: string
}

function normalizedContentType(raw: unknown): string {
  return String(raw ?? '')
    .split(';')[0]!
    .trim()
    .toLowerCase()
}

/**
 * Whether a redirect hop must wire the connect-time lookup guard. Mirrors
 * WebFetchTool (utils.ts envProxyActive): guarded when NO proxy handles the
 * target — i.e. no proxy configured, or the target is NO_PROXY-bypassed (then
 * the connection is direct and the guard sees the real resolved address).
 *
 * W-WEBFETCH-FAKEIP-SSRF F-bycatch-1 (2026-07-24 审计 §6.4): the previous
 * call passed the BARE hostname to shouldBypassProxy, whose `new URL(...)`
 * then threw → constant false → the NO_PROXY re-guard branch was dead code.
 * shouldBypassProxy takes a full URL string. Exported for the pinning test.
 */
export function ingestHopUsesGuardedLookup(
  currentUrl: URL,
  proxyUrl: string | undefined,
  noProxy?: string,
): boolean {
  return !proxyUrl || shouldBypassProxy(currentUrl.toString(), noProxy)
}

/**
 * Fetch a URL for ingestion with the full SSRF guard set. Throws on any
 * policy violation, redirect overflow, disallowed content type, oversize
 * body, or transport failure — the ingest caller surfaces the message
 * verbatim (fail-closed, no partial drafts).
 */
export async function safeIngestFetch(url: string): Promise<IngestFetchResult> {
  let current = assertWebFetchUrlAllowed(url)

  for (let hop = 0; hop <= MAX_REDIRECT_HOPS; hop++) {
    const useGuardedLookup = ingestHopUsesGuardedLookup(current, getProxyUrl())

    const response = await axios.get<string>(current.toString(), {
      timeout: FETCH_TIMEOUT_MS,
      maxRedirects: 0,
      maxContentLength: MAX_CONTENT_BYTES,
      maxBodyLength: MAX_CONTENT_BYTES,
      responseType: 'text',
      // Redirect statuses resolve (we handle them manually); 4xx/5xx throw.
      validateStatus: status =>
        (status >= 200 && status < 300) || (status >= 300 && status < 400),
      headers: {
        'User-Agent': getWebFetchUserAgent(),
        Accept: 'text/html, text/markdown, text/plain;q=0.9, */*;q=0.1',
      },
      ...(useGuardedLookup ? { lookup: webFetchGuardedLookup } : {}),
    })

    if (response.status >= 300 && response.status < 400) {
      const location = response.headers['location']
      if (typeof location !== 'string' || location.length === 0) {
        throw new Error(`跳转响应缺少 Location（HTTP ${response.status}）`)
      }
      if (hop === MAX_REDIRECT_HOPS) {
        throw new Error(`跳转层数超限（>${MAX_REDIRECT_HOPS} 跳）`)
      }
      // Resolve relative redirects against the current URL, then re-run the
      // FULL pre-check set on the new target (per-hop re-validation).
      current = assertWebFetchUrlAllowed(new URL(location, current).toString())
      continue
    }

    const contentType = normalizedContentType(response.headers['content-type'])
    if (
      contentType.length > 0 &&
      !ALLOWED_CONTENT_TYPE_PREFIXES.some(p => contentType.startsWith(p))
    ) {
      throw new Error(
        `不支持的内容类型「${contentType}」— 知识库进料只接受文本文档（html/markdown/plain）`,
      )
    }

    const body = typeof response.data === 'string' ? response.data : ''
    if (body.length === 0) {
      throw new Error('抓取到的正文为空')
    }
    return { finalUrl: current.toString(), contentType, body }
  }

  throw new Error('unreachable: redirect loop guard')
}
