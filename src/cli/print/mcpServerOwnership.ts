import type { McpServerConfigForProcessTransport } from 'src/entrypoints/agentSdkTypes.js'
import { filterMcpServersByPolicy } from 'src/services/mcp/config.js'
import { getMcpServerApiNamespace } from 'src/services/mcp/mcpStringUtils.js'

export type McpProcessOwner = 'public' | 'plugin'

export const DIRECT_TUI_SDK_MCP_UNSUPPORTED_ERROR =
  'SDK-transport MCP servers are not supported by the direct TUI runtime'

/** Classify only explicit inventory transport evidence; never infer from absence. */
export function isDirectTuiSdkMcpInventoryRecord(
  record:
    | {
        transport?: unknown
        config?: { type?: unknown }
      }
    | null
    | undefined,
): boolean {
  return record?.transport === 'sdk' || record?.config?.type === 'sdk'
}

/**
 * An unsupported SDK row in a mixed public desired set is request-local.
 * Preserve that name's last process owner while allowing process siblings to
 * reconcile normally, and never retain the unsupported row for replay.
 */
export function preservePublicProcessDesiredAcrossSdkRejections(
  requested: Readonly<Record<string, McpServerConfigForProcessTransport>>,
  previousDesired: Readonly<
    Record<string, McpServerConfigForProcessTransport>
  >,
  currentConfigs: Readonly<Record<string, { type?: string }>>,
): {
  desired: Record<string, McpServerConfigForProcessTransport>
  retained: Record<string, McpServerConfigForProcessTransport>
} {
  const desired: Record<string, McpServerConfigForProcessTransport> = {}
  const retained: Record<string, McpServerConfigForProcessTransport> = {}
  for (const [name, config] of Object.entries(requested)) {
    if (config.type !== 'sdk') {
      desired[name] = config
      continue
    }
    const currentConfig = currentConfigs[name]
    const currentProcessConfig =
      currentConfig &&
      (currentConfig.type === undefined ||
        currentConfig.type === 'stdio' ||
        currentConfig.type === 'sse' ||
        currentConfig.type === 'http')
        ? (currentConfig as McpServerConfigForProcessTransport)
        : undefined
    const stableDesired =
      previousDesired[name]?.type !== 'sdk'
        ? previousDesired[name]
        : currentProcessConfig
    if (stableDesired) desired[name] = stableDesired
    // Retention is an execution decision, not desired-state bookkeeping. An
    // absent logical owner must stay absent when the rejected SDK row arrives.
    if (currentProcessConfig) retained[name] = currentProcessConfig
  }
  return { desired, retained }
}

type PrioritizedMcpConfig = {
  scope?: string
  pluginMcp?: unknown
}

/** Existing config authority: managed > local > project > user > plugin > Acosmi. */
export function orderMcpServersByPrecedence<T extends PrioritizedMcpConfig>(
  servers: Readonly<Record<string, T>>,
): Record<string, T> {
  const priority = (config: T): number => {
    if (config.scope === 'enterprise' || config.scope === 'managed') return 60
    if (config.pluginMcp !== undefined) return 20
    if (config.scope === 'local') return 50
    if (config.scope === 'project') return 40
    if (config.scope === 'user') return 30
    if (config.scope === 'acosmi') return 10
    return 0
  }
  return Object.fromEntries(
    Object.entries(servers).sort(
      ([, left], [, right]) => priority(right) - priority(left),
    ),
  )
}

export function filterMcpServersByWireNamespace<T>(
  servers: Readonly<Record<string, T>>,
): {
  accepted: Record<string, T>
  errors: Record<string, string>
} {
  const accepted: Record<string, T> = {}
  const errors: Record<string, string> = {}
  const namespaceOwners = new Map<string, string>()

  for (const [name, config] of Object.entries(servers)) {
    const namespace = getMcpServerApiNamespace(name)
    const existingOwner = namespaceOwners.get(namespace)
    if (existingOwner !== undefined) {
      errors[name] =
        `MCP wire namespace "${namespace}" is already claimed by earlier server "${existingOwner}"`
      continue
    }
    namespaceOwners.set(namespace, name)
    accepted[name] = config
  }

  return { accepted, errors }
}

/**
 * Reserve direct-session namespaces before selecting executable startup rows.
 * Inactive CLI configs remain logical owners, so a lower-authority persisted
 * row must not connect under their wire namespace. Exact-name lower rows must
 * already be omitted by the caller; an active reservation may still appear in
 * both inputs and is returned as executable.
 */
export function admitStartupMcpNamespaceReservations<T>(
  reservations: Readonly<Record<string, T>>,
  active: Readonly<Record<string, T>>,
): {
  acceptedActive: Record<string, T>
  acceptedReservationNames: Set<string>
  errors: Record<string, string>
} {
  const candidates = {
    ...reservations,
    ...Object.fromEntries(
      Object.entries(active).filter(([name]) => !(name in reservations)),
    ),
  }
  const filtered = filterMcpServersByWireNamespace(candidates)
  return {
    acceptedActive: Object.fromEntries(
      Object.entries(active).filter(
        ([name]) => name in filtered.accepted,
      ),
    ),
    acceptedReservationNames: new Set(
      Object.keys(reservations).filter(name => name in filtered.accepted),
    ),
    errors: filtered.errors,
  }
}

/**
 * The native direct-TUI control host cannot service SDK-transport MCP frames.
 * Strip those configs before namespace ownership is assigned so an unsupported
 * server cannot shadow a healthy process-transport server with the same wire
 * namespace.
 */
export function filterDirectTuiProcessMcpServers<
  T extends { type?: string },
>(servers: Readonly<Record<string, T>>): {
  accepted: Record<string, T>
  errors: Record<string, string>
} {
  const accepted: Record<string, T> = {}
  const errors: Record<string, string> = {}
  for (const [name, config] of Object.entries(servers)) {
    if (config.type === 'sdk') {
      errors[name] = DIRECT_TUI_SDK_MCP_UNSUPPORTED_ERROR
    } else {
      accepted[name] = config
    }
  }
  return { accepted, errors }
}

export function filterMcpServersForOwner<T>(
  _owner: McpProcessOwner,
  servers: Record<string, T>,
  otherOwnerNames: ReadonlySet<string>,
): {
  accepted: Record<string, T>
  errors: Record<string, string>
} {
  const accepted: Record<string, T> = {}
  const errors: Record<string, string> = {}
  const otherOwnerNamespaces = new Set(
    [...otherOwnerNames].map(getMcpServerApiNamespace),
  )
  const acceptedNamespaceOwners = new Map<string, string>()

  for (const [name, config] of Object.entries(servers)) {
    const namespace = getMcpServerApiNamespace(name)
    if (otherOwnerNames.has(name)) {
      errors[name] =
        'MCP server name is already owned by another MCP lifecycle'
    } else if (otherOwnerNamespaces.has(namespace)) {
      errors[name] =
        `MCP wire namespace "${namespace}" is already owned by another MCP lifecycle`
    } else if (acceptedNamespaceOwners.has(namespace)) {
      errors[name] =
        `MCP wire namespace "${namespace}" is already claimed by earlier desired server "${acceptedNamespaceOwners.get(namespace)}"`
    } else {
      accepted[name] = config
      acceptedNamespaceOwners.set(namespace, name)
    }
  }

  return { accepted, errors }
}

export function planCapturedMcpPolicyTransitions<T>(
  capturedConfigs: Readonly<Record<string, T>>,
  policyAllowedNames: ReadonlySet<string>,
  currentlyPolicyBlockedNames: ReadonlySet<string>,
  explicitlyDisabledNames: ReadonlySet<string> = new Set(),
): { toBlock: string[]; toRestore: string[] } {
  const names = Object.keys(capturedConfigs)
  return {
    toBlock: names.filter(
      name =>
        !policyAllowedNames.has(name) &&
        !currentlyPolicyBlockedNames.has(name),
    ),
    toRestore: names.filter(
      name =>
        policyAllowedNames.has(name) &&
        currentlyPolicyBlockedNames.has(name) &&
        !explicitlyDisabledNames.has(name),
    ),
  }
}

export function canCommitDirectMcpOAuthGeneration(input: {
  sameGeneration: boolean
  expectedOwner: 'public' | 'plugin' | 'fixed'
  currentOwner: 'public' | 'plugin' | 'fixed' | null
  authorizedServerKey: string
  currentServerKey: string
  disabled: boolean
}): boolean {
  if (
    !input.sameGeneration ||
    input.disabled ||
    input.authorizedServerKey !== input.currentServerKey
  ) {
    return false
  }
  return (
    input.currentOwner === input.expectedOwner ||
    (input.expectedOwner === 'plugin' && input.currentOwner === null)
  )
}

export function prepareMcpServersForOwner(
  owner: McpProcessOwner,
  servers: Record<string, McpServerConfigForProcessTransport>,
  otherProcessOwnerNames: ReadonlySet<string>,
  protectedProcessOwnerNames: ReadonlySet<string> = new Set(),
): {
  reconciliationServers: Record<string, McpServerConfigForProcessTransport>
  errors: Record<string, string>
} {
  const processTransport = filterDirectTuiProcessMcpServers(servers)
  // Policy must run before namespace claims. Otherwise a denied earlier
  // config could shadow an allowed sibling and then be removed itself.
  const policy = filterMcpServersByPolicy(processTransport.accepted)
  const policyErrors = Object.fromEntries(
    policy.blocked.map(name => [
      name,
      'Blocked by enterprise policy (allowedMcpServers/deniedMcpServers)',
    ]),
  )
  const ownership = filterMcpServersForOwner(
    owner,
    policy.allowed,
    new Set([...otherProcessOwnerNames, ...protectedProcessOwnerNames]),
  )

  return {
    reconciliationServers: ownership.accepted,
    errors: {
      ...processTransport.errors,
      ...policyErrors,
      ...ownership.errors,
    },
  }
}
