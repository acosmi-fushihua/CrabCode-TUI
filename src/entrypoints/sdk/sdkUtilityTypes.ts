// SDK Utility Types - Types that cannot be expressed as Zod schemas.

/**
 * A version of BetaUsage where all nullable fields are required and non-null.
 * Used internally for accumulating usage across API calls.
 */
export type NonNullableUsage = {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number
  cache_read_input_tokens: number
  server_tool_use: {
    web_search_requests: number
    web_fetch_requests: number
  }
  service_tier: string | null
  cache_creation: {
    ephemeral_1h_input_tokens: number
    ephemeral_5m_input_tokens: number
  }
  inference_geo?: string | null
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  iterations?: any[] | null
  speed?: string | null
}

export {}
