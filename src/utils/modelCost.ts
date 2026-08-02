
import type { BetaUsage as Usage } from '../types/api-types.js'
import type { AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS } from 'src/services/analytics/index.js'
import { logEvent } from 'src/services/analytics/index.js'
import { setHasUnknownModelCost } from '../bootstrap/state.js'
import { getCachedModelPricePerMTok } from './model/modelCapabilities.js'

export type ModelCosts = {
  inputTokens: number
  outputTokens: number
  promptCacheWriteTokens: number
  promptCacheReadTokens: number
  webSearchRequests: number
}

const ZERO_COST: ModelCosts = {
  inputTokens: 0,
  outputTokens: 0,
  promptCacheWriteTokens: 0,
  promptCacheReadTokens: 0,
  webSearchRequests: 0,
}

/**
 * Derive a full ModelCosts from an SDK pricePerMTok ($ per million tokens).
 * Heuristic ratio: output = 5x input, cache_write = 1.25x input,
 * cache_read = 0.1x input.
 */
export function sdkPriceToModelCosts(pricePerMTok: number): ModelCosts {
  return {
    inputTokens: pricePerMTok,
    outputTokens: pricePerMTok * 5,
    promptCacheWriteTokens: pricePerMTok * 1.25,
    promptCacheReadTokens: pricePerMTok * 0.1,
    webSearchRequests: 0.01,
  }
}

function tokensToUSDCost(modelCosts: ModelCosts, usage: Usage): number {
  return (
    (usage.input_tokens / 1_000_000) * modelCosts.inputTokens +
    (usage.output_tokens / 1_000_000) * modelCosts.outputTokens +
    ((usage.cache_read_input_tokens ?? 0) / 1_000_000) *
      modelCosts.promptCacheReadTokens +
    ((usage.cache_creation_input_tokens ?? 0) / 1_000_000) *
      modelCosts.promptCacheWriteTokens +
    (usage.server_tool_use?.web_search_requests ?? 0) *
      modelCosts.webSearchRequests
  )
}

export function getModelCosts(model: string, usage: Usage): ModelCosts {
  const sdkPrice = getCachedModelPricePerMTok(model)
  if (sdkPrice !== undefined && sdkPrice > 0) {
    return sdkPriceToModelCosts(sdkPrice)
  }
  trackUnknownModelCost(model)
  return ZERO_COST
}

function trackUnknownModelCost(model: string): void {
  logEvent('tengu_unknown_model_cost', {
    model: model as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })
  setHasUnknownModelCost()
}

export function calculateUSDCost(resolvedModel: string, usage: Usage): number {
  return tokensToUSDCost(getModelCosts(resolvedModel, usage), usage)
}

export function calculateCostFromTokens(
  model: string,
  tokens: {
    inputTokens: number
    outputTokens: number
    cacheReadInputTokens: number
    cacheCreationInputTokens: number
  },
): number {
  const usage: Usage = {
    input_tokens: tokens.inputTokens,
    output_tokens: tokens.outputTokens,
    cache_read_input_tokens: tokens.cacheReadInputTokens,
    cache_creation_input_tokens: tokens.cacheCreationInputTokens,
  } as Usage
  return calculateUSDCost(model, usage)
}

function formatPrice(price: number): string {
  if (Number.isInteger(price)) {
    return `$${price}`
  }
  return `$${price.toFixed(2)}`
}

export function formatModelPricing(costs: ModelCosts): string {
  return `${formatPrice(costs.inputTokens)}/${formatPrice(costs.outputTokens)} per Mtok`
}

export function getModelPricingString(model: string): string | undefined {
  const sdkPrice = getCachedModelPricePerMTok(model)
  if (sdkPrice === undefined || sdkPrice <= 0) return undefined
  return formatModelPricing(sdkPriceToModelCosts(sdkPrice))
}
