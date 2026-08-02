/**
 * Alibaba DashScope (Qwen with web search enabled) provider.
 * TS port of Go services/gateway/search/ali.go.
 *
 * Uses the OpenAI-compatible /chat/completions endpoint with enable_search:true
 * and search_options.search_strategy='pro' / forced_search:true. Results are
 * parsed from message.search_results[] (primary) or message.annotations[]
 * (fallback to url_citation entries).
 */

import { createCombinedAbortSignal } from '../../utils/combinedAbortSignal.js'
import { filterDomainAllowed } from './util.js'
import type { SearchOptions, SearchProvider, SearchResult } from './types.js'

const ALI_TIMEOUT_MS = 30_000
const MAX_ERR_RESPONSE_BYTES = 1 << 20 // 1 MiB
const DEFAULT_MAX_RESULTS = 8

interface AliSearchResultItem {
  title?: string
  url?: string
  snippet?: string
  content?: string
}

interface AliAnnotation {
  type?: string
  title?: string
  url?: string
  text?: string
}

interface AliMessage {
  content?: string
  search_results?: AliSearchResultItem[]
  annotations?: AliAnnotation[]
}

interface AliResponse {
  choices?: Array<{ message?: AliMessage }>
}

export class AliProvider implements SearchProvider {
  readonly name = 'ali'

  constructor(
    private readonly apiKey: string,
    private readonly endpoint: string,
    private readonly model: string,
  ) {}

  async search(
    query: string,
    opts: SearchOptions,
    signal?: AbortSignal,
  ): Promise<SearchResult[]> {
    const prompt = buildAliPrompt(query, opts)

    const reqBody = {
      model: this.model,
      messages: [{ role: 'user', content: prompt }],
      enable_search: true,
      search_options: {
        search_strategy: 'pro',
        forced_search: true,
      },
    }

    const url = this.endpoint.replace(/\/+$/, '') + '/chat/completions'
    const { signal: composedSignal, cleanup } = createCombinedAbortSignal(
      signal,
      { timeoutMs: ALI_TIMEOUT_MS },
    )

    try {
      const resp = await fetch(url, {
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
        throw new Error(`ali: API error ${resp.status}: ${errBody}`)
      }

      const data = (await resp.json()) as AliResponse
      return parseAliResults(data, opts)
    } finally {
      cleanup()
    }
  }
}

function buildAliPrompt(query: string, opts: SearchOptions): string {
  let prompt = '搜索以下内容并返回相关结果：' + query
  if (opts.allowedDomains && opts.allowedDomains.length > 0) {
    prompt += '\n仅搜索以下域名：' + opts.allowedDomains.join(', ')
  }
  return prompt
}

function parseAliResults(data: AliResponse, opts: SearchOptions): SearchResult[] {
  const maxResults = opts.maxResults && opts.maxResults > 0
    ? opts.maxResults
    : DEFAULT_MAX_RESULTS

  if (!data.choices || data.choices.length === 0) return []
  const msg = data.choices[0]?.message
  if (!msg) return []

  const results: SearchResult[] = []

  // Primary: structured search_results field
  for (const r of msg.search_results ?? []) {
    if (!r.url) continue
    if (!filterDomainAllowed(r.url, opts)) continue
    const snippet = r.snippet || r.content || ''
    results.push({
      title: r.title ?? '',
      url: r.url,
      snippet,
      content: r.content,
    })
  }

  // Fallback: url_citation annotations
  if (results.length === 0) {
    for (const ann of msg.annotations ?? []) {
      if (ann.type !== 'url_citation') continue
      if (!ann.url) continue
      if (!filterDomainAllowed(ann.url, opts)) continue
      results.push({
        title: ann.title ?? '',
        url: ann.url,
        snippet: ann.text ?? '',
      })
    }
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
