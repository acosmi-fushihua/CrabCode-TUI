/**
 * Web search service types — TS port of Go services/gateway/search/search.go.
 *
 * After the Go cut (Y-path step 11/12), web search executes locally in the TS
 * client; this module defines the cross-provider Result / Options / Provider
 * contract that Ali (DashScope) and Bocha implementations share.
 */

export interface SearchResult {
  title: string
  url: string
  snippet: string
  content?: string
}

export interface SearchOptions {
  maxResults?: number
  allowedDomains?: string[]
  blockedDomains?: string[]
  language?: string
}

export interface SearchProvider {
  readonly name: string
  search(
    query: string,
    opts: SearchOptions,
    signal?: AbortSignal,
  ): Promise<SearchResult[]>
}

export interface SearchResponse {
  provider: string
  results: SearchResult[]
}
