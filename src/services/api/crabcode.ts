// Barrel re-export file — all logic has been split into sub-modules.
// Consumers continue to import from 'services/api/crabcode.js' unchanged.

// Types
export type {
  JsonValue,
  JsonObject,
  JsonArray,
  TaskBudgetParam,
  Options,
  FastModeOptions,
  QueryWithModelOptions,
  CachedMCEditsBlock,
  CachedMCPinnedEdits,
} from './types.js'

// Params & config
export {
  getExtraBodyParams,
  getAPIMetadata,
  configureTaskBudgetParams,
  getMaxOutputTokensForModel,
  MAX_NON_STREAMING_TOKENS,
  adjustParamsForNonStreaming,
  getPromptCachingEnabled,
} from './params.js'

// Cache utilities
export {
  getCacheControl,
  userMessageToMessageParam,
  assistantMessageToMessageParam,
  addCacheBreakpoints,
  buildSystemPromptBlocks,
} from './cacheUtils.js'

// Usage tracking
export {
  updateUsage,
  accumulateUsage,
} from './usageUtils.js'

// Message processing
export {
  stripExcessMediaItems,
} from './messageProcessing.js'

// Query functions
export {
  verifyApiKey,
  queryModelWithoutStreaming,
  queryModelWithStreaming,
  executeNonStreamingRequest,
  cleanupStream,
  queryFastMode,
  queryWithModel,
} from './queryModel.js'
