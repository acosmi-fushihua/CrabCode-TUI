import { createHash } from 'node:crypto'

const PLUGIN_MCP_RUNTIME_PREFIX = 'plugin:'
export const PLUGIN_MCP_REMOTE_PACKAGE_PREFIX =
  '__crabcode_remote_mcpb_'
const MAX_PLUGIN_MCP_RUNTIME_NAME_LENGTH = 4096
const MAX_PLUGIN_MCP_IDENTITY_SEGMENT_LENGTH = 1024

function identityHash(value: string): string {
  return createHash('sha256').update(value).digest('hex')
}

export function isReservedPluginMcpAuthoredServerName(value: string): boolean {
  return (
    value.startsWith(PLUGIN_MCP_RUNTIME_PREFIX) ||
    value.startsWith(PLUGIN_MCP_REMOTE_PACKAGE_PREFIX)
  )
}

function encodeIdentitySegment(value: string, label: string): string {
  if (value.length === 0) {
    throw new Error(`plugin MCP ${label} must be non-empty`)
  }
  const encoded = encodeURIComponent(value)
  return encoded.length <= MAX_PLUGIN_MCP_IDENTITY_SEGMENT_LENGTH
    ? `e-${encoded}`
    : `h-${identityHash(value)}`
}

function decodeIdentitySegment(value: string): string | null {
  if (/^h-[0-9a-f]{64}$/.test(value)) return value
  if (!value.startsWith('e-')) return null
  const encoded = value.slice(2)
  if (encoded.length === 0) return null
  try {
    const decoded = decodeURIComponent(encoded)
    return decoded.length > 0 && encodeURIComponent(decoded) === encoded
      ? decoded
      : null
  } catch {
    return null
  }
}

/**
 * Runtime/display key for a plugin MCP server. Source and logical server name
 * are encoded as separate canonical segments, so two plugins with the same
 * manifest name cannot overwrite one another. Consumers must still use the
 * lifecycle inventory metadata for authorization/toggle decisions; this name
 * is an opaque lookup key, not a permission token.
 */
export function buildPluginMcpRuntimeName(
  pluginName: string,
  pluginSource: string,
  serverName: string,
): string {
  return `${PLUGIN_MCP_RUNTIME_PREFIX}${encodeIdentitySegment(pluginName, 'plugin name')}:${encodeIdentitySegment(pluginSource, 'plugin source')}:${encodeIdentitySegment(serverName, 'server name')}`
}

export type PluginMcpRuntimeIdentity = {
  pluginName: string
  pluginSource: string
  serverName: string
}

/** Strict canonical parser. Parsed segments are presentation-only. */
export function parsePluginMcpRuntimeName(
  value: string,
): PluginMcpRuntimeIdentity | null {
  if (
    !value.startsWith(PLUGIN_MCP_RUNTIME_PREFIX) ||
    value.length > MAX_PLUGIN_MCP_RUNTIME_NAME_LENGTH
  ) {
    return null
  }
  const parts = value.split(':')
  const pluginName = decodeIdentitySegment(parts[1] ?? '')
  const pluginSource = decodeIdentitySegment(parts[2] ?? '')
  const serverName = decodeIdentitySegment(parts[3] ?? '')
  if (
    parts.length !== 4 ||
    parts[0] !== 'plugin' ||
    pluginName === null ||
    pluginSource === null ||
    serverName === null
  ) {
    return null
  }
  return {
    pluginName,
    pluginSource,
    serverName,
  }
}

/** Strict parser gate used only by plugin-runtime management boundaries. */
export function isPluginMcpRuntimeName(value: string): boolean {
  return parsePluginMcpRuntimeName(value) !== null
}
