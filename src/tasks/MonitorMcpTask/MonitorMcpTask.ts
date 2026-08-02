// Stub for MonitorMcpTask (ANT-ONLY)
import type { TaskStateBase } from '../../Task.js'

export type MonitorMcpTaskState = TaskStateBase & {
  readonly type: 'monitor_mcp'
}

export function killMonitorMcp(_taskId: string, _setAppState?: unknown): void {}
export function killMonitorMcpTasksForAgent(_agentId: string, _getAppState?: unknown, _setAppState?: unknown): void {}
