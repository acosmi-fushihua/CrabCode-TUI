import type { ContentBlockParam } from '../../types/api-types.js'
import { feature } from '../../utils/featurePolyfill.js'

import { Client } from '@modelcontextprotocol/sdk/client/index.js'
import {
  SSEClientTransport,
  type SSEClientTransportOptions,
} from '@modelcontextprotocol/sdk/client/sse.js'
import {
  StreamableHTTPClientTransport,
  type StreamableHTTPClientTransportOptions,
} from '@modelcontextprotocol/sdk/client/streamableHttp.js'
import {
  createFetchWithInit,
  type FetchLike,
  type Transport,
} from '@modelcontextprotocol/sdk/shared/transport.js'
import {
  ElicitRequestSchema,
  type ListPromptsResult,
  ListPromptsResultSchema,
  ListResourcesResultSchema,
  ListRootsRequestSchema,
  type ListToolsResult,
  ListToolsResultSchema,
} from '@modelcontextprotocol/sdk/types.js'
import mapValues from 'lodash-es/mapValues.js'
import memoize from 'lodash-es/memoize.js'
import zipObject from 'lodash-es/zipObject.js'
import { pathToFileURL } from 'url'
import { getOriginalCwd, getSessionId } from '../../bootstrap/state.js'
import type { Command } from '../../commands.js'
import { getOauthConfig } from '../../constants/oauth.js'
import { PRODUCT_URL } from '../../constants/product.js'
import {
  type Tool,
  type ToolCallProgress,
  toolMatchesName,
} from '../../Tool.js'
import { ListMcpResourcesTool } from '../../tools/ListMcpResourcesTool/ListMcpResourcesTool.js'
import { type MCPProgress, MCPTool } from '../../tools/MCPTool/MCPTool.js'
import { ReadMcpResourceTool } from '../../tools/ReadMcpResourceTool/ReadMcpResourceTool.js'
import { errorMessage, TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS } from '../../utils/errors.js'
import { getMCPUserAgent } from '../../utils/http.js'
import { maybeNotifyIDEConnected } from '../../utils/ide.js'
import { logMCPDebug, logMCPError } from '../../utils/log.js'
import { logForDebugging } from '../../utils/debug.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import { getWebSocketTLSOptions } from '../../utils/mtls.js'
import {
  getProxyFetchOptions,
  getWebSocketProxyAgent,
  getWebSocketProxyUrl,
} from '../../utils/proxy.js'
import { recursivelySanitizeUnicode } from '../../utils/sanitization.js'
import { getSessionIngressAuthToken } from '../../utils/sessionIngressAuth.js'
import { subprocessEnv } from '../../utils/subprocessEnv.js'
import { registerCleanup } from '../../utils/cleanupRegistry.js'
import { WebSocketTransport } from '../../utils/mcpWebSocketTransport.js'
import { memoizeWithLRU } from '../../utils/memoize.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import {
  CrabCodeAuthProvider,
  wrapFetchWithStepUpDetection,
} from './auth.js'
import { buildMcpToolName } from './mcpStringUtils.js'
import { getMcpServerHeaders } from './headersHelper.js'
import type {
  ConnectedMCPServer,
  MCPServerConnection,
  ScopedMcpServerConfig,
  ServerResource,
} from './types.js'
import {
  isMcpSessionExpiredError,
  McpSessionExpiredError,
} from './errors.js'
import {
  wrapFetchWithTimeout,
  createAcosmiProxyFetch,
  handleRemoteAuthFailure,
  mcpBaseUrlAnalytics,
} from './authCache.js'
import {
  callMCPToolWithUrlElicitationRetry,
  callMCPTool,
} from './toolCall.js'
import { transformResultContent } from './resultProcessing.js'
import { jsonParse, jsonStringify } from '../../utils/slowOperations.js'
import type { AssistantMessage } from 'src/types/message.js'
import { classifyMcpToolForCollapse } from '../../tools/MCPTool/classifyForCollapse.js'
import { UnauthorizedError } from '@modelcontextprotocol/sdk/client/auth.js'
import { getAcosmiOAuthTokens } from '../../utils/auth.js'
import { ExactStdioClientTransport } from './ExactStdioClientTransport.js'

/**
 * Cap on MCP tool descriptions and server instructions sent to the model.
 * OpenAPI-generated MCP servers have been observed dumping 15-60KB of endpoint
 * docs into tool.description; this caps the p95 tail without losing the intent.
 */
const MAX_MCP_DESCRIPTION_LENGTH = 2048

function installedHtmlVideoRuntimeAuthority(
  name: string,
  config: ScopedMcpServerConfig,
): boolean {
  const lifecycle = config.pluginMcp
  return (
    (config.type === undefined || config.type === 'stdio') &&
    lifecycle?.runtimeName === name &&
    lifecycle.pluginName === 'crabcode-html-video' &&
    lifecycle.serverName === 'html-video' &&
    lifecycle.pluginEnabled === true &&
    lifecycle.active === true &&
    lifecycle.reasonCode === 'ready' &&
    lifecycle.configState === 'ready' &&
    lifecycle.dependencyState === 'ready'
  )
}

// Minimal interface for WebSocket instances passed to mcpWebSocketTransport
type WsClientLike = {
  readonly readyState: number
  close(): void
  send(data: string): void
}

/**
 * Create a ws.WebSocket client with the MCP protocol.
 * Bun's ws shim types lack the 3-arg constructor (url, protocols, options)
 * that the real ws package supports, so we cast the constructor here.
 */
async function createNodeWsClient(
  url: string,
  options: Record<string, unknown>,
): Promise<WsClientLike> {
  const wsModule = await import('ws')
  const WS = wsModule.default as unknown as new (
    url: string,
    protocols: string[],
    options: Record<string, unknown>,
  ) => WsClientLike
  return new WS(url, ['mcp'], options)
}

function getConnectionTimeoutMs(): number {
  return parseInt(process.env.MCP_TIMEOUT || '', 10) || 30000
}

/**
 * Default timeout for individual MCP requests (auth, tool calls, etc.)
 */
const MCP_REQUEST_TIMEOUT_MS = 60000

// For the IDE MCP servers, we only include specific tools
const ALLOWED_IDE_TOOLS = ['mcp__ide__executeCode', 'mcp__ide__getDiagnostics']
function isIncludedMcpTool(tool: Tool): boolean {
  return (
    !tool.name.startsWith('mcp__ide__') || ALLOWED_IDE_TOOLS.includes(tool.name)
  )
}

/**
 * Generates the cache key for a server connection.
 *
 * The tuple is deliberately self-delimiting. Server names are only bounded by
 * `McpServerNameSchema`; valid names may contain any textual delimiter (for
 * example `-{`). The former `${name}-${json}` shape therefore could not be
 * parsed without truncating such names during stale-cache reconciliation.
 *
 * @param name Server name
 * @param serverRef Server configuration
 * @returns Cache key string
 */
export function getServerCacheKey(
  name: string,
  serverRef: ScopedMcpServerConfig,
): string {
  return jsonStringify([name, serverRef])
}

export type ParsedMcpServerCacheKey = {
  name: string
  serverRef: ScopedMcpServerConfig
}

/**
 * Parse the exact tuple emitted by {@link getServerCacheKey}. All cache
 * observers use this one parser; malformed/legacy keys fail closed instead of
 * guessing a server name from an ambiguous delimiter.
 */
export function parseServerCacheKey(
  key: string,
): ParsedMcpServerCacheKey | null {
  try {
    const parsed = jsonParse(key) as unknown
    if (!Array.isArray(parsed) || parsed.length !== 2) return null
    const [name, serverRef] = parsed
    if (
      typeof name !== 'string' ||
      name.length === 0 ||
      typeof serverRef !== 'object' ||
      serverRef === null ||
      Array.isArray(serverRef)
    ) {
      return null
    }
    return {
      name,
      serverRef: serverRef as ScopedMcpServerConfig,
    }
  } catch {
    return null
  }
}

/**
 * Attempts to connect to a single MCP server
 * @param name Server name
 * @param serverRef Scoped server configuration
 * @returns A wrapped client (either connected or failed)
 */
export const connectToServer = memoize(
  async (
    name: string,
    serverRef: ScopedMcpServerConfig,
    serverStats?: {
      totalServers: number
      stdioCount: number
      sseCount: number
      httpCount: number
      sseIdeCount: number
      wsIdeCount: number
    },
  ): Promise<MCPServerConnection> => {
    const connectStartTime = Date.now()
    let transport: Transport | undefined
    try {
      // If we have the session ingress JWT, we will connect via the session ingress rather than
      // to remote MCP's directly.
      const sessionIngressToken = getSessionIngressAuthToken()

      if (serverRef.type === 'sse') {
        // Create an auth provider for this server
        const authProvider = new CrabCodeAuthProvider(name, serverRef)

        // Get combined headers (static + dynamic)
        const combinedHeaders = await getMcpServerHeaders(name, serverRef)

        // Use the auth provider with SSEClientTransport
        const transportOptions: SSEClientTransportOptions = {
          authProvider,
          // Use fresh timeout per request to avoid stale AbortSignal bug.
          // Step-up detection wraps innermost so the 403 is seen before the
          // SDK's handler calls auth() → tokens().
          fetch: wrapFetchWithTimeout(
            wrapFetchWithStepUpDetection(createFetchWithInit(), authProvider),
          ),
          requestInit: {
            headers: {
              'User-Agent': getMCPUserAgent(),
              ...combinedHeaders,
            },
          },
        }

        // IMPORTANT: Always set eventSourceInit with a fetch that does NOT use the
        // timeout wrapper. The EventSource connection is long-lived (stays open indefinitely
        // to receive server-sent events), so applying a 60-second timeout would kill it.
        // The timeout is only meant for individual API requests (POST, auth refresh), not
        // the persistent SSE stream.
        transportOptions.eventSourceInit = {
          fetch: async (url: string | URL, init?: RequestInit) => {
            // Get auth headers from the auth provider
            const authHeaders: Record<string, string> = {}
            const tokens = await authProvider.tokens()
            if (tokens) {
              authHeaders.Authorization = `Bearer ${tokens.access_token}`
            }

            const proxyOptions = getProxyFetchOptions()
            // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
            return fetch(url, {
              ...init,
              ...proxyOptions,
              headers: {
                'User-Agent': getMCPUserAgent(),
                ...authHeaders,
                ...init?.headers,
                ...combinedHeaders,
                Accept: 'text/event-stream',
              },
            })
          },
        }

        transport = new SSEClientTransport(
          new URL(serverRef.url),
          transportOptions,
        )
        logMCPDebug(name, `SSE transport initialized, awaiting connection`)
      } else if (serverRef.type === 'sse-ide') {
        logMCPDebug(name, `Setting up SSE-IDE transport to ${serverRef.url}`)
        // IDE servers don't need authentication
        // IDE servers use lockfile-based auth when available.
        const proxyOptions = getProxyFetchOptions()
        const transportOptions: SSEClientTransportOptions =
          proxyOptions.dispatcher
            ? {
                eventSourceInit: {
                  fetch: async (url: string | URL, init?: RequestInit) => {
                    // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
                    return fetch(url, {
                      ...init,
                      ...proxyOptions,
                      headers: {
                        'User-Agent': getMCPUserAgent(),
                        ...init?.headers,
                      },
                    })
                  },
                },
              }
            : {}

        transport = new SSEClientTransport(
          new URL(serverRef.url),
          Object.keys(transportOptions).length > 0
            ? transportOptions
            : undefined,
        )
      } else if (serverRef.type === 'ws-ide') {
        const tlsOptions = getWebSocketTLSOptions()
        const wsHeaders = {
          'User-Agent': getMCPUserAgent(),
          ...(serverRef.authToken && {
            'X-CrabCode-Ide-Authorization': serverRef.authToken,
          }),
        }

        let wsClient: WsClientLike
        if (typeof Bun !== 'undefined') {
          // Bun's WebSocket supports headers/proxy/tls options but the DOM typings don't
          // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
          wsClient = new globalThis.WebSocket(serverRef.url, {
            protocols: ['mcp'],
            headers: wsHeaders,
            proxy: getWebSocketProxyUrl(serverRef.url),
            tls: tlsOptions || undefined,
          } as unknown as string[])
        } else {
          wsClient = await createNodeWsClient(serverRef.url, {
            headers: wsHeaders,
            agent: getWebSocketProxyAgent(serverRef.url),
            ...(tlsOptions || {}),
          })
        }
        transport = new WebSocketTransport(wsClient)
      } else if (serverRef.type === 'ws') {
        logMCPDebug(
          name,
          `Initializing WebSocket transport to ${serverRef.url}`,
        )

        const combinedHeaders = await getMcpServerHeaders(name, serverRef)

        const tlsOptions = getWebSocketTLSOptions()
        const wsHeaders = {
          'User-Agent': getMCPUserAgent(),
          ...(sessionIngressToken && {
            Authorization: `Bearer ${sessionIngressToken}`,
          }),
          ...combinedHeaders,
        }

        // Redact sensitive headers before logging
        const wsHeadersForLogging = mapValues(wsHeaders, (value, key) =>
          key.toLowerCase() === 'authorization' ? '[REDACTED]' : value,
        )

        logMCPDebug(
          name,
          `WebSocket transport options: ${jsonStringify({
            url: serverRef.url,
            headers: wsHeadersForLogging,
            hasSessionAuth: !!sessionIngressToken,
          })}`,
        )

        let wsClient: WsClientLike
        if (typeof Bun !== 'undefined') {
          // Bun's WebSocket supports headers/proxy/tls options but the DOM typings don't
          // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
          wsClient = new globalThis.WebSocket(serverRef.url, {
            protocols: ['mcp'],
            headers: wsHeaders,
            proxy: getWebSocketProxyUrl(serverRef.url),
            tls: tlsOptions || undefined,
          } as unknown as string[])
        } else {
          wsClient = await createNodeWsClient(serverRef.url, {
            headers: wsHeaders,
            agent: getWebSocketProxyAgent(serverRef.url),
            ...(tlsOptions || {}),
          })
        }
        transport = new WebSocketTransport(wsClient)
      } else if (serverRef.type === 'http') {
        logMCPDebug(name, `Initializing HTTP transport to ${serverRef.url}`)
        logMCPDebug(
          name,
          `Node version: ${process.version}, Platform: ${process.platform}`,
        )
        logMCPDebug(
          name,
          `Environment: ${jsonStringify({
            NODE_OPTIONS: process.env.NODE_OPTIONS || 'not set',
            UV_THREADPOOL_SIZE: process.env.UV_THREADPOOL_SIZE || 'default',
            HTTP_PROXY: process.env.HTTP_PROXY || 'not set',
            HTTPS_PROXY: process.env.HTTPS_PROXY || 'not set',
            NO_PROXY: process.env.NO_PROXY || 'not set',
          })}`,
        )

        // Create an auth provider for this server
        const authProvider = new CrabCodeAuthProvider(name, serverRef)

        // Get combined headers (static + dynamic)
        const combinedHeaders = await getMcpServerHeaders(name, serverRef)

        // Check if this server has stored OAuth tokens. If so, the SDK's
        // authProvider will set Authorization — don't override with the
        // session ingress token (SDK merges requestInit AFTER authProvider).
        // CCR proxy URLs (ccr_shttp_mcp) have no stored OAuth, so they still
        // get the ingress token. See PR #24454 discussion.
        const hasOAuthTokens = !!(await authProvider.tokens())

        // Use the auth provider with StreamableHTTPClientTransport
        const proxyOptions = getProxyFetchOptions()
        logMCPDebug(
          name,
          `Proxy options: ${proxyOptions.dispatcher ? 'custom dispatcher' : 'default'}`,
        )

        const transportOptions: StreamableHTTPClientTransportOptions = {
          authProvider,
          // Use fresh timeout per request to avoid stale AbortSignal bug.
          // Step-up detection wraps innermost so the 403 is seen before the
          // SDK's handler calls auth() → tokens().
          fetch: wrapFetchWithTimeout(
            wrapFetchWithStepUpDetection(createFetchWithInit(), authProvider),
          ),
          requestInit: {
            ...proxyOptions,
            headers: {
              'User-Agent': getMCPUserAgent(),
              ...(sessionIngressToken &&
                !hasOAuthTokens && {
                  Authorization: `Bearer ${sessionIngressToken}`,
                }),
              ...combinedHeaders,
            },
          },
        }

        // Redact sensitive headers before logging
        const headersForLogging = transportOptions.requestInit?.headers
          ? mapValues(
              transportOptions.requestInit.headers as Record<string, string>,
              (value, key) =>
                key.toLowerCase() === 'authorization' ? '[REDACTED]' : value,
            )
          : undefined

        logMCPDebug(
          name,
          `HTTP transport options: ${jsonStringify({
            url: serverRef.url,
            headers: headersForLogging,
            hasAuthProvider: !!authProvider,
            timeoutMs: MCP_REQUEST_TIMEOUT_MS,
          })}`,
        )

        transport = new StreamableHTTPClientTransport(
          new URL(serverRef.url),
          transportOptions,
        )
        logMCPDebug(name, `HTTP transport created successfully`)
      } else if (serverRef.type === 'sdk') {
        throw new Error('SDK servers should be handled in print.ts')
      } else if (serverRef.type === 'acosmi-proxy') {
        logMCPDebug(
          name,
          `Initializing acosmi.com proxy transport for server ${serverRef.id}`,
        )

        const tokens = getAcosmiOAuthTokens()
        if (!tokens) {
          throw new Error('No acosmi.com OAuth token found')
        }

        const oauthConfig = getOauthConfig()
        const proxyUrl = `${oauthConfig.MCP_PROXY_URL}${oauthConfig.MCP_PROXY_PATH.replace('{server_id}', serverRef.id)}`

        logMCPDebug(name, `Using acosmi.com proxy at ${proxyUrl}`)

        // eslint-disable-next-line eslint-plugin-n/no-unsupported-features/node-builtins
        const fetchWithAuth = createAcosmiProxyFetch(globalThis.fetch)

        const proxyOptions = getProxyFetchOptions()
        const transportOptions: StreamableHTTPClientTransportOptions = {
          // Wrap fetchWithAuth with fresh timeout per request
          fetch: wrapFetchWithTimeout(fetchWithAuth),
          requestInit: {
            ...proxyOptions,
            headers: {
              'User-Agent': getMCPUserAgent(),
              'X-Mcp-Client-Session-Id': getSessionId(),
            },
          },
        }

        transport = new StreamableHTTPClientTransport(
          new URL(proxyUrl),
          transportOptions,
        )
        logMCPDebug(name, `acosmi.com proxy transport created successfully`)
      } else if (serverRef.type === 'stdio' || !serverRef.type) {
        // Plugin MCP commands have already passed stdioPreflight: execute the
        // canonical argv directly from the canonical plugin root. A global
        // shell prefix is a compatibility knob for manually-authored MCP
        // configs and must never turn an installed plugin command into shell
        // text.
        const shellPrefix = serverRef.pluginMcp
          ? undefined
          : process.env.CRABCODE_SHELL_PREFIX
        const finalCommand = shellPrefix || serverRef.command
        const finalArgs = shellPrefix
          ? [[serverRef.command, ...serverRef.args].join(' ')]
          : serverRef.args
        transport = new ExactStdioClientTransport({
          command: finalCommand,
          args: finalArgs,
          ...(serverRef.pluginMcp?.pluginRoot
            ? { cwd: serverRef.pluginMcp.pluginRoot }
            : {}),
          env: {
            ...subprocessEnv(),
            ...serverRef.env,
          } as Record<string, string>,
          stderr: 'pipe', // prevents error output from the MCP server from printing to the UI
        })
      } else {
        throw new Error(`Unsupported server type: ${serverRef.type}`)
      }
      if (!transport) {
        throw new Error(`MCP server "${name}" did not create a transport`)
      }

      // Set up stderr logging for stdio transport before connecting in case there are any stderr
      // outputs emitted during the connection start (this can be useful for debugging failed connections).
      // Store handler reference for cleanup to prevent memory leaks
      let stderrHandler: ((data: Buffer) => void) | undefined
      let stderrOutput = ''
      if (serverRef.type === 'stdio' || !serverRef.type) {
        const stdioTransport = transport as ExactStdioClientTransport
        if (stdioTransport.stderr) {
          stderrHandler = (data: Buffer) => {
            // Cap stderr accumulation to prevent unbounded memory growth
            if (stderrOutput.length < 64 * 1024 * 1024) {
              try {
                stderrOutput += data.toString()
              } catch {
                // Ignore errors from exceeding max string length
              }
            }
          }
          stdioTransport.stderr.on('data', stderrHandler)
        }
      }

      const client = new Client(
        {
          name: 'crabcode',
          title: 'CrabCode',
          version: MACRO.VERSION ?? 'unknown',
          description: "Acosmi's agentic coding tool",
          websiteUrl: PRODUCT_URL,
        },
        {
          capabilities: {
            roots: {},
            // Empty object declares the capability. Sending {form:{},url:{}}
            // breaks Java MCP SDK servers (Spring AI) whose Elicitation class
            // has zero fields and fails on unknown properties.
            elicitation: {},
          },
        },
      )

      // Add debug logging for client events if available
      if (serverRef.type === 'http') {
        logMCPDebug(name, `Client created, setting up request handler`)
      }

      client.setRequestHandler(ListRootsRequestSchema, async () => {
        logMCPDebug(name, `Received ListRoots request from server`)
        return {
          roots: [
            {
              uri: pathToFileURL(getOriginalCwd()).href,
            },
          ],
        }
      })

      // Add a timeout to connection attempts to prevent tests from hanging indefinitely
      logMCPDebug(
        name,
        `Starting connection with timeout of ${getConnectionTimeoutMs()}ms`,
      )

      // For HTTP transport, try a basic connectivity test first
      if (serverRef.type === 'http') {
        logMCPDebug(name, `Testing basic HTTP connectivity to ${serverRef.url}`)
        try {
          const testUrl = new URL(serverRef.url)
          logMCPDebug(
            name,
            `Parsed URL: host=${testUrl.hostname}, port=${testUrl.port || 'default'}, protocol=${testUrl.protocol}`,
          )

          // Log DNS resolution attempt
          if (
            testUrl.hostname === '127.0.0.1' ||
            testUrl.hostname === 'localhost'
          ) {
            logMCPDebug(name, `Using loopback address: ${testUrl.hostname}`)
          }
        } catch (urlError) {
          logMCPDebug(name, `Failed to parse URL: ${urlError}`)
        }
      }

      const connectPromise = client.connect(transport)
      const timeoutPromise = new Promise<never>((_, reject) => {
        const timeoutId = setTimeout(() => {
          const elapsed = Date.now() - connectStartTime
          logMCPDebug(
            name,
            `Connection timeout triggered after ${elapsed}ms (limit: ${getConnectionTimeoutMs()}ms)`,
          )
          // The outer failure path awaits this same idempotent close promise
          // before returning a failed connection.
          transport!.close().catch(() => {})
          reject(
            new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
              `MCP server "${name}" connection timed out after ${getConnectionTimeoutMs()}ms`,
              'MCP connection timeout',
            ),
          )
        }, getConnectionTimeoutMs())

        // Clean up timeout if connect resolves or rejects
        connectPromise.then(
          () => {
            clearTimeout(timeoutId)
          },
          _error => {
            clearTimeout(timeoutId)
          },
        )
      })

      try {
        await Promise.race([connectPromise, timeoutPromise])
        if (stderrOutput) {
          logMCPError(name, `Server stderr: ${stderrOutput}`)
          stderrOutput = '' // Release accumulated string to prevent memory growth
        }
        const elapsed = Date.now() - connectStartTime
        logMCPDebug(
          name,
          `Successfully connected (transport: ${serverRef.type || 'stdio'}) in ${elapsed}ms`,
        )
      } catch (error) {
        const elapsed = Date.now() - connectStartTime
        // SSE-specific error logging
        if (serverRef.type === 'sse' && error instanceof Error) {
          logMCPDebug(
            name,
            `SSE Connection failed after ${elapsed}ms: ${jsonStringify({
              url: serverRef.url,
              error: error.message,
              errorType: error.constructor.name,
              stack: error.stack,
            })}`,
          )
          logMCPError(name, error)

          if (error instanceof UnauthorizedError) {
            return handleRemoteAuthFailure(name, serverRef, 'sse')
          }
        } else if (serverRef.type === 'http' && error instanceof Error) {
          const errorObj = error as Error & {
            cause?: unknown
            code?: string
            errno?: string | number
            syscall?: string
          }
          logMCPDebug(
            name,
            `HTTP Connection failed after ${elapsed}ms: ${error.message} (code: ${errorObj.code || 'none'}, errno: ${errorObj.errno || 'none'})`,
          )
          logMCPError(name, error)

          if (error instanceof UnauthorizedError) {
            return handleRemoteAuthFailure(name, serverRef, 'http')
          }
        } else if (
          serverRef.type === 'acosmi-proxy' &&
          error instanceof Error
        ) {
          logMCPDebug(
            name,
            `acosmi.com proxy connection failed after ${elapsed}ms: ${error.message}`,
          )
          logMCPError(name, error)

          // StreamableHTTPError has a `code` property with the HTTP status
          const errorCode = (error as Error & { code?: number }).code
          if (errorCode === 401) {
            return handleRemoteAuthFailure(name, serverRef, 'acosmi-proxy')
          }
        } else if (
          serverRef.type === 'sse-ide' ||
          serverRef.type === 'ws-ide'
        ) {
          logEvent('tengu_mcp_ide_server_connection_failed', {
            connectionDurationMs: elapsed,
          })
        }
        transport.close().catch(() => {})
        if (stderrOutput) {
          logMCPError(name, `Server stderr: ${stderrOutput}`)
        }
        throw error
      }

      const capabilities = client.getServerCapabilities()
      const serverVersion = client.getServerVersion()
      const rawInstructions = client.getInstructions()
      let instructions = rawInstructions
      if (
        rawInstructions &&
        rawInstructions.length > MAX_MCP_DESCRIPTION_LENGTH
      ) {
        instructions =
          rawInstructions.slice(0, MAX_MCP_DESCRIPTION_LENGTH) + '… [truncated]'
        logMCPDebug(
          name,
          `Server instructions truncated from ${rawInstructions.length} to ${MAX_MCP_DESCRIPTION_LENGTH} chars`,
        )
      }

      // Log successful connection details
      logMCPDebug(
        name,
        `Connection established with capabilities: ${jsonStringify({
          hasTools: !!capabilities?.tools,
          hasPrompts: !!capabilities?.prompts,
          hasResources: !!capabilities?.resources,
          hasResourceSubscribe: !!capabilities?.resources?.subscribe,
          serverVersion: serverVersion || 'unknown',
        })}`,
      )
      logForDebugging(
        `[MCP] Server "${name}" connected with subscribe=${!!capabilities?.resources?.subscribe}`,
      )

      // Register default elicitation handler that returns cancel during the
      // window before registerElicitationHandler overwrites it in
      // onConnectionAttempt (useManageMCPConnections).
      client.setRequestHandler(ElicitRequestSchema, async request => {
        logMCPDebug(
          name,
          `Elicitation request received during initialization: ${jsonStringify(request)}`,
        )
        return { action: 'cancel' as const }
      })

      if (serverRef.type === 'sse-ide' || serverRef.type === 'ws-ide') {
        const ideConnectionDurationMs = Date.now() - connectStartTime
        logEvent('tengu_mcp_ide_server_connection_succeeded', {
          connectionDurationMs: ideConnectionDurationMs,
          serverVersion:
            serverVersion as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        })
        try {
          void maybeNotifyIDEConnected(client)
        } catch (error) {
          logMCPError(
            name,
            `Failed to send ide_connected notification: ${error}`,
          )
        }
      }

      // Enhanced connection drop detection and logging for all transport types
      const connectionStartTime = Date.now()
      let hasErrorOccurred = false

      // Store original handlers
      const originalOnerror = client.onerror
      const originalOnclose = client.onclose

      // The SDK's transport calls onerror on connection failures but doesn't call onclose,
      // which CC uses to trigger reconnection. We bridge this gap by tracking consecutive
      // terminal errors and manually closing after MAX_ERRORS_BEFORE_RECONNECT failures.
      let consecutiveConnectionErrors = 0
      const MAX_ERRORS_BEFORE_RECONNECT = 3

      // Guard against re-entry: close() aborts in-flight streams which may fire
      // onerror again before the close chain completes.
      let hasTriggeredClose = false

      // client.close() → transport.close() → transport.onclose → SDK's _onclose():
      // rejects all pending request handlers (so hung callTool() promises fail with
      // McpError -32000 "Connection closed") and then invokes our client.onclose
      // handler below (which clears the memo cache so the next call reconnects).
      // Calling client.onclose?.() directly would only clear the cache — pending
      // tool calls would stay hung.
      const closeTransportAndRejectPending = (reason: string) => {
        if (hasTriggeredClose) return
        hasTriggeredClose = true
        logMCPDebug(name, `Closing transport (${reason})`)
        void client.close().catch(e => {
          logMCPDebug(name, `Error during close: ${errorMessage(e)}`)
        })
      }

      const isTerminalConnectionError = (msg: string): boolean => {
        return (
          msg.includes('ECONNRESET') ||
          msg.includes('ETIMEDOUT') ||
          msg.includes('EPIPE') ||
          msg.includes('EHOSTUNREACH') ||
          msg.includes('ECONNREFUSED') ||
          msg.includes('Body Timeout Error') ||
          msg.includes('terminated') ||
          // SDK SSE reconnection intermediate errors — may be wrapped around the
          // actual network error, so the substrings above won't match
          msg.includes('SSE stream disconnected') ||
          msg.includes('Failed to reconnect SSE stream')
        )
      }

      // Enhanced error handler with detailed logging
      client.onerror = (error: Error) => {
        const uptime = Date.now() - connectionStartTime
        hasErrorOccurred = true
        const transportType = serverRef.type || 'stdio'

        // Log the connection drop with context
        logMCPDebug(
          name,
          `${transportType.toUpperCase()} connection dropped after ${Math.floor(uptime / 1000)}s uptime`,
        )

        // Log specific error details for debugging
        if (error.message) {
          if (error.message.includes('ECONNRESET')) {
            logMCPDebug(
              name,
              `Connection reset - server may have crashed or restarted`,
            )
          } else if (error.message.includes('ETIMEDOUT')) {
            logMCPDebug(
              name,
              `Connection timeout - network issue or server unresponsive`,
            )
          } else if (error.message.includes('ECONNREFUSED')) {
            logMCPDebug(name, `Connection refused - server may be down`)
          } else if (error.message.includes('EPIPE')) {
            logMCPDebug(
              name,
              `Broken pipe - server closed connection unexpectedly`,
            )
          } else if (error.message.includes('EHOSTUNREACH')) {
            logMCPDebug(name, `Host unreachable - network connectivity issue`)
          } else if (error.message.includes('ESRCH')) {
            logMCPDebug(
              name,
              `Process not found - stdio server process terminated`,
            )
          } else if (error.message.includes('spawn')) {
            logMCPDebug(
              name,
              `Failed to spawn process - check command and permissions`,
            )
          } else {
            logMCPDebug(name, `Connection error: ${error.message}`)
          }
        }

        // For HTTP transports, detect session expiry (404 + JSON-RPC -32001)
        // and close the transport so pending tool calls reject and the next
        // call reconnects with a fresh session ID.
        if (
          (transportType === 'http' || transportType === 'acosmi-proxy') &&
          isMcpSessionExpiredError(error)
        ) {
          logMCPDebug(
            name,
            `MCP session expired (server returned 404 with session-not-found), triggering reconnection`,
          )
          closeTransportAndRejectPending('session expired')
          if (originalOnerror) {
            originalOnerror(error)
          }
          return
        }

        // For remote transports (SSE/HTTP), track terminal connection errors
        // and trigger reconnection via close if we see repeated failures.
        if (
          transportType === 'sse' ||
          transportType === 'http' ||
          transportType === 'acosmi-proxy'
        ) {
          // The SDK's StreamableHTTP transport fires this after exhausting its
          // own SSE reconnect attempts (default maxRetries: 2) — but it never
          // calls onclose, so pending callTool() promises hang indefinitely.
          // This is the definitive "transport gave up" signal.
          if (error.message.includes('Maximum reconnection attempts')) {
            closeTransportAndRejectPending('SSE reconnection exhausted')
            if (originalOnerror) {
              originalOnerror(error)
            }
            return
          }

          if (isTerminalConnectionError(error.message)) {
            consecutiveConnectionErrors++
            logMCPDebug(
              name,
              `Terminal connection error ${consecutiveConnectionErrors}/${MAX_ERRORS_BEFORE_RECONNECT}`,
            )

            if (consecutiveConnectionErrors >= MAX_ERRORS_BEFORE_RECONNECT) {
              consecutiveConnectionErrors = 0
              closeTransportAndRejectPending('max consecutive terminal errors')
            }
          } else {
            // Non-terminal error (e.g., transient issue), reset counter
            consecutiveConnectionErrors = 0
          }
        }

        // Call original handler
        if (originalOnerror) {
          originalOnerror(error)
        }
      }

      // Enhanced close handler with connection drop context
      client.onclose = () => {
        const uptime = Date.now() - connectionStartTime
        const transportType = serverRef.type ?? 'unknown'

        logMCPDebug(
          name,
          `${transportType.toUpperCase()} connection closed after ${Math.floor(uptime / 1000)}s (${hasErrorOccurred ? 'with errors' : 'cleanly'})`,
        )

        // Clear the memoization cache so next operation reconnects
        const key = getServerCacheKey(name, serverRef)

        // Also clear fetch caches (keyed by server name). Reconnection
        // creates a new connection object; without clearing, the next
        // fetch would return stale tools/resources from the old connection.
        fetchToolsForClient.cache.delete(name)
        fetchResourcesForClient.cache.delete(name)
        fetchCommandsForClient.cache.delete(name)
        connectToServer.cache.delete(key)
        logMCPDebug(name, `Cleared connection cache for reconnection`)

        if (originalOnclose) {
          originalOnclose()
        }
      }

      const cleanup = async () => {
        // Remove stderr event listener to prevent memory leaks
        if (stderrHandler && (serverRef.type === 'stdio' || !serverRef.type)) {
          const stdioTransport = transport as ExactStdioClientTransport
          stdioTransport.stderr?.off('data', stderrHandler)
        }

        // ExactStdioClientTransport owns the full process tree. Its close path
        // performs stdin grace, tree escalation and an unbounded terminal
        // witness; network transports likewise propagate close failures.
        await client.close()
      }

      // Register cleanup for all transport types - even network transports might need cleanup
      // This ensures all MCP servers get properly terminated, not just stdio ones
      const cleanupUnregister = registerCleanup(cleanup)

      // Create the wrapped cleanup that includes unregistering
      const wrappedCleanup = async () => {
        await cleanup()
        cleanupUnregister?.()
      }

      const connectionDurationMs = Date.now() - connectStartTime
      logEvent('tengu_mcp_server_connection_succeeded', {
        connectionDurationMs,
        transportType: (serverRef.type ??
          'stdio') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        totalServers: serverStats?.totalServers,
        stdioCount: serverStats?.stdioCount,
        sseCount: serverStats?.sseCount,
        httpCount: serverStats?.httpCount,
        sseIdeCount: serverStats?.sseIdeCount,
        wsIdeCount: serverStats?.wsIdeCount,
        ...mcpBaseUrlAnalytics(serverRef),
      })
      return {
        name,
        client,
        type: 'connected' as const,
        capabilities: capabilities ?? {},
        serverInfo: serverVersion,
        instructions,
        config: serverRef,
        cleanup: wrappedCleanup,
        ...(installedHtmlVideoRuntimeAuthority(name, serverRef)
          ? { runtimePathAuthority: 'installed-html-video' as const }
          : {}),
      }
    } catch (error) {
      const connectionDurationMs = Date.now() - connectStartTime
      logEvent('tengu_mcp_server_connection_failed', {
        connectionDurationMs,
        totalServers: serverStats?.totalServers || 1,
        stdioCount:
          serverStats?.stdioCount || (serverRef.type === 'stdio' ? 1 : 0),
        sseCount: serverStats?.sseCount || (serverRef.type === 'sse' ? 1 : 0),
        httpCount:
          serverStats?.httpCount || (serverRef.type === 'http' ? 1 : 0),
        sseIdeCount:
          serverStats?.sseIdeCount || (serverRef.type === 'sse-ide' ? 1 : 0),
        wsIdeCount:
          serverStats?.wsIdeCount || (serverRef.type === 'ws-ide' ? 1 : 0),
        transportType: (serverRef.type ??
          'stdio') as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
        ...mcpBaseUrlAnalytics(serverRef),
      })
      logMCPDebug(
        name,
        `Connection failed after ${connectionDurationMs}ms: ${errorMessage(error)}`,
      )
      logMCPError(name, `Connection failed: ${errorMessage(error)}`)

      const teardownErrors: string[] = []
      if (transport) {
        try {
          await transport.close()
        } catch (cleanupError) {
          teardownErrors.push(errorMessage(cleanupError))
        }
      }
      const failureMessage =
        teardownErrors.length > 0
          ? `${errorMessage(error)}; transport cleanup failed: ${teardownErrors.join('; ')}`
          : errorMessage(error)
      return {
        name,
        type: 'failed' as const,
        config: serverRef,
        error: failureMessage,
      }
    }
  },
  getServerCacheKey,
)

// Use a native Map because stale-server cleanup must iterate memoized keys.
;(connectToServer as unknown as { cache: Map<string, Promise<MCPServerConnection>> }).cache =
  new Map<string, Promise<MCPServerConnection>>()

function detachCrabCodeChannelHandlers(client: ConnectedMCPServer): void {
  if (!client.client) return
  try {
    client.client.removeNotificationHandler('notifications/crabcode/channel')
    client.client.removeNotificationHandler(
      'notifications/crabcode/channel/permission',
    )
  } catch {
    // Handler removal is best-effort; transport cleanup below remains the
    // authoritative teardown and must not be skipped.
  }
}

function clearServerDerivedCaches(name: string): void {
  fetchToolsForClient.cache.delete(name)
  fetchResourcesForClient.cache.delete(name)
  fetchCommandsForClient.cache.delete(name)
}

/**
 * Evict only a connection that already belongs to this process.
 *
 * Unlike `clearServerCache`, this primitive never calls `connectToServer` on a
 * cache miss. Management actions such as clearing OAuth credentials can
 * therefore clean local residue for a worker-owned server without accidentally
 * creating a duplicate main-process transport merely in order to tear it down.
 */
export async function evictExistingServerCache(
  name: string,
  serverRef: ScopedMcpServerConfig,
): Promise<void> {
  const key = getServerCacheKey(name, serverRef)
  const cached = connectToServer.cache.get(key)
  if (cached) {
    try {
      const wrappedClient = await cached
      if (wrappedClient.type === 'connected') {
        detachCrabCodeChannelHandlers(wrappedClient)
        await wrappedClient.cleanup()
      }
    } catch {
      // A rejected cached connection still needs its cache entries removed.
    }
  }
  connectToServer.cache.delete(key)
  clearServerDerivedCaches(name)
}

/**
 * Clears the memoize cache for a specific server
 * @param name Server name
 * @param serverRef Server configuration
 */
export async function clearServerCache(
  name: string,
  serverRef: ScopedMcpServerConfig,
): Promise<void> {
  const key = getServerCacheKey(name, serverRef)

  try {
    const wrappedClient = await connectToServer(name, serverRef)

    if (wrappedClient.type === 'connected') {
      detachCrabCodeChannelHandlers(wrappedClient)
      await wrappedClient.cleanup()
    }
  } catch {
    // Ignore errors - server might have failed to connect
  }

  // Clear from cache (both connection and fetch caches so reconnect
  // fetches fresh tools/resources/commands instead of stale ones)
  connectToServer.cache.delete(key)
  clearServerDerivedCaches(name)
}

/**
 * Cleanup all `connectToServer` cache entries whose server name is NOT in
 * `activeNames`. Awaits + calls `.cleanup()` on the connected wrappers so
 * stdio child processes get killed (otherwise reload leaves stdio orphans
 * — see W-PRINT-APPSERVER-MCP-WIRE 审计 §3 债 #12).
 *
 * Cache keys are JSON tuples `[name, serverRef]` produced and decoded by the
 * shared get/parse helpers above. This remains reversible for every valid MCP
 * server name.
 *
 * Safe to call when no servers are stale (no-op). Fail-soft per-server:
 * a single cleanup throw does not skip subsequent evictions.
 *
 * @param activeNames Server names that are CURRENTLY configured (post-reload
 *                    `getAllMcpConfigs()` result). All others in the cache
 *                    are treated as stale and cleaned up.
 */
export async function clearStaleServerCaches(
  activeNames: Set<string>,
): Promise<void> {
  // lodash memoize.cache is a Map by default; iterate keys defensively.
  const cache = connectToServer.cache as unknown as {
    keys?: () => IterableIterator<string>
    get?: (key: string) => Promise<MCPServerConnection> | undefined
    delete: (key: string) => void
    size?: number
  }
  if (!cache.keys || (typeof cache.size === 'number' && cache.size === 0)) {
    return
  }

  // Snapshot keys so we can mutate the cache mid-iteration.
  const keys: string[] = []
  for (const k of cache.keys()) {
    keys.push(k)
  }

  for (const key of keys) {
    const parsed = parseServerCacheKey(key)
    // Anomalous/legacy key shape: skip rather than guess a name that could
    // collide with a real server.
    if (!parsed) continue
    const { name } = parsed
    if (activeNames.has(name)) {
      continue
    }

    // Stale entry — cleanup + evict. Per-entry try/catch keeps a single
    // failed cleanup from blocking subsequent evictions.
    try {
      const pending = cache.get?.(key)
      if (pending) {
        const wrappedClient = await pending
        if (wrappedClient.type === 'connected') {
          detachCrabCodeChannelHandlers(wrappedClient)
          await wrappedClient.cleanup()
        }
      }
    } catch {
      // Cleanup failed (transport already torn down, child crashed, etc.);
      // still evict the cache entry below.
    }

    cache.delete(key)
    fetchToolsForClient.cache.delete(name)
    fetchResourcesForClient.cache.delete(name)
    fetchCommandsForClient.cache.delete(name)
  }
}

/**
 * Generation-aware reload cleanup. Unlike the legacy name-only helper above,
 * this compares complete cache keys, so a same-name plugin upgrade/config
 * change tears down the old child and cannot reuse its tools cache.
 */
export async function clearStaleServerCachesForConfigs(
  activeConfigs: Readonly<Record<string, ScopedMcpServerConfig>>,
): Promise<void> {
  const activeKeys = new Set(
    Object.entries(activeConfigs).map(([name, config]) =>
      getServerCacheKey(name, config),
    ),
  )
  const cache = connectToServer.cache as unknown as {
    keys?: () => IterableIterator<string>
    get?: (key: string) => Promise<MCPServerConnection> | undefined
    delete: (key: string) => void
    size?: number
  }
  if (!cache.keys || (typeof cache.size === 'number' && cache.size === 0)) return

  const keys = [...cache.keys()]
  for (const key of keys) {
    if (activeKeys.has(key)) continue
    const parsed = parseServerCacheKey(key)
    if (!parsed) continue
    const { name } = parsed
    try {
      const pending = cache.get?.(key)
      if (pending) {
        const client = await pending
        if (client.type === 'connected') {
          detachCrabCodeChannelHandlers(client)
          await client.cleanup()
        }
      }
    } catch {
      // A dead child/transport is still safe to evict.
    }
    cache.delete(key)
    fetchToolsForClient.cache.delete(name)
    fetchResourcesForClient.cache.delete(name)
    fetchCommandsForClient.cache.delete(name)
  }
}

/**
 * Ensures a valid connected client for an MCP server.
 * For most server types, uses the memoization cache if available, or reconnects
 * if the cache was cleared (e.g., after onclose). This ensures tool/resource
 * calls always use a valid connection.
 *
 * SDK MCP servers run in-process and are handled separately via setupSdkMcpClients,
 * so they are returned as-is without going through connectToServer.
 *
 * @param client The connected MCP server client
 * @returns Connected MCP server client (same or reconnected)
 * @throws Error if server cannot be connected
 */
type ActiveMcpConfigResolver = (
  name: string,
) => Promise<ScopedMcpServerConfig | null>

let activeMcpConfigResolverOverride: ActiveMcpConfigResolver | null = null

export function __setActiveMcpConfigResolverForTest(
  resolver: ActiveMcpConfigResolver | null,
): void {
  activeMcpConfigResolverOverride = resolver
}

async function resolveActiveMcpConfigForInvocation(
  name: string,
): Promise<ScopedMcpServerConfig | null> {
  if (activeMcpConfigResolverOverride) {
    return activeMcpConfigResolverOverride(name)
  }
  const { getActiveMcpConfigByName } = await import('./config.js')
  return getActiveMcpConfigByName(name)
}

export async function ensureConnectedClient(
  client: ConnectedMCPServer,
  // The returned client is guaranteed to carry a live `client` handle — this
  // helper exists to (re)establish an invokable connection. Worker-owned
  // handle-free projections are rejected here (W-MCP runtime-ownership
  // consolidation: worker owns invocation; main-proc has no handle), so every
  // caller can deref `.client` without a guard.
): Promise<ConnectedMCPServer & { client: Client }> {
  let invocationConfig = client.config
  // A cached Tool/Resource object may outlive a settings or plugin activation
  // change. Re-resolve configured servers at the invocation boundary so an
  // old object cannot resurrect a disabled/removed/plugin-blocked transport.
  // Dynamic non-plugin clients (IDE/agent-inline) are intentionally exempt:
  // they are owned by their enclosing runtime rather than global config.
  if (client.config.pluginMcp || client.config.scope !== 'dynamic') {
    const freshConfig = await resolveActiveMcpConfigForInvocation(client.name)
    if (!freshConfig) {
      throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
        `MCP server "${client.name}" is no longer authorized`,
        'MCP server not active',
      )
    }
    // A Tool object captures its schema, description and annotation-based
    // permission classification from the connection generation that listed it.
    // Never transparently point that stale object at a different config: a tool
    // marked read-only in generation A could otherwise invoke a same-named,
    // destructive tool after generation B is installed. The next owner boundary
    // rebuilds the tool pool from the fresh generation.
    if (
      getServerCacheKey(client.name, freshConfig) !==
      getServerCacheKey(client.name, client.config)
    ) {
      throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
        `MCP server "${client.name}" configuration changed after this tool was listed`,
        'MCP tool snapshot stale',
      )
    }
    invocationConfig = freshConfig
  }

  // SDK MCP servers run in-process and are handled separately via setupSdkMcpClients
  if (invocationConfig.type === 'sdk') {
    if (!client.client) {
      throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
        `MCP server "${client.name}" has no live client handle`,
        'MCP server not connected',
      )
    }
    return client as ConnectedMCPServer & { client: Client }
  }

  const connectedClient = await connectToServer(client.name, invocationConfig)
  if (connectedClient.type !== 'connected' || !connectedClient.client) {
    throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
      `MCP server "${client.name}" is not connected`,
      'MCP server not connected',
    )
  }
  return connectedClient as ConnectedMCPServer & { client: Client }
}

/**
 * Compares two MCP server configurations to determine if they are equivalent.
 * Used to detect when a server needs to be reconnected due to config changes.
 */
export function areMcpConfigsEqual(
  a: ScopedMcpServerConfig,
  b: ScopedMcpServerConfig,
): boolean {
  // Quick type check first
  if (a.type !== b.type) return false

  // Compare by serializing - this handles all config variations
  // We exclude 'scope' from comparison since it's metadata, not connection config
  const { scope: _scopeA, ...configA } = a
  const { scope: _scopeB, ...configB } = b
  return jsonStringify(configA) === jsonStringify(configB)
}

// Max cache size for fetch* caches. Keyed by server name (stable across
// reconnects), bounded to prevent unbounded growth with many MCP servers.
const MCP_FETCH_CACHE_SIZE = 20

/**
 * Encode MCP tool input for the auto-mode security classifier.
 * Exported so the auto-mode eval scripts can mirror production encoding
 * for `mcp__*` tool stubs without duplicating this logic.
 */
export function mcpToolInputToAutoClassifierInput(
  input: Record<string, unknown>,
  toolName: string,
): string {
  const keys = Object.keys(input)
  return keys.length > 0
    ? keys.map(k => `${k}=${String(input[k])}`).join(' ')
    : toolName
}

export const fetchToolsForClient = memoizeWithLRU(
  async (client: MCPServerConnection): Promise<Tool[]> => {
    if (client.type !== 'connected') return []
    // W-MCP-RUNTIME-OWNERSHIP: worker owns invocation; main-proc has no handle.
    // A handle-free connected projection exposes its tools via `workerMeta`,
    // not this in-process list path — skip rather than deref a missing handle.
    if (!client.client) return []

    try {
      if (!client.capabilities?.tools) {
        return []
      }

      const result = (await client.client.request(
        { method: 'tools/list' },
        ListToolsResultSchema,
      )) as ListToolsResult

      // Sanitize tool data from MCP server
      const toolsToProcess = recursivelySanitizeUnicode(result.tools)

      // Check if we should skip the mcp__ prefix for SDK MCP servers
      const skipPrefix =
        client.config.type === 'sdk' &&
        isEnvTruthy(process.env.CRABCODE_AGENT_SDK_MCP_NO_PREFIX)

      // Convert MCP tools to our Tool format
      return toolsToProcess
        .map((tool): Tool => {
          const fullyQualifiedName = buildMcpToolName(client.name, tool.name)
          return {
            ...MCPTool,
            // In skip-prefix mode, use the original name for model invocation so MCP tools
            // can override builtins by name. mcpInfo is used for permission checking.
            name: skipPrefix ? tool.name : fullyQualifiedName,
            mcpInfo: { serverName: client.name, toolName: tool.name },
            isMcp: true,
            // Collapse whitespace: _meta is open to external MCP servers, and
            // a newline here would inject orphan lines into the deferred-tool
            // list (formatDeferredToolLine joins on '\n').
            searchHint:
              typeof tool._meta?.['acosmi/searchHint'] === 'string'
                ? tool._meta['acosmi/searchHint']
                    .replace(/\s+/g, ' ')
                    .trim() || undefined
                : undefined,
            alwaysLoad: tool._meta?.['acosmi/alwaysLoad'] === true,
            async description() {
              return tool.description ?? ''
            },
            async prompt() {
              const desc = tool.description ?? ''
              return desc.length > MAX_MCP_DESCRIPTION_LENGTH
                ? desc.slice(0, MAX_MCP_DESCRIPTION_LENGTH) + '… [truncated]'
                : desc
            },
            isConcurrencySafe() {
              return tool.annotations?.readOnlyHint ?? false
            },
            isReadOnly() {
              return tool.annotations?.readOnlyHint ?? false
            },
            toAutoClassifierInput(input) {
              return mcpToolInputToAutoClassifierInput(input, tool.name)
            },
            isDestructive() {
              return tool.annotations?.destructiveHint ?? false
            },
            isOpenWorld() {
              return tool.annotations?.openWorldHint ?? false
            },
            isSearchOrReadCommand() {
              return classifyMcpToolForCollapse(client.name, tool.name)
            },
            inputJSONSchema: tool.inputSchema as Tool['inputJSONSchema'],
            async checkPermissions() {
              return {
                behavior: 'passthrough' as const,
                message: 'MCPTool requires permission.',
                suggestions: [
                  {
                    type: 'addRules' as const,
                    rules: [
                      {
                        toolName: fullyQualifiedName,
                        ruleContent: undefined,
                      },
                    ],
                    behavior: 'allow' as const,
                    destination: 'localSettings' as const,
                  },
                ],
              }
            },
            async call(
              args: Record<string, unknown>,
              context,
              _canUseTool,
              parentMessage,
              onProgress?: ToolCallProgress<MCPProgress>,
            ) {
              const toolUseId = extractToolUseId(parentMessage)
              const meta: Record<string, unknown> = toolUseId
                ? { 'crabcode/toolUseId': toolUseId }
                : {}

              // Emit progress when tool starts
              if (onProgress && toolUseId) {
                onProgress({
                  toolUseID: toolUseId,
                  data: {
                    type: 'mcp_progress',
                    status: 'started',
                    serverName: client.name,
                    toolName: tool.name,
                  },
                })
              }

              const startTime = Date.now()
              const MAX_SESSION_RETRIES = 1
              for (let attempt = 0; ; attempt++) {
                try {
                  const connectedClient = await ensureConnectedClient(client)
                  const mcpResult = await callMCPToolWithUrlElicitationRetry({
                    client: connectedClient,
                    clientConnection: client,
                    tool: tool.name,
                    args,
                    meta,
                    signal: context.abortController.signal,
                    setAppState: context.setAppState,
                    onProgress:
                      onProgress && toolUseId
                        ? progressData => {
                            onProgress({
                              toolUseID: toolUseId,
                              data: progressData,
                            })
                          }
                        : undefined,
                    handleElicitation: context.handleElicitation,
                  })

                  // Emit progress when tool completes successfully
                  if (onProgress && toolUseId) {
                    onProgress({
                      toolUseID: toolUseId,
                      data: {
                        type: 'mcp_progress',
                        status: 'completed',
                        serverName: client.name,
                        toolName: tool.name,
                        elapsedTimeMs: Date.now() - startTime,
                      },
                    })
                  }

                  return {
                    data: mcpResult.content,
                    ...(mcpResult.artifacts?.length
                      ? { artifacts: mcpResult.artifacts }
                      : {}),
                    ...((mcpResult._meta || mcpResult.structuredContent) && {
                      mcpMeta: {
                        ...(mcpResult._meta && {
                          _meta: mcpResult._meta,
                        }),
                        ...(mcpResult.structuredContent && {
                          structuredContent: mcpResult.structuredContent,
                        }),
                      },
                    }),
                  }
                } catch (error) {
                  // Session expired — the connection cache has been
                  // cleared, so retry with a fresh client.
                  if (
                    error instanceof McpSessionExpiredError &&
                    attempt < MAX_SESSION_RETRIES
                  ) {
                    logMCPDebug(
                      client.name,
                      `Retrying tool '${tool.name}' after session recovery`,
                    )
                    continue
                  }

                  // Emit progress when tool fails
                  if (onProgress && toolUseId) {
                    onProgress({
                      toolUseID: toolUseId,
                      data: {
                        type: 'mcp_progress',
                        status: 'failed',
                        serverName: client.name,
                        toolName: tool.name,
                        elapsedTimeMs: Date.now() - startTime,
                      },
                    })
                  }
                  // Wrap MCP SDK errors so telemetry gets useful context
                  // instead of just "Error" or "McpError" (the constructor
                  // name). MCP SDK errors are protocol-level messages and
                  // don't contain user file paths or code.
                  if (
                    error instanceof Error &&
                    !(
                      error instanceof
                      TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS
                    )
                  ) {
                    const name = error.constructor.name
                    if (name === 'Error') {
                      throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
                        error.message,
                        error.message.slice(0, 200),
                      )
                    }
                    // McpError has a numeric `code` with the JSON-RPC error
                    // code (e.g. -32000 ConnectionClosed, -32001 RequestTimeout)
                    if (
                      name === 'McpError' &&
                      'code' in error &&
                      typeof error.code === 'number'
                    ) {
                      throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
                        error.message,
                        `McpError ${error.code}`,
                      )
                    }
                  }
                  throw error
                }
              }
            },
            userFacingName() {
              // Prefer title annotation if available, otherwise use tool name
              const displayName = tool.annotations?.title || tool.name
              return `${client.name} - ${displayName} (MCP)`
            },
          }
        })
        .filter(isIncludedMcpTool)
    } catch (error) {
      logMCPError(client.name, `Failed to fetch tools: ${errorMessage(error)}`)
      return []
    }
  },
  (client: MCPServerConnection) => client.name,
  MCP_FETCH_CACHE_SIZE,
)

export const fetchResourcesForClient = memoizeWithLRU(
  async (client: MCPServerConnection): Promise<ServerResource[]> => {
    if (client.type !== 'connected') return []
    // W-MCP-RUNTIME-OWNERSHIP: worker owns invocation; main-proc has no handle.
    if (!client.client) return []

    try {
      if (!client.capabilities?.resources) {
        return []
      }

      const result = await client.client.request(
        { method: 'resources/list' },
        ListResourcesResultSchema,
      )

      if (!result.resources) return []

      // Add server name to each resource
      return result.resources.map(resource => ({
        ...resource,
        server: client.name,
      }))
    } catch (error) {
      logMCPError(
        client.name,
        `Failed to fetch resources: ${errorMessage(error)}`,
      )
      return []
    }
  },
  (client: MCPServerConnection) => client.name,
  MCP_FETCH_CACHE_SIZE,
)

export const fetchCommandsForClient = memoizeWithLRU(
  async (client: MCPServerConnection): Promise<Command[]> => {
    if (client.type !== 'connected') return []
    // W-MCP-RUNTIME-OWNERSHIP: worker owns invocation; main-proc has no handle.
    if (!client.client) return []

    try {
      if (!client.capabilities?.prompts) {
        return []
      }

      // Request prompts list from client
      const result = (await client.client.request(
        { method: 'prompts/list' },
        ListPromptsResultSchema,
      )) as ListPromptsResult

      if (!result.prompts) return []

      // Sanitize prompt data from MCP server
      const promptsToProcess = recursivelySanitizeUnicode(result.prompts)

      // Convert MCP prompts to our Command format
      return promptsToProcess.map(prompt => {
        const argNames = Object.values(prompt.arguments ?? {}).map(k => k.name)
        return {
          type: 'prompt' as const,
          name: buildMcpToolName(client.name, prompt.name),
          description: prompt.description ?? '',
          hasUserSpecifiedDescription: !!prompt.description,
          contentLength: 0, // Dynamic MCP content
          isEnabled: () => true,
          isHidden: false,
          isMcp: true,
          progressMessage: 'running',
          userFacingName() {
            // Use prompt.name (programmatic identifier) not prompt.title (display name)
            // to avoid spaces breaking slash command parsing
            return `${client.name}:${prompt.name} (MCP)`
          },
          argNames,
          source: 'mcp',
          async getPromptForCommand(args: string) {
            const argsArray = args.split(' ')
            try {
              const connectedClient = await ensureConnectedClient(client)
              const result = await connectedClient.client.getPrompt({
                name: prompt.name,
                arguments: zipObject(argNames, argsArray),
              })
              const transformed = await Promise.all(
                result.messages.map(message =>
                  transformResultContent(message.content, connectedClient.name),
                ),
              )
              return transformed.flat()
            } catch (error) {
              logMCPError(
                client.name,
                `Error running command '${prompt.name}': ${errorMessage(error)}`,
              )
              throw error
            }
          },
        }
      })
    } catch (error) {
      logMCPError(
        client.name,
        `Failed to fetch commands: ${errorMessage(error)}`,
      )
      return []
    }
  },
  (client: MCPServerConnection) => client.name,
  MCP_FETCH_CACHE_SIZE,
)

function extractToolUseId(message: AssistantMessage): string | undefined {
  if (message.message.content[0]?.type !== 'tool_use') {
    return undefined
  }
  return message.message.content[0].id
}
