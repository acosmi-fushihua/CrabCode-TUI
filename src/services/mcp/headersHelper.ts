import { getIsNonInteractiveSession } from '../../bootstrap/state.js'
import { checkHasTrustDialogAccepted } from '../../utils/config.js'
import { logAntError } from '../../utils/debug.js'
import { errorMessage } from '../../utils/errors.js'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { logError, logMCPDebug, logMCPError } from '../../utils/log.js'
import { jsonParse } from '../../utils/slowOperations.js'
import { logEvent } from '../analytics/index.js'
import type {
  McpHTTPServerConfig,
  McpSSEServerConfig,
  McpWebSocketServerConfig,
  ScopedMcpServerConfig,
} from './types.js'

/**
 * Check if the MCP server config comes from project settings (projectSettings or localSettings)
 * This is important for security checks
 */
function isMcpServerFromProjectOrLocalSettings(
  config: ScopedMcpServerConfig,
): boolean {
  return config.scope === 'project' || config.scope === 'local'
}

/**
 * Get dynamic headers for an MCP server using the headersHelper script
 * @param serverName The name of the MCP server
 * @param config The MCP server configuration
 * @returns Headers object or null if not configured or failed
 */
export async function getMcpHeadersFromHelper(
  serverName: string,
  config: McpSSEServerConfig | McpHTTPServerConfig | McpWebSocketServerConfig,
): Promise<Record<string, string> | null> {
  if (!config.headersHelper) {
    return null
  }

  // Security gate for project/local-scoped headersHelper. The helper runs
  // `shell:true` at CONNECT time (before the model loop) = a silent RCE
  // surface, so a project/local-scoped helper from a (potentially cloned,
  // attacker-controlled) workspace must NOT run without established trust.
  //
  // Trust = the workspace trust dialog was accepted & persisted
  // (`checkHasTrustDialogAccepted()`), honored in BOTH interactive and
  // non-interactive sessions. In a non-interactive session (-p / CI) there is
  // no dialog to surface, so the ONLY additional way to authorize execution is
  // an explicit env opt-in (`CRABCODE_TRUST_PROJECT=1`). Previously a
  // non-interactive session skipped this gate entirely and auto-trusted — that
  // auto-trust is the vulnerability being closed here. `user`-scoped helpers
  // (git credential-helper style in user settings) are unaffected.
  if (
    'scope' in config &&
    isMcpServerFromProjectOrLocalSettings(config as ScopedMcpServerConfig)
  ) {
    const nonInteractive = getIsNonInteractiveSession()
    let trusted = checkHasTrustDialogAccepted()
    if (
      !trusted &&
      nonInteractive &&
      process.env.CRABCODE_TRUST_PROJECT === '1'
    ) {
      trusted = true
    }
    if (!trusted) {
      const error = new Error(
        `Security: headersHelper for MCP server '${serverName}' blocked — ` +
          'workspace trust is not established for this project/local-scoped server' +
          (nonInteractive
            ? ' (non-interactive session: set CRABCODE_TRUST_PROJECT=1 to authorize, or run interactively and accept the trust dialog).'
            : `. If you see this message, post in ${MACRO.FEEDBACK_CHANNEL}.`),
      )
      logAntError('MCP headersHelper invoked before trust check', error)
      logEvent('tengu_mcp_headersHelper_missing_trust', {})
      return null
    }
  }

  try {
    logMCPDebug(serverName, 'Executing headersHelper to get dynamic headers')
    const execResult = await localExecBridge.execCommand({
      command: config.headersHelper,
      args: [],
      shell: true,
      timeout: 10000,
      // Pass server context so one helper script can serve multiple MCP servers
      // (git credential-helper style). See acosmi/issues#28.
      env: {
        ...process.env,
        CRABCODE_MCP_SERVER_NAME: serverName,
        CRABCODE_MCP_SERVER_URL: config.url,
      },
    })
    if (execResult.code !== 0 || !execResult.stdout) {
      throw new Error(
        `headersHelper for MCP server '${serverName}' did not return a valid value`,
      )
    }
    const result = execResult.stdout.trim()

    const headers = jsonParse(result)
    if (
      typeof headers !== 'object' ||
      headers === null ||
      Array.isArray(headers)
    ) {
      throw new Error(
        `headersHelper for MCP server '${serverName}' must return a JSON object with string key-value pairs`,
      )
    }

    // Validate all values are strings
    for (const [key, value] of Object.entries(headers)) {
      if (typeof value !== 'string') {
        throw new Error(
          `headersHelper for MCP server '${serverName}' returned non-string value for key "${key}": ${typeof value}`,
        )
      }
    }

    logMCPDebug(
      serverName,
      `Successfully retrieved ${Object.keys(headers).length} headers from headersHelper`,
    )
    return headers as Record<string, string>
  } catch (error) {
    logMCPError(
      serverName,
      `Error getting headers from headersHelper: ${errorMessage(error)}`,
    )
    logError(
      new Error(
        `Error getting MCP headers from headersHelper for server '${serverName}': ${errorMessage(error)}`,
      ),
    )
    // Return null instead of throwing to avoid blocking the connection
    return null
  }
}

/**
 * Get combined headers for an MCP server (static + dynamic)
 * @param serverName The name of the MCP server
 * @param config The MCP server configuration
 * @returns Combined headers object
 */
export async function getMcpServerHeaders(
  serverName: string,
  config: McpSSEServerConfig | McpHTTPServerConfig | McpWebSocketServerConfig,
): Promise<Record<string, string>> {
  const staticHeaders = config.headers || {}
  const dynamicHeaders =
    (await getMcpHeadersFromHelper(serverName, config)) || {}

  // Dynamic headers override static headers if both are present
  return {
    ...staticHeaders,
    ...dynamicHeaders,
  }
}
