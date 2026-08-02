/**
 * MCP Client — barrel re-export file.
 *
 * The implementation has been split into focused sub-modules:
 *   - errors.ts            — Error classes & detection helpers
 *   - authCache.ts         — Auth cache, proxy fetch, timeout wrapper, analytics helpers
 *   - resultProcessing.ts  — Transform & process MCP tool/prompt results
 *   - toolCall.ts          — callMCPTool, callMCPToolWithUrlElicitationRetry, callIdeRpc
 *   - connectionAndFetch.ts — connectToServer, clearServerCache, ensureConnectedClient, fetch*ForClient
 *   - orchestration.ts     — getMcpToolsCommandsAndResources, prefetchAllMcpResources, reconnect, SDK setup
 *
 * All original exports are re-exported here so existing import paths continue to work.
 */

// --- errors.ts ---
export {
  McpAuthError,
  McpToolCallError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  isMcpSessionExpiredError,
} from './errors.js'

// --- authCache.ts ---
export {
  clearMcpAuthCache,
  createAcosmiProxyFetch,
  wrapFetchWithTimeout,
  getMcpServerConnectionBatchSize,
} from './authCache.js'

// --- resultProcessing.ts ---
export {
  transformResultContent,
  type MCPResultType,
  type ProcessedMCPResultWithArtifacts,
  type TransformedMCPResult,
  type TransformedMCPResultWithArtifacts,
  inferCompactSchema,
  transformMCPResult,
  transformMCPResultWithArtifacts,
  processMCPResult,
  processMCPResultWithArtifacts,
} from './resultProcessing.js'

// --- toolCall.ts ---
export {
  callIdeRpc,
  callMCPToolWithUrlElicitationRetry,
} from './toolCall.js'

// --- connectionAndFetch.ts ---
export {
  getServerCacheKey,
  connectToServer,
  clearServerCache,
  evictExistingServerCache,
  ensureConnectedClient,
  areMcpConfigsEqual,
  mcpToolInputToAutoClassifierInput,
  fetchToolsForClient,
  fetchResourcesForClient,
  fetchCommandsForClient,
} from './connectionAndFetch.js'

// --- orchestration.ts ---
export {
  getMcpToolsCommandsAndResources,
  prefetchAllMcpResources,
  reconnectMcpServerImpl,
  setupSdkMcpClients,
} from './orchestration.js'
