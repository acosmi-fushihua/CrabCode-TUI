import type { Client } from '@modelcontextprotocol/sdk/client/index.js'
import type {
  Resource,
  ServerCapabilities,
} from '@modelcontextprotocol/sdk/types.js'
import { z } from 'zod/v4'
import { isReservedPluginMcpAuthoredServerName } from './pluginMcpIdentity.js'
import { lazySchema } from '../../utils/lazySchema.js'

// Configuration schemas and types
export const ConfigScopeSchema = lazySchema(() =>
  z.enum([
    'local',
    'user',
    'project',
    'dynamic',
    'enterprise',
    'acosmi',
    'managed',
  ]),
)
export type ConfigScope = z.infer<ReturnType<typeof ConfigScopeSchema>>

export const TransportSchema = lazySchema(() =>
  z.enum(['stdio', 'sse', 'sse-ide', 'http', 'ws', 'sdk']),
)
export type Transport = z.infer<ReturnType<typeof TransportSchema>>

// Shared URL validators for remote MCP transports. Each remote transport's
// `url` must be an absolute URL with a scheme appropriate to its family:
//   - HTTP-family (sse / sse-ide / http / acosmi-proxy) → http(s)
//   - WS-family   (ws / ws-ide)                          → ws(s)
// `.url()` already rejects scheme-less / relative URLs; the `.refine` then
// pins the allowed scheme set so e.g. a `file://` or `javascript:` URL can't
// slip into a remote transport config.
function httpUrl() {
  return z
    .string()
    .url()
    .refine(
      (u) => {
        try {
          const scheme = new URL(u).protocol
          return scheme === 'http:' || scheme === 'https:'
        } catch {
          return false
        }
      },
      { message: 'url must use http:// or https://' },
    )
}

function wsUrl() {
  return z
    .string()
    .url()
    .refine(
      (u) => {
        try {
          const scheme = new URL(u).protocol
          return scheme === 'ws:' || scheme === 'wss:'
        } catch {
          return false
        }
      },
      { message: 'url must use ws:// or wss://' },
    )
}

export const McpStdioServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('stdio').optional(), // Optional for backwards compatibility
    command: z.string().min(1, 'Command cannot be empty'),
    args: z.array(z.string()).default([]),
    env: z.record(z.string(), z.string()).optional(),
  }),
)

// Cross-App Access (XAA / SEP-990): just a per-server flag. IdP connection
// details (issuer, clientId, callbackPort) come from settings.xaaIdp — configured
// once, shared across all XAA-enabled servers. clientId/clientSecret (parent
// oauth config + keychain slot) are for the MCP server's AS.
const McpXaaConfigSchema = lazySchema(() => z.boolean())

const McpOAuthConfigSchema = lazySchema(() =>
  z.object({
    clientId: z.string().optional(),
    callbackPort: z.number().int().positive().optional(),
    authServerMetadataUrl: z
      .string()
      .url()
      .startsWith('https://', {
        message: 'authServerMetadataUrl must use https://',
      })
      .optional(),
    xaa: McpXaaConfigSchema().optional(),
  }),
)

export const McpSSEServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('sse'),
    url: httpUrl(),
    headers: z.record(z.string(), z.string()).optional(),
    headersHelper: z.string().optional(),
    oauth: McpOAuthConfigSchema().optional(),
  }),
)

// Internal-only server type for IDE extensions
export const McpSSEIDEServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('sse-ide'),
    url: httpUrl(),
    ideName: z.string(),
    ideRunningInWindows: z.boolean().optional(),
  }),
)

// Internal-only server type for IDE extensions
export const McpWebSocketIDEServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('ws-ide'),
    url: wsUrl(),
    ideName: z.string(),
    authToken: z.string().optional(),
    ideRunningInWindows: z.boolean().optional(),
  }),
)

export const McpHTTPServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('http'),
    url: httpUrl(),
    headers: z.record(z.string(), z.string()).optional(),
    headersHelper: z.string().optional(),
    oauth: McpOAuthConfigSchema().optional(),
  }),
)

export const McpWebSocketServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('ws'),
    url: wsUrl(),
    headers: z.record(z.string(), z.string()).optional(),
    headersHelper: z.string().optional(),
  }),
)

export const McpSdkServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('sdk'),
    name: z.string(),
  }),
)

// Config type for Acosmi proxy servers
export const McpAcosmiProxyServerConfigSchema = lazySchema(() =>
  z.object({
    type: z.literal('acosmi-proxy'),
    url: httpUrl(),
    id: z.string(),
  }),
)

export const McpServerConfigSchema = lazySchema(() =>
  z.union([
    McpStdioServerConfigSchema(),
    McpSSEServerConfigSchema(),
    McpSSEIDEServerConfigSchema(),
    McpWebSocketIDEServerConfigSchema(),
    McpHTTPServerConfigSchema(),
    McpWebSocketServerConfigSchema(),
    McpSdkServerConfigSchema(),
    McpAcosmiProxyServerConfigSchema(),
  ]),
)

export type McpStdioServerConfig = z.infer<
  ReturnType<typeof McpStdioServerConfigSchema>
>
export type McpSSEServerConfig = z.infer<
  ReturnType<typeof McpSSEServerConfigSchema>
>
export type McpSSEIDEServerConfig = z.infer<
  ReturnType<typeof McpSSEIDEServerConfigSchema>
>
export type McpWebSocketIDEServerConfig = z.infer<
  ReturnType<typeof McpWebSocketIDEServerConfigSchema>
>
export type McpHTTPServerConfig = z.infer<
  ReturnType<typeof McpHTTPServerConfigSchema>
>
export type McpWebSocketServerConfig = z.infer<
  ReturnType<typeof McpWebSocketServerConfigSchema>
>
export type McpSdkServerConfig = z.infer<
  ReturnType<typeof McpSdkServerConfigSchema>
>
export type McpAcosmiProxyServerConfig = z.infer<
  ReturnType<typeof McpAcosmiProxyServerConfigSchema>
>
export type McpServerConfig = z.infer<ReturnType<typeof McpServerConfigSchema>>

/**
 * Internal lifecycle facts attached only to plugin-provided MCP configs.
 *
 * This object deliberately stays out of `McpServerConfigSchema`: it is a
 * runtime projection, not user-authored MCP JSON and not a remote wire
 * field. Keeping the facts beside the resolved config lets every consumer use
 * one activation/preflight truth without parsing the display-oriented runtime
 * name back into a plugin storage identity.
 */
export type PluginMcpLifecycleMetadata = {
  pluginId: string
  pluginName: string
  serverName: string
  runtimeName: string
  /** Source-bound persistence key; defaults to serverName for local servers. */
  activationKey?: string
  sourceKind: 'builtin' | 'inline' | 'marketplace'
  pluginEnabled: boolean
  activation: 'inactive' | 'enabled' | 'required'
  configState: 'ready' | 'requiresConfig' | 'invalid'
  authState: 'notRequired' | 'unknown' | 'requiresLogin' | 'ready'
  dependencyState: 'ready' | 'requiresDependency' | 'unknown'
  active: boolean
  reasonCode:
    | 'ready'
    | 'plugin-disabled'
    | 'inactive'
    | 'requires-config'
    | 'invalid-config'
    | 'requires-login'
    | 'requires-dependency'
    | 'required-not-local'
  /** Canonical plugin root used as stdio cwd after S2 containment. */
  pluginRoot: string
  /** Plugin version/path generation; participates in the connection cache key. */
  generation: string
  /** Fixed, secret-free diagnostic suitable for logs/status details. */
  reason?: string
}

export type ScopedMcpServerConfig = McpServerConfig & {
  scope: ConfigScope
  // For plugin-provided servers: the providing plugin's LoadedPlugin.source
  // (e.g. 'slack@acosmi'). Stashed at config-build time so the channel
  // gate doesn't have to race AppState.plugins.enabled hydration.
  pluginSource?: string
  pluginMcp?: PluginMcpLifecycleMetadata
  /** UI-only carrier for a lifecycle row with no executable config yet. */
  pluginMcpManagementOnly?: true
}

/** User/plugin-authored server keys; `plugin:` is reserved for runtime IDs. */
export const McpServerNameSchema = lazySchema(() =>
  z
    .string()
    .min(1, 'MCP server name must not be empty')
    .max(1024, 'MCP server name must not exceed 1024 characters')
    .refine(name => encodeURIComponent(name).length <= 1024, {
      message: 'MCP server name is too large after canonical encoding',
    })
    .refine(name => !isReservedPluginMcpAuthoredServerName(name), {
      message: 'MCP server name uses a reserved CrabCode runtime namespace',
    }),
)

export const McpJsonConfigSchema = lazySchema(() =>
  z.object({
    mcpServers: z.record(McpServerNameSchema(), McpServerConfigSchema()),
  }),
)

export type McpJsonConfig = z.infer<ReturnType<typeof McpJsonConfigSchema>>

// Metadata carried by a *worker-owned* `connected` server (W-MCP runtime
// ownership consolidation, debt #3). When the bun worker owns a non-IDE MCP
// server, the main process has no live `client` handle for it — the actual
// tool invocation happens inside the worker turn. We still want the TUI to
// render it as `connected` (so `buildMcpProperties` / `MCPSettings` count it
// and surface its tools), so we represent it as a `ConnectedMCPServer` with
// `client: undefined` and this `workerMeta` carrying the worker's last
// snapshot of tool/resource/command counts. Present ONLY on worker-owned
// entries; absent on every main-proc-authored live connection.
export type WorkerOwnedConnectionMeta = {
  tools: ReadonlyArray<{ name: string; description?: string | null }>
  resources: ReadonlyArray<{ uri: string; name?: string | null }>
  commands: ReadonlyArray<{ name: string; description?: string | null }>
}

// Server connection types
export type ConnectedMCPServer = {
  // Live SDK handle for main-proc-owned connections. `undefined` for
  // worker-owned servers projected via `mcpServer/state/query` — those have
  // no in-process handle and carry `workerMeta` instead. Every direct-path
  // consumer that dereferences `client` MUST guard for `undefined` and
  // gracefully skip/degrade (worker owns invocation; main-proc has no handle).
  client?: Client
  name: string
  type: 'connected'
  capabilities: ServerCapabilities
  serverInfo?: {
    name: string
    version: string
  }
  instructions?: string
  config: ScopedMcpServerConfig
  cleanup: () => Promise<void>
  // Present only when this entry is a handle-free, worker-owned projection.
  workerMeta?: WorkerOwnedConnectionMeta
  /**
   * Host-minted locality capability. This is runtime state, never parsed from
   * MCP config or server metadata, so an external server cannot grant itself
   * access to host filesystem paths by reusing a reserved name.
   */
  runtimePathAuthority?: 'installed-html-video'
}

export type FailedMCPServer = {
  name: string
  type: 'failed'
  config: ScopedMcpServerConfig
  error?: string
}

export type NeedsAuthMCPServer = {
  name: string
  type: 'needs-auth'
  config: ScopedMcpServerConfig
}

export type PendingMCPServer = {
  name: string
  type: 'pending'
  config: ScopedMcpServerConfig
  reconnectAttempt?: number
  maxReconnectAttempts?: number
}

export type DisabledMCPServer = {
  name: string
  type: 'disabled'
  config: ScopedMcpServerConfig
}

export type MCPServerConnection =
  | ConnectedMCPServer
  | FailedMCPServer
  | NeedsAuthMCPServer
  | PendingMCPServer
  | DisabledMCPServer

// Resource types
export type ServerResource = Resource & { server: string }

// MCP CLI State types
export interface SerializedTool {
  name: string
  description: string
  inputJSONSchema?: {
    [x: string]: unknown
    type: 'object'
    properties?: {
      [x: string]: unknown
    }
  }
  isMcp?: boolean
  originalToolName?: string // Original unnormalized tool name from MCP server
}

export interface SerializedClient {
  name: string
  type: 'connected' | 'failed' | 'needs-auth' | 'pending' | 'disabled'
  capabilities?: ServerCapabilities
}

export interface MCPCliState {
  clients: SerializedClient[]
  configs: Record<string, ScopedMcpServerConfig>
  tools: SerializedTool[]
  resources: Record<string, ServerResource[]>
  normalizedNames?: Record<string, string> // Maps normalized names to original names
}
