import type { McpServerStatus } from '../../entrypoints/sdk/coreTypes.js'
import type { PluginMcpInventoryRecord } from '../../services/mcp/pluginMcpLifecycle.js'
import type { ScopedMcpServerConfig } from '../../services/mcp/types.js'

export type SdkMcpLifecycleStatus = NonNullable<
  McpServerStatus['lifecycleStatus']
>

export function sdkMcpLifecycleStatusFromReason(
  reasonCode: PluginMcpInventoryRecord['reasonCode'],
): SdkMcpLifecycleStatus {
  switch (reasonCode) {
    case 'plugin-disabled':
      return 'pluginDisabled'
    case 'requires-config':
      return 'requiresConfig'
    case 'requires-dependency':
      return 'requiresDependency'
    case 'invalid-config':
      return 'invalidConfig'
    case 'requires-login':
      return 'requiresLogin'
    case 'required-not-local':
      return 'requiredNotLocal'
    case 'inactive':
      return 'inactive'
    case 'ready':
      return 'ready'
  }
}

function statusConfig(
  config: ScopedMcpServerConfig | undefined,
): McpServerStatus['config'] {
  if (!config) return undefined
  if (config.type === 'sse' || config.type === 'http') {
    return {
      type: config.type,
      url: config.url,
      headers: config.headers,
    }
  }
  if (config.type === 'acosmi-proxy') {
    return { type: 'acosmi-proxy', url: config.url, id: config.id }
  }
  if (config.type === 'stdio' || config.type === undefined) {
    return {
      type: 'stdio',
      command: config.command,
      args: config.args,
    }
  }
  if (config.type === 'sdk') {
    return { type: 'sdk', name: config.name }
  }
  return undefined
}

/** Project lifecycle-only plugin rows into the SDK management read face. */
export function buildPluginMcpManagementStatuses(
  inventory: readonly PluginMcpInventoryRecord[],
  liveNames: ReadonlySet<string>,
): McpServerStatus[] {
  return inventory
    .filter(record => !liveNames.has(record.runtimeName))
    .map(record => ({
      name: record.runtimeName,
      status:
        record.reasonCode === 'requires-login' ? 'needs-auth' : 'disabled',
      config: statusConfig(record.config),
      scope: record.config?.scope,
      lifecycleStatus: sdkMcpLifecycleStatusFromReason(record.reasonCode),
      ...(record.reason ? { lifecycleReason: record.reason } : {}),
    }))
}
