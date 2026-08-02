import { logForDebugging } from '../debug.js'
import {
  type PermissionUpdate,
  permissionUpdateSchema,
} from '../permissions/PermissionUpdateSchema.js'
import type { PermissionResponse } from './permissionSync.js'

function parsePermissionUpdates(raw: unknown): PermissionUpdate[] {
  if (!Array.isArray(raw)) return []
  const schema = permissionUpdateSchema()
  const valid: PermissionUpdate[] = []
  for (const entry of raw) {
    const result = schema.safeParse(entry)
    if (result.success) {
      valid.push(result.data)
    } else {
      logForDebugging(
        `[SwarmPermissionPoller] Dropping malformed permissionUpdate entry: ${result.error.message}`,
        { level: 'warn' },
      )
    }
  }
  return valid
}

export type PermissionResponseCallback = {
  requestId: string
  toolUseId: string
  onAllow: (
    updatedInput: Record<string, unknown> | undefined,
    permissionUpdates: PermissionUpdate[],
    feedback?: string,
  ) => void
  onReject: (feedback?: string) => void
}

const pendingCallbacks = new Map<string, PermissionResponseCallback>()

export function registerPermissionCallback(
  callback: PermissionResponseCallback,
): void {
  pendingCallbacks.set(callback.requestId, callback)
  logForDebugging(
    `[SwarmPermissionPoller] Registered callback for request ${callback.requestId}`,
  )
}

export function unregisterPermissionCallback(requestId: string): void {
  pendingCallbacks.delete(requestId)
  logForDebugging(
    `[SwarmPermissionPoller] Unregistered callback for request ${requestId}`,
  )
}

export function hasPermissionCallback(requestId: string): boolean {
  return pendingCallbacks.has(requestId)
}

export function getPendingPermissionRequestIds(): string[] {
  return [...pendingCallbacks.keys()]
}

export function clearAllPendingCallbacks(): void {
  pendingCallbacks.clear()
  pendingSandboxCallbacks.clear()
}

export function processMailboxPermissionResponse(params: {
  requestId: string
  decision: 'approved' | 'rejected'
  feedback?: string
  updatedInput?: Record<string, unknown>
  permissionUpdates?: unknown
}): boolean {
  return processPermissionResponse(params, 'mailbox')
}

export function processPolledPermissionResponse(
  response: PermissionResponse,
): boolean {
  return processPermissionResponse(response, 'poll')
}

function processPermissionResponse(
  response: {
    requestId: string
    decision: 'approved' | 'rejected' | 'denied'
    feedback?: string
    updatedInput?: Record<string, unknown>
    permissionUpdates?: unknown
  },
  source: 'mailbox' | 'poll',
): boolean {
  const callback = pendingCallbacks.get(response.requestId)
  if (!callback) {
    logForDebugging(
      `[SwarmPermissionPoller] No callback registered for ${source} response ${response.requestId}`,
    )
    return false
  }
  logForDebugging(
    `[SwarmPermissionPoller] Processing ${source} response for request ${response.requestId}: ${response.decision}`,
  )
  pendingCallbacks.delete(response.requestId)
  if (response.decision === 'approved') {
    callback.onAllow(
      response.updatedInput,
      parsePermissionUpdates(response.permissionUpdates),
    )
  } else {
    callback.onReject(response.feedback)
  }
  return true
}

export type SandboxPermissionResponseCallback = {
  requestId: string
  host: string
  resolve: (allow: boolean) => void
}

const pendingSandboxCallbacks = new Map<
  string,
  SandboxPermissionResponseCallback
>()

export function registerSandboxPermissionCallback(
  callback: SandboxPermissionResponseCallback,
): void {
  pendingSandboxCallbacks.set(callback.requestId, callback)
  logForDebugging(
    `[SwarmPermissionPoller] Registered sandbox callback for request ${callback.requestId}`,
  )
}

export function hasSandboxPermissionCallback(requestId: string): boolean {
  return pendingSandboxCallbacks.has(requestId)
}

export function processSandboxPermissionResponse(params: {
  requestId: string
  host: string
  allow: boolean
}): boolean {
  const callback = pendingSandboxCallbacks.get(params.requestId)
  if (!callback) {
    logForDebugging(
      `[SwarmPermissionPoller] No sandbox callback registered for request ${params.requestId}`,
    )
    return false
  }
  logForDebugging(
    `[SwarmPermissionPoller] Processing sandbox response for request ${params.requestId}: allow=${params.allow}`,
  )
  pendingSandboxCallbacks.delete(params.requestId)
  callback.resolve(params.allow)
  return true
}
