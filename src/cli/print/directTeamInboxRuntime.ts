import { randomUUID } from 'crypto'

import type { SDKControlPermissionRequest } from '../../entrypoints/sdk/controlTypes.js'
import type { StructuredIO } from '../structuredIO.js'
import { SANDBOX_NETWORK_ACCESS_TOOL_NAME } from '../structuredIO.js'
import type { RequiresActionDetails } from '../../utils/sessionState.js'
import type { AppState } from '../../state/AppStateStore.js'
import { isInProcessTeammateTask } from '../../tasks/InProcessTeammateTask/types.js'
import {
  permissionModeFromString,
  toExternalPermissionMode,
} from '../../utils/permissions/PermissionMode.js'
import { applyPermissionUpdate } from '../../utils/permissions/PermissionUpdate.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import {
  ensureBackendsRegistered,
  getBackendByType,
} from '../../utils/swarm/backends/registry.js'
import type { PaneBackendType } from '../../utils/swarm/backends/types.js'
import { TEAM_LEAD_NAME } from '../../utils/swarm/constants.js'
import {
  hasPermissionCallback,
  hasSandboxPermissionCallback,
  processMailboxPermissionResponse,
  processSandboxPermissionResponse,
} from '../../utils/swarm/permissionCallbackRegistry.js'
import {
  sendPermissionResponseViaMailbox,
  sendSandboxPermissionResponseViaMailbox,
} from '../../utils/swarm/permissionSync.js'
import {
  removeTeammateFromTeamFile,
  setMemberMode,
} from '../../utils/swarm/teamHelpers.js'
import { isInsideTmux } from '../../utils/swarm/backends/detection.js'
import {
  findInProcessTeammateTaskId,
  handlePlanApprovalResponse,
} from '../../utils/inProcessTeammateHelpers.js'
import {
  getAgentName,
  getTeamName,
  isPlanModeRequired,
  isTeamLead,
  isTeammate,
} from '../../utils/teammate.js'
import { isInProcessTeammate } from '../../utils/teammateContext.js'
import {
  type ModeSetRequestMessage,
  type PermissionRequestMessage,
  type PermissionResponseMessage,
  type PlanApprovalRequestMessage,
  type PlanApprovalResponseMessage,
  type SandboxPermissionRequestMessage,
  type SandboxPermissionResponseMessage,
  type ShutdownApprovedMessage,
  type TeamPermissionUpdateMessage,
  writeToMailbox,
} from '../../utils/teammateMailbox.js'
import { unassignTeammateTasks } from '../../utils/tasks.js'
import { logForDebugging } from '../../utils/debug.js'
import type {
  DirectTeamStructuredHandlers,
  DirectTeamStructuredMessageType,
} from './directTeamInboxRouter.js'

type SetAppState = (update: (previous: AppState) => AppState) => void

export type DirectTeamInboxTarget = {
  role: 'leader' | 'teammate'
  agentName: string
  teamName: string | undefined
}

type DirectTeamInboxRuntimeOptions = {
  structuredIO: StructuredIO
  getAppState: () => AppState
  setAppState: SetAppState
  onPermissionPrompt: (details: RequiresActionDetails) => void
  onPermissionQueueSettled: () => void
  onWorkerPermissionNotification: (
    notification: DirectTeamWorkerPermissionNotification,
  ) => void
}

export type DirectTeamWorkerPermissionNotification = {
  message: string
  notificationType: 'worker_permission_prompt'
}

const MAX_SETTLED_REQUEST_KEYS = 1_000

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function validPermissionRequest(
  value: unknown,
): value is PermissionRequestMessage {
  if (!isRecord(value)) return false
  return (
    value.type === 'permission_request' &&
    typeof value.request_id === 'string' &&
    typeof value.agent_id === 'string' &&
    typeof value.tool_name === 'string' &&
    typeof value.tool_use_id === 'string' &&
    typeof value.description === 'string' &&
    isRecord(value.input) &&
    Array.isArray(value.permission_suggestions)
  )
}

function validPermissionResponse(
  value: unknown,
): value is PermissionResponseMessage {
  if (
    !isRecord(value) ||
    value.type !== 'permission_response' ||
    typeof value.request_id !== 'string'
  ) {
    return false
  }
  if (value.subtype === 'error') {
    return typeof value.error === 'string'
  }
  if (value.subtype !== 'success') return false
  if (value.response === undefined) return true
  if (!isRecord(value.response)) return false
  return (
    (value.response.updated_input === undefined ||
      isRecord(value.response.updated_input)) &&
    (value.response.permission_updates === undefined ||
      Array.isArray(value.response.permission_updates))
  )
}

function validSandboxPermissionRequest(
  value: unknown,
): value is SandboxPermissionRequestMessage {
  return (
    isRecord(value) &&
    value.type === 'sandbox_permission_request' &&
    typeof value.requestId === 'string' &&
    typeof value.workerId === 'string' &&
    typeof value.workerName === 'string' &&
    (value.workerColor === undefined || typeof value.workerColor === 'string') &&
    isRecord(value.hostPattern) &&
    typeof value.hostPattern.host === 'string' &&
    typeof value.createdAt === 'number' &&
    Number.isFinite(value.createdAt)
  )
}

function validSandboxPermissionResponse(
  value: unknown,
): value is SandboxPermissionResponseMessage {
  return (
    isRecord(value) &&
    value.type === 'sandbox_permission_response' &&
    typeof value.requestId === 'string' &&
    typeof value.host === 'string' &&
    typeof value.allow === 'boolean' &&
    typeof value.timestamp === 'string'
  )
}

function validTeamPermissionUpdate(
  value: unknown,
): value is TeamPermissionUpdateMessage {
  if (
    !isRecord(value) ||
    value.type !== 'team_permission_update' ||
    !isRecord(value.permissionUpdate) ||
    value.permissionUpdate.type !== 'addRules' ||
    !Array.isArray(value.permissionUpdate.rules) ||
    !['allow', 'deny', 'ask'].includes(
      String(value.permissionUpdate.behavior),
    ) ||
    value.permissionUpdate.destination !== 'session' ||
    typeof value.directoryPath !== 'string' ||
    typeof value.toolName !== 'string'
  ) {
    return false
  }
  return value.permissionUpdate.rules.every(
    rule =>
      isRecord(rule) &&
      typeof rule.toolName === 'string' &&
      (rule.ruleContent === undefined || typeof rule.ruleContent === 'string'),
  )
}

function requestKey(
  type: DirectTeamStructuredMessageType,
  sender: string,
  requestId: string,
): string {
  return `${type}\u0000${sender}\u0000${requestId}`
}

function rememberBounded(set: Set<string>, value: string): void {
  set.add(value)
  if (set.size <= MAX_SETTLED_REQUEST_KEYS) return
  const oldest = set.values().next().value
  if (oldest !== undefined) set.delete(oldest)
}

export function workerToolPermissionNotification(
  request: Pick<PermissionRequestMessage, 'agent_id' | 'tool_name'>,
): DirectTeamWorkerPermissionNotification {
  return {
    message: `${request.agent_id} needs permission for ${request.tool_name}`,
    notificationType: 'worker_permission_prompt',
  }
}

export function workerSandboxPermissionNotification(
  request: Pick<
    SandboxPermissionRequestMessage,
    'workerName' | 'hostPattern'
  >,
): DirectTeamWorkerPermissionNotification {
  return {
    message: `${request.workerName} needs network access to ${request.hostPattern.host}`,
    notificationType: 'worker_permission_prompt',
  }
}

export function directWorkerToolPermissionControlRequest(
  request: PermissionRequestMessage,
): SDKControlPermissionRequest {
  return {
    subtype: 'can_use_tool',
    tool_name: request.tool_name,
    input: request.input,
    permission_suggestions:
      request.permission_suggestions as SDKControlPermissionRequest['permission_suggestions'],
    tool_use_id: request.tool_use_id,
    agent_id: request.agent_id,
    description: request.description,
  }
}

export function directWorkerSandboxPermissionControlRequest(
  request: SandboxPermissionRequestMessage,
): SDKControlPermissionRequest {
  const host = request.hostPattern.host
  return {
    subtype: 'can_use_tool',
    tool_name: SANDBOX_NETWORK_ACCESS_TOOL_NAME,
    input: { host },
    tool_use_id: request.requestId,
    // Historical worker notification copy is keyed by workerName.
    // `agent_id` is existing can_use_tool presentation metadata here;
    // mailbox response authority remains request.workerName/requestId.
    agent_id: request.workerName,
    description: `Allow network connection to ${host}?`,
  }
}

export function resolveDirectTeamInboxTarget(
  appState: AppState,
): DirectTeamInboxTarget | undefined {
  // In-process teammates have their own mailbox loop and share the leader's
  // AppState. Polling here would read the wrong identity from shared state.
  if (isInProcessTeammate()) return undefined

  if (isTeammate()) {
    const agentName = getAgentName()
    if (!agentName) return undefined
    return {
      role: 'teammate',
      agentName,
      teamName: getTeamName(appState.teamContext),
    }
  }

  const teamContext = appState.teamContext
  if (!teamContext || !isTeamLead(teamContext)) return undefined
  const leadAgentId = teamContext.leadAgentId
  return {
    role: 'leader',
    agentName: teamContext.teammates[leadAgentId]?.name ?? TEAM_LEAD_NAME,
    teamName: teamContext.teamName,
  }
}

export class DirectTeamInboxRuntime {
  private readonly pendingPermissionRequests = new Set<string>()
  private readonly settledPermissionRequests = new Set<string>()
  private readonly pendingSandboxRequests = new Set<string>()
  private readonly settledSandboxRequests = new Set<string>()

  constructor(private readonly options: DirectTeamInboxRuntimeOptions) {}

  handlers(
    target: DirectTeamInboxTarget,
    workerNotificationGate: boolean,
  ): DirectTeamStructuredHandlers {
    let toolPermissionNotificationSent = false
    let sandboxPermissionNotificationSent = false
    return {
      permission_request: (message, parsed) => {
        if (target.role !== 'leader') {
          return {
            kind: 'deferred',
            reason: 'permission requests require the team-leader authority',
          }
        }
        if (!validPermissionRequest(parsed)) {
          return {
            kind: 'deferred',
            reason: 'permission request omitted required fixed fields',
          }
        }
        const key = requestKey(
          'permission_request',
          message.from,
          parsed.request_id,
        )
        if (
          this.pendingPermissionRequests.has(key) ||
          this.settledPermissionRequests.has(key)
        ) {
          return { kind: 'consumed' }
        }
        this.pendingPermissionRequests.add(key)
        let queued = false
        void this.resolvePermissionRequest(
          parsed,
          target.teamName,
          key,
          () => {
            queued = true
            if (
              workerNotificationGate &&
              !toolPermissionNotificationSent
            ) {
              toolPermissionNotificationSent = true
              this.options.onWorkerPermissionNotification(
                workerToolPermissionNotification(parsed),
              )
            }
          },
        )
        if (!queued) {
          this.pendingPermissionRequests.delete(key)
          return {
            kind: 'deferred',
            reason:
              'the existing can_use_tool reverse-control queue was unavailable',
          }
        }
        return { kind: 'consumed' }
      },
      permission_response: (_message, parsed) => {
        if (target.role !== 'teammate') {
          return {
            kind: 'deferred',
            reason: 'permission responses require a teammate callback',
          }
        }
        if (!validPermissionResponse(parsed)) {
          return {
            kind: 'deferred',
            reason: 'permission response omitted required fixed fields',
          }
        }
        if (!hasPermissionCallback(parsed.request_id)) {
          return {
            kind: 'deferred',
            reason: 'no matching process-local permission callback is registered',
          }
        }
        const processed =
          parsed.subtype === 'success'
            ? processMailboxPermissionResponse({
                requestId: parsed.request_id,
                decision: 'approved',
                updatedInput: parsed.response?.updated_input,
                permissionUpdates: parsed.response?.permission_updates,
              })
            : processMailboxPermissionResponse({
                requestId: parsed.request_id,
                decision: 'rejected',
                feedback: parsed.error,
              })
        return processed
          ? { kind: 'consumed' }
          : {
              kind: 'deferred',
              reason: 'the process-local permission callback rejected the response',
            }
      },
      sandbox_permission_request: (message, parsed) => {
        if (target.role !== 'leader') {
          return {
            kind: 'deferred',
            reason:
              'sandbox permission requests require the team-leader authority',
          }
        }
        if (!validSandboxPermissionRequest(parsed)) {
          return {
            kind: 'deferred',
            reason: 'sandbox permission request omitted required fixed fields',
          }
        }
        const key = requestKey(
          'sandbox_permission_request',
          message.from,
          parsed.requestId,
        )
        if (
          this.pendingSandboxRequests.has(key) ||
          this.settledSandboxRequests.has(key)
        ) {
          return { kind: 'consumed' }
        }
        this.pendingSandboxRequests.add(key)
        let queued = false
        void this.resolveSandboxPermissionRequest(
          parsed,
          target.teamName,
          key,
          () => {
            queued = true
            if (
              workerNotificationGate &&
              !sandboxPermissionNotificationSent
            ) {
              sandboxPermissionNotificationSent = true
              this.options.onWorkerPermissionNotification(
                workerSandboxPermissionNotification(parsed),
              )
            }
          },
        )
        if (!queued) {
          this.pendingSandboxRequests.delete(key)
          return {
            kind: 'deferred',
            reason:
              'the existing can_use_tool reverse-control queue was unavailable',
          }
        }
        return { kind: 'consumed' }
      },
      sandbox_permission_response: (_message, parsed) => {
        if (target.role !== 'teammate') {
          return {
            kind: 'deferred',
            reason: 'sandbox responses require a teammate callback',
          }
        }
        if (!validSandboxPermissionResponse(parsed)) {
          return {
            kind: 'deferred',
            reason: 'sandbox permission response omitted required fixed fields',
          }
        }
        if (!hasSandboxPermissionCallback(parsed.requestId)) {
          return {
            kind: 'deferred',
            reason:
              'no matching process-local sandbox permission callback is registered',
          }
        }
        return processSandboxPermissionResponse({
          requestId: parsed.requestId,
          host: parsed.host,
          allow: parsed.allow,
        })
          ? this.clearPendingSandboxRequest()
          : {
              kind: 'deferred',
              reason:
                'the process-local sandbox permission callback rejected the response',
            }
      },
      shutdown_request: () =>
        target.role === 'teammate'
          ? { kind: 'model_context' }
          : {
              kind: 'deferred',
              reason: 'shutdown requests are only actionable by teammates',
            },
      shutdown_approved: async (_message, parsed) => {
        if (target.role !== 'leader') {
          return {
            kind: 'deferred',
            reason: 'shutdown approvals require the team-leader authority',
          }
        }
        await this.handleShutdownApproval(
          parsed as ShutdownApprovedMessage,
          target.teamName,
        )
        return { kind: 'model_context' }
      },
      team_permission_update: (_message, parsed) => {
        if (target.role !== 'teammate') {
          return {
            kind: 'deferred',
            reason: 'team permission updates are only actionable by teammates',
          }
        }
        if (!validTeamPermissionUpdate(parsed)) {
          return {
            kind: 'deferred',
            reason: 'team permission update omitted required fixed fields',
          }
        }
        this.options.setAppState(previous => ({
          ...previous,
          toolPermissionContext: applyPermissionUpdate(
            previous.toolPermissionContext,
            {
              type: 'addRules',
              rules: parsed.permissionUpdate.rules,
              behavior: parsed.permissionUpdate.behavior,
              destination: 'session',
            },
          ),
        }))
        return { kind: 'consumed' }
      },
      mode_set_request: (message, parsed) => {
        if (target.role !== 'teammate') {
          return {
            kind: 'deferred',
            reason: 'mode-set requests are only actionable by teammates',
          }
        }
        const request = parsed as ModeSetRequestMessage
        // The fixed historical poller consumes but ignores forged mode
        // changes. Trust is checked against the mailbox envelope, not a JSON
        // field that the sender controls.
        if (message.from !== TEAM_LEAD_NAME) {
          return { kind: 'consumed' }
        }
        const targetMode = permissionModeFromString(request.mode)
        this.options.setAppState(previous => ({
          ...previous,
          toolPermissionContext: applyPermissionUpdate(
            previous.toolPermissionContext,
            {
              type: 'setMode',
              mode: toExternalPermissionMode(targetMode),
              destination: 'session',
            },
          ),
        }))
        if (target.teamName) {
          setMemberMode(target.teamName, target.agentName, targetMode)
        }
        return { kind: 'consumed' }
      },
      plan_approval_request: async (message, parsed) => {
        if (target.role !== 'leader') {
          return {
            kind: 'deferred',
            reason: 'plan approval requests require the team-leader authority',
          }
        }
        await this.approvePlanRequest(
          message.from,
          parsed as PlanApprovalRequestMessage,
          target.teamName,
        )
        return { kind: 'model_context' }
      },
      plan_approval_response: (message, parsed) => {
        if (target.role === 'teammate') {
          this.applyPlanApprovalResponse(
            message.from,
            parsed as PlanApprovalResponseMessage,
          )
        }
        // Fixed history intentionally passes the already-routed approval
        // response onward so the model sees approval/rejection context.
        return { kind: 'model_context' }
      },
    }
  }

  private async resolvePermissionRequest(
    request: PermissionRequestMessage,
    teamName: string | undefined,
    key: string,
    onQueued: () => void,
  ): Promise<void> {
    let queued = false
    try {
      const result = await this.options.structuredIO.requestDirectPermission(
        directWorkerToolPermissionControlRequest(request),
        this.options.onPermissionPrompt,
        () => {
          queued = true
          onQueued()
        },
      )
      if (result.behavior === 'allow') {
        await sendPermissionResponseViaMailbox(
          request.agent_id,
          {
            decision: 'approved',
            resolvedBy: 'leader',
            updatedInput: result.updatedInput,
            permissionUpdates: result.updatedPermissions,
          },
          request.request_id,
          teamName,
        )
      } else {
        await sendPermissionResponseViaMailbox(
          request.agent_id,
          {
            decision: 'rejected',
            resolvedBy: 'leader',
            feedback: result.message,
          },
          request.request_id,
          teamName,
        )
      }
    } catch (error) {
      if (queued) {
        logForDebugging(
          `[DirectTeamInbox] permission request ${request.request_id} failed: ${error}`,
          { level: 'warn' },
        )
      }
    } finally {
      this.pendingPermissionRequests.delete(key)
      if (queued) {
        rememberBounded(this.settledPermissionRequests, key)
        try {
          this.options.onPermissionQueueSettled()
        } catch (error) {
          logForDebugging(
            `[DirectTeamInbox] permission settled observer failed: ${error}`,
            { level: 'warn' },
          )
        }
      }
    }
  }

  private clearPendingSandboxRequest(): { kind: 'consumed' } {
    this.options.setAppState(previous =>
      previous.pendingSandboxRequest === null
        ? previous
        : { ...previous, pendingSandboxRequest: null },
    )
    return { kind: 'consumed' }
  }

  private async resolveSandboxPermissionRequest(
    request: SandboxPermissionRequestMessage,
    teamName: string | undefined,
    key: string,
    onQueued: () => void,
  ): Promise<void> {
    const host = request.hostPattern.host
    let queued = false
    try {
      const result = await this.options.structuredIO.requestDirectPermission(
        directWorkerSandboxPermissionControlRequest(request),
        this.options.onPermissionPrompt,
        () => {
          queued = true
          onQueued()
        },
      )
      await sendSandboxPermissionResponseViaMailbox(
        request.workerName,
        request.requestId,
        host,
        result.behavior === 'allow',
        teamName,
      )
    } catch (error) {
      if (queued) {
        logForDebugging(
          `[DirectTeamInbox] sandbox request ${request.requestId} failed: ${error}`,
          { level: 'warn' },
        )
      }
    } finally {
      this.pendingSandboxRequests.delete(key)
      if (queued) {
        rememberBounded(this.settledSandboxRequests, key)
        try {
          this.options.onPermissionQueueSettled()
        } catch (error) {
          logForDebugging(
            `[DirectTeamInbox] sandbox settled observer failed: ${error}`,
            { level: 'warn' },
          )
        }
      }
    }
  }

  private async approvePlanRequest(
    sender: string,
    request: PlanApprovalRequestMessage,
    teamName: string | undefined,
  ): Promise<void> {
    const current = this.options.getAppState()
    const leaderMode = toExternalPermissionMode(
      current.toolPermissionContext.mode,
    )
    const permissionMode = leaderMode === 'plan' ? 'default' : leaderMode
    const response: PlanApprovalResponseMessage = {
      type: 'plan_approval_response',
      requestId: request.requestId,
      approved: true,
      timestamp: new Date().toISOString(),
      permissionMode,
    }
    await writeToMailbox(
      sender,
      {
        from: TEAM_LEAD_NAME,
        text: jsonStringify(response),
        timestamp: new Date().toISOString(),
      },
      teamName,
    )
    const taskId = findInProcessTeammateTaskId(sender, current)
    if (taskId) {
      handlePlanApprovalResponse(taskId, response, this.options.setAppState)
    }
  }

  private applyPlanApprovalResponse(
    sender: string,
    response: PlanApprovalResponseMessage,
  ): void {
    if (!isPlanModeRequired() || sender !== TEAM_LEAD_NAME) return
    if (!response.approved) return
    const targetMode = response.permissionMode ?? 'default'
    this.options.setAppState(previous => ({
      ...previous,
      toolPermissionContext: applyPermissionUpdate(
        previous.toolPermissionContext,
        {
          type: 'setMode',
          mode: toExternalPermissionMode(targetMode),
          destination: 'session',
        },
      ),
    }))
  }

  private async handleShutdownApproval(
    approval: ShutdownApprovedMessage,
    teamName: string | undefined,
  ): Promise<void> {
    if (approval.paneId && approval.backendType) {
      try {
        await ensureBackendsRegistered()
        const backend = getBackendByType(
          approval.backendType as PaneBackendType,
        )
        await backend?.killPane(approval.paneId, !(await isInsideTmux()))
      } catch (error) {
        logForDebugging(
          `[DirectTeamInbox] failed to kill pane for ${approval.from}: ${error}`,
          { level: 'warn' },
        )
      }
    }

    const current = this.options.getAppState()
    const teammateId = current.teamContext?.teammates
      ? Object.entries(current.teamContext.teammates).find(
          ([, teammate]) => teammate.name === approval.from,
        )?.[0]
      : undefined
    if (!teammateId) return

    if (teamName) {
      removeTeammateFromTeamFile(teamName, {
        agentId: teammateId,
        name: approval.from,
      })
    }
    const { notificationMessage } = teamName
      ? await unassignTeammateTasks(
          teamName,
          teammateId,
          approval.from,
          'shutdown',
        )
      : { notificationMessage: `${approval.from} has shut down.` }

    this.options.setAppState(previous => {
      if (
        !previous.teamContext?.teammates ||
        !(teammateId in previous.teamContext.teammates)
      ) {
        return previous
      }
      const { [teammateId]: _, ...remainingTeammates } =
        previous.teamContext.teammates
      const tasks = { ...previous.tasks }
      for (const [taskId, task] of Object.entries(tasks)) {
        if (
          isInProcessTeammateTask(task) &&
          task.identity.agentId === teammateId
        ) {
          tasks[taskId] = {
            ...task,
            status: 'completed',
            endTime: Date.now(),
          }
        }
      }
      return {
        ...previous,
        tasks,
        teamContext: {
          ...previous.teamContext,
          teammates: remainingTeammates,
        },
        inbox: {
          messages: [
            ...previous.inbox.messages,
            {
              id: randomUUID(),
              from: 'system',
              text: jsonStringify({
                type: 'teammate_terminated',
                message: notificationMessage,
              }),
              timestamp: new Date().toISOString(),
              status: 'pending',
            },
          ],
        },
      }
    })
  }
}
