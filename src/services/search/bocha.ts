/**
 * Bocha Search API provider — TS port of Go services/gateway/search/bocha.go.
 *
 * Posts {query, count, language} to the configured endpoint (default
 * https://api.bochaai.com/v1/web-search) and parses web_results[] (primary)
 * or results[] (alternate field name). Each entry is field-fallback parsed:
 *   url ← url || link
 *   snippet ← description || snippet
 *   content ← content || body
 */

import { createCombinedAbortSignal } from '../../utils/combinedAbortSignal.js'
import { filterDomainAllowed } from './util.js'
import type { SearchOptions, SearchProvider, SearchResult } from './types.js'

const BOCHA_TIMEOUT_MS = 30_000
const MAX_ERR_RESPONSE_BYTES = 1 << 20 // 1 MiB
const DEFAULT_MAX_RESULTS = 8
const DEFAULT_LANGUAGE = 'zh-CN'

interface BochaWebResult {
  title?: string
  url?: string
  link?: string
  description?: string
  snippet?: string
  content?: string
  body?: string
}

interface BochaResponse {
  web_results?: BochaWebResult[]
  results?: BochaWebResult[]
}

export class BochaProvider implements SearchProvider {
  readonly name = 'bocha'

  constructor(
    private readonly apiKey: string,
    private readonly endpoint: string,
  ) {}

  async search(
    query: string,
    opts: SearchOptions,
    signal?: AbortSignal,
  ): Promise<SearchResult[]> {
    const maxResults = opts.maxResults && opts.maxResults > 0
      ? opts.maxResults
      : DEFAULT_MAX_RESULTS
    const language = opts.language || DEFAULT_LANGUAGE

    const reqBody = {
      query,
      count: maxResults,
      language,
    }

    const { signal: composedSignal, cleanup } = createCombinedAbortSignal(
      signal,
      { timeoutMs: BOCHA_TIMEOUT_MS },
    )

    try {
      const resp = await fetch(this.endpoint, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${this.apiKey}`,
        },
        body: JSON.stringify(reqBody),
        signal: composedSignal,
      })

      if (!resp.ok) {
        const errBody = await readLimited(resp, MAX_ERR_RESPONSE_BYTES)
        throw new Error(`bocha: API error ${resp.status}: ${errBody}`)
      }

      const data = (await resp.json()) as BochaResponse
      return parseBochaResults(data, opts)
    } finally {
      cleanup()
    }
  }
}

function parseBochaResults(
  data: BochaResponse,
  opts: SearchOptions,
): SearchResult[] {
  const maxResults = opts.maxResults && opts.maxResults > 0
    ? opts.maxResults
    : DEFAULT_MAX_RESULTS

  const webResults = data.web_results && data.web_results.length > 0
    ? data.web_results
    : data.results ?? []

  const results: SearchResult[] = []
  for (const r of webResults) {
    const rawURL = r.url || r.link || ''
    if (!rawURL) continue
    if (!filterDomainAllowed(rawURL, opts)) continue

    const snippet = r.description || r.snippet || ''
    const content = r.content || r.body || ''

    results.push({
      title: r.title ?? '',
      url: rawURL,
      snippet,
      content: content || undefined,
    })
  }

  return results.length > maxResults ? results.slice(0, maxResults) : results
}

async function readLimited(resp: Response, maxBytes: number): Promise<string> {
  if (!resp.body) return ''
  const reader = resp.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    while (total < maxBytes) {
      const { done, value } = await reader.read()
      if (done) break
      chunks.push(value)
      total += value.length
    }
  } catch {
    // ignore — return whatever we already accumulated
  }
  const merged = new Uint8Array(Math.min(total, maxBytes))
  let offset = 0
  for (const c of chunks) {
    const room = merged.length - offset
    if (room <= 0) break
    merged.set(c.subarray(0, room), offset)
    offset += Math.min(c.length, room)
  }
  return new TextDecoder('utf-8', { fatal: false }).decode(merged)
}
