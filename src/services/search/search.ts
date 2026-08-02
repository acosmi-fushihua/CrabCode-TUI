/**
 * Web search service entry point — TS port of the Go gateway search Service
 * (services/gateway/search/search.go).
 *
 * After the Go cut, search executes in-process; provider selection and
 * configuration come from the same CRABCODE_SEARCH_* env vars that
 * managedEnv.bridgeStructuredSettingsToEnv bridges from
 * ~/.crabcode/settings.json. No bridge / IPC / hub involvement.
 */

import { AliProvider } from './ali.js'
import { BochaProvider } from './bocha.js'
import type {
  SearchOptions,
  SearchProvider,
  SearchResponse,
} from './types.js'

const ALI_DEFAULT_ENDPOINT = 'https://dashscope.aliyuncs.com/compatible-mode/v1'
const ALI_DEFAULT_MODEL = 'qwen-plus-search'
const BOCHA_DEFAULT_ENDPOINT = 'https://api.bochaai.com/v1/web-search'

export function createProvider(): SearchProvider | null {
  const kind = process.env.CRABCODE_SEARCH_PROVIDER
  switch (kind) {
    case 'ali': {
      const apiKey = process.env.CRABCODE_SEARCH_ALI_API_KEY
      if (!apiKey) return null
      const endpoint = process.env.CRABCODE_SEARCH_ALI_ENDPOINT || ALI_DEFAULT_ENDPOINT
      const model = process.env.CRABCODE_SEARCH_ALI_MODEL || ALI_DEFAULT_MODEL
      return new AliProvider(apiKey, endpoint, model)
    }
    case 'bocha': {
      const apiKey = process.env.CRABCODE_SEARCH_BOCHA_API_KEY
      if (!apiKey) return null
      const endpoint =
        process.env.CRABCODE_SEARCH_BOCHA_ENDPOINT || BOCHA_DEFAULT_ENDPOINT
      return new BochaProvider(apiKey, endpoint)
    }
    default:
      return null
  }
}

/**
 * Returns true when a search provider is configured (env vars present).
 * WebSearchTool.isEnabled uses this as the gating predicate, replacing the
 * legacy `CRABCODE_SEARCH_PROVIDER && isGoBridgeAvailable()` check that
 * required the Go hub.
 */
export function isSearchProviderConfigured(): boolean {
  return createProvider() !== null
}

/**
 * Execute a search query. Returns null when no provider is configured (the
 * caller treats this as the "search provider not configured" error path);
 * throws on transport / API errors so they propagate to the tool framework.
 */
export async function executeSearch(
  query: string,
  opts: SearchOptions,
  signal?: AbortSignal,
): Promise<SearchResponse | null> {
  const provider = createProvider()
  if (!provider) return null
  const results = await provider.search(query, opts, signal)
  return { provider: provider.name, results }
}
