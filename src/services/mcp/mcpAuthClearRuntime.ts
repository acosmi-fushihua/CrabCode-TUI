import type {
  McpHTTPServerConfig,
  McpSSEServerConfig,
  ScopedMcpServerConfig,
} from './types.js'
import { getActiveMcpConfigByName } from './config.js'
import { revokeServerTokens } from './auth.js'
import {
  evictExistingServerCache,
  getServerCacheKey,
} from './connectionAndFetch.js'

interface McpAuthClearRuntimeDeps {
  revokeServerTokens: typeof revokeServerTokens
  getActiveMcpConfigByName: typeof getActiveMcpConfigByName
  evictExistingServerCache: typeof evictExistingServerCache
}

const defaultDeps: McpAuthClearRuntimeDeps = {
  revokeServerTokens,
  getActiveMcpConfigByName,
  evictExistingServerCache,
}

let depsOverride: McpAuthClearRuntimeDeps | null = null

export type OAuthScopedMcpServerConfig = ScopedMcpServerConfig &
  (McpHTTPServerConfig | McpSSEServerConfig)

export function __setMcpAuthClearRuntimeDepsForTest(
  deps: McpAuthClearRuntimeDeps | null,
): void {
  depsOverride = deps
}

/**
 * Revoke one server's credentials without ever connecting or reconnecting it.
 *
 * The caller may hold a stale client snapshot while another window disables or
 * uninstalls a plugin. Revocation is allowed against that captured endpoint,
 * but post-revoke runtime state is decided by a fresh active-config lookup:
 * an inactive plugin returns null and must disappear; an active/manual server
 * remains as a handle-free needs-auth row. Cache eviction is exact and
 * connect-free for both captured and fresh generations.
 */
export async function clearMcpAuthenticationRuntime(
  serverName: string,
  capturedConfig: OAuthScopedMcpServerConfig,
  options: {
    retainUnpersistedDynamic?: boolean
    /** Optional owner-scoped resolver for runtimes with multiple desired sets. */
    resolveFreshConfig?: () => Promise<ScopedMcpServerConfig | null>
  } = {},
): Promise<OAuthScopedMcpServerConfig | null> {
  const deps = depsOverride ?? defaultDeps
  await deps.revokeServerTokens(serverName, capturedConfig)
  await deps.evictExistingServerCache(serverName, capturedConfig)

  const resolvedFreshConfig = options.resolveFreshConfig
    ? await options.resolveFreshConfig()
    : await deps.getActiveMcpConfigByName(serverName)
  const freshConfig =
    resolvedFreshConfig?.type === 'http' || resolvedFreshConfig?.type === 'sse'
      ? resolvedFreshConfig
      : null
  if (
    freshConfig &&
    getServerCacheKey(serverName, freshConfig) !==
      getServerCacheKey(serverName, capturedConfig)
  ) {
    await deps.evictExistingServerCache(serverName, freshConfig)
  }

  // A stale plugin or persisted manual snapshot is not authority to resurrect
  // an inactive/disabled/policy-filtered server. The only resolver-invisible
  // exception is an explicitly identified control-plane dynamic server owned
  // by this exact headless runtime; callers must opt in to retaining it.
  return (
    freshConfig ??
    (options.retainUnpersistedDynamic &&
    capturedConfig.scope === 'dynamic' &&
    !capturedConfig.pluginMcp
      ? capturedConfig
      : null)
  )
}
