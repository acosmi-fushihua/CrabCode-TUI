/**
 * Pure string utility functions for MCP tool/server name parsing.
 * This file has no heavy dependencies to keep it lightweight for
 * consumers that only need string parsing (e.g., permissionValidation).
 */

import { normalizeNameForMCP } from './normalization.js'
import { createHash } from 'node:crypto'
import { parsePluginMcpRuntimeName } from './pluginMcpIdentity.js'

const MCP_API_NAME_MAX_LENGTH = 64
// 5 (`mcp__`) + 31 + 2 (`__`) + 26 (`h_` + 96-bit hex) = 64.
// Preserve every ordinary server name that can coexist with the bounded
// hashed tool fallback; 32+ must hash.
const MCP_SERVER_NAMESPACE_MAX_LENGTH = 31
const MCP_WIRE_HASH_HEX_LENGTH = 24
const MCP_HASHED_SEGMENT_PREFIX = 'h_'
const MCP_ESCAPED_SEGMENT_PREFIX = 'e_'
const MCP_PLUGIN_SERVER_PREFIX = 'p_'

function shortHash(
  value: string,
  length = MCP_WIRE_HASH_HEX_LENGTH,
): string {
  return createHash('sha256').update(value).digest('hex').slice(0, length)
}

/**
 * Keep ordinary, bounded MCP names on their long-standing public wire IDs.
 * Only plugin runtime identities and unsafe/oversized names use tagged hashes.
 *
 * The escape tag makes the mapping domain-separated: an authored literal such
 * as `h_<digest>` or `p_<digest>` becomes `e_h_<digest>`/`e_p_<digest>` and
 * therefore cannot impersonate a derived hash or a source-qualified plugin.
 */
function escapeReservedWireSegment(value: string): string {
  // Escape only strings that can actually collide with a derived marker (or
  // an already-escaped marker chain). Ordinary authored names like `h_foo`,
  // `p_custom`, and `e_name` keep their longstanding wire IDs.
  return /^(?:e_)*(?:h_|p_)[0-9a-f]{24}$/.test(value)
    ? `${MCP_ESCAPED_SEGMENT_PREFIX}${value}`
    : value
}

export function getMcpServerApiNamespace(serverName: string): string {
  if (parsePluginMcpRuntimeName(serverName)) {
    return `${MCP_PLUGIN_SERVER_PREFIX}${shortHash(serverName)}`
  }

  const normalized = normalizeNameForMCP(serverName)
  const escaped = escapeReservedWireSegment(normalized)
  if (
    normalized.length > 0 &&
    !normalized.includes('__') &&
    !normalized.endsWith('_') &&
    escaped.length <= MCP_SERVER_NAMESPACE_MAX_LENGTH
  ) {
    return escaped
  }
  return `${MCP_HASHED_SEGMENT_PREFIX}${shortHash(serverName)}`
}

/*
 * Extracts MCP server information from a tool name string
 * @param toolString The string to parse. Expected format: "mcp__serverName__toolName"
 * @returns An object containing server name and optional tool name, or null if not a valid MCP rule
 *
 * Known limitation: If a server name contains "__", parsing will be incorrect.
 * For example, "mcp__my__server__tool" would parse as server="my" and tool="server__tool"
 * instead of server="my__server" and tool="tool". This is rare in practice since server
 * names typically don't contain double underscores.
 */
export function mcpInfoFromString(toolString: string): {
  serverName: string
  toolName: string | undefined
} | null {
  const parts = toolString.split('__')
  const [mcpPart, serverName, ...toolNameParts] = parts
  if (mcpPart !== 'mcp' || !serverName) {
    return null
  }
  // Join all parts after server name to preserve double underscores in tool names
  const toolName =
    toolNameParts.length > 0 ? toolNameParts.join('__') : undefined
  return { serverName, toolName }
}

/**
 * Generates the MCP tool/command name prefix for a given server
 * @param serverName Name of the MCP server
 * @returns The prefix string
 */
export function getMcpPrefix(serverName: string): string {
  return `mcp__${getMcpServerApiNamespace(serverName)}__`
}

/**
 * Builds a fully qualified MCP tool name from server and tool names.
 * Inverse of mcpInfoFromString().
 * @param serverName Name of the MCP server (unnormalized)
 * @param toolName Name of the tool (unnormalized)
 * @returns The fully qualified name, e.g., "mcp__server__tool"
 */
export function buildMcpToolName(serverName: string, toolName: string): string {
  const prefix = getMcpPrefix(serverName)
  const normalizedToolName = normalizeNameForMCP(toolName)
  const escapedToolName = escapeReservedWireSegment(normalizedToolName)
  const availableLength = MCP_API_NAME_MAX_LENGTH - prefix.length
  const pluginRuntime = parsePluginMcpRuntimeName(serverName) !== null
  if (
    normalizedToolName.length > 0 &&
    (!pluginRuntime || normalizedToolName === toolName) &&
    escapedToolName.length <= availableLength
  ) {
    return `${prefix}${escapedToolName}`
  }

  // 96-bit suffix: strong enough for attacker-chosen/offline tool names and
  // still bounded when paired with the longest server namespace above.
  return `${prefix}${MCP_HASHED_SEGMENT_PREFIX}${shortHash(toolName)}`
}

export function getLegacyMcpPermissionIdentity(
  serverName: string,
  toolName: string,
): { serverNamespace: string; fullyQualifiedToolName: string } | null {
  const identity = parsePluginMcpRuntimeName(serverName)
  const legacyServerName = identity
    ? `plugin:${identity.pluginName}:${identity.serverName}`
    : serverName
  const serverNamespace = normalizeNameForMCP(
    legacyServerName,
  )
  return {
    serverNamespace,
    fullyQualifiedToolName: `mcp__${serverNamespace}__${normalizeNameForMCP(toolName)}`,
  }
}

export const getLegacyPluginMcpPermissionIdentity =
  getLegacyMcpPermissionIdentity

/**
 * Returns the name to use for permission rule matching.
 * For MCP tools, uses the fully qualified mcp__server__tool name so that
 * deny rules targeting builtins (e.g., "Write") don't match unprefixed MCP
 * replacements that share the same display name. Falls back to `tool.name`.
 */
export function getToolNameForPermissionCheck(tool: {
  name: string
  mcpInfo?: { serverName: string; toolName: string }
}): string {
  return tool.mcpInfo
    ? tool.name.startsWith('mcp__')
      ? tool.name
      : buildMcpToolName(tool.mcpInfo.serverName, tool.mcpInfo.toolName)
    : tool.name
}

/*
 * Extracts the display name from an MCP tool/command name
 * @param fullName The full MCP tool/command name (e.g., "mcp__server_name__tool_name")
 * @param serverName The server name to remove from the prefix
 * @returns The display name without the MCP prefix
 */
export function getMcpDisplayName(
  fullName: string,
  serverName: string,
): string {
  const parsed = mcpInfoFromString(fullName)
  if (!parsed?.toolName) return fullName

  const currentNamespace = getMcpServerApiNamespace(serverName)
  const legacyNamespace = normalizeNameForMCP(serverName)
  if (
    parsed.serverName !== currentNamespace &&
    parsed.serverName !== legacyNamespace
  ) {
    return fullName
  }

  return /^e_(?:e_)*(?:h_|p_)[0-9a-f]{24}$/.test(parsed.toolName)
    ? parsed.toolName.slice(MCP_ESCAPED_SEGMENT_PREFIX.length)
    : parsed.toolName
}

/**
 * Extracts just the tool/command display name from a userFacingName
 * @param userFacingName The full user-facing name (e.g., "github - Add comment to issue (MCP)")
 * @returns The display name without server prefix and (MCP) suffix
 */
export function extractMcpToolDisplayName(userFacingName: string): string {
  // This is really ugly but our current Tool type doesn't make it easy to have different display names for different purposes.

  // First, remove the (MCP) suffix if present
  let withoutSuffix = userFacingName.replace(/\s*\(MCP\)\s*$/, '')

  // Trim the result
  withoutSuffix = withoutSuffix.trim()

  // Then, remove the server prefix (everything before " - ")
  const dashIndex = withoutSuffix.indexOf(' - ')
  if (dashIndex !== -1) {
    const displayName = withoutSuffix.substring(dashIndex + 3).trim()
    return displayName
  }

  // If no dash found, return the string without (MCP)
  return withoutSuffix
}
