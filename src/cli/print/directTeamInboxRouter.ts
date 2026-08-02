import {
  isModeSetRequest,
  isPermissionRequest,
  isPermissionResponse,
  isPlanApprovalRequest,
  isPlanApprovalResponse,
  isSandboxPermissionRequest,
  isSandboxPermissionResponse,
  isShutdownApproved,
  isShutdownRequest,
  isStructuredProtocolMessage,
  isTeamPermissionUpdate,
  type TeammateMessage,
} from '../../utils/teammateMailbox.js'
import { jsonParse } from '../../utils/slowOperations.js'

/**
 * Exact structured mailbox denominator from the fixed historical
 * `useInboxPoller` / `isStructuredProtocolMessage` implementation.
 *
 * `shutdown_rejected`, `idle_notification`, and task notifications are
 * deliberately absent: the fixed implementation delivers those as model
 * context instead of routing them through a control handler.
 */
export const DIRECT_TEAM_STRUCTURED_MESSAGE_TYPES = [
  'permission_request',
  'permission_response',
  'sandbox_permission_request',
  'sandbox_permission_response',
  'shutdown_request',
  'shutdown_approved',
  'team_permission_update',
  'mode_set_request',
  'plan_approval_request',
  'plan_approval_response',
] as const

export type DirectTeamStructuredMessageType =
  (typeof DIRECT_TEAM_STRUCTURED_MESSAGE_TYPES)[number]

export type ClassifiedDirectTeamInboxMessage =
  | {
      kind: 'regular'
      message: TeammateMessage
    }
  | {
      kind: 'structured'
      message: TeammateMessage
      type: DirectTeamStructuredMessageType
      parsed: unknown
    }
  | {
      kind: 'malformed_structured'
      message: TeammateMessage
      type: DirectTeamStructuredMessageType
    }

type StructuredRouteResult =
  | { kind: 'consumed' }
  | { kind: 'model_context' }
  | { kind: 'deferred'; reason: string }

export type DirectTeamStructuredHandler = (
  message: TeammateMessage,
  parsed: unknown,
) => StructuredRouteResult | Promise<StructuredRouteResult>

export type DirectTeamStructuredHandlers = Partial<
  Record<DirectTeamStructuredMessageType, DirectTeamStructuredHandler>
>

export type DeferredDirectTeamInboxMessage = {
  message: TeammateMessage
  type: DirectTeamStructuredMessageType
  reason: string
}

export type DirectTeamInboxRouteResult = {
  /**
   * Side-effect-only protocol messages that are safe to mark read now.
   * Model-context messages are not included until their caller has reliably
   * submitted or queued them.
   */
  consumed: TeammateMessage[]
  /**
   * Ordinary messages plus the small, explicit historical subset of
   * structured messages whose handler intentionally passes context onward.
   * Source order is preserved.
   */
  modelContext: TeammateMessage[]
  /**
   * Unsupported, malformed, or failed protocol messages. Callers must leave
   * these unread; they must never be silently converted to user prompts.
   */
  deferred: DeferredDirectTeamInboxMessage[]
}

const structuredTypeSet = new Set<string>(DIRECT_TEAM_STRUCTURED_MESSAGE_TYPES)

function rawStructuredType(
  messageText: string,
): DirectTeamStructuredMessageType | undefined {
  try {
    const parsed = jsonParse(messageText)
    if (!parsed || typeof parsed !== 'object' || !('type' in parsed)) {
      return undefined
    }
    const type = (parsed as { type?: unknown }).type
    return typeof type === 'string' && structuredTypeSet.has(type)
      ? (type as DirectTeamStructuredMessageType)
      : undefined
  } catch {
    return undefined
  }
}

export function classifyDirectTeamInboxMessage(
  message: TeammateMessage,
): ClassifiedDirectTeamInboxMessage {
  const classifiers: ReadonlyArray<
    readonly [
      DirectTeamStructuredMessageType,
      (text: string) => unknown | null,
    ]
  > = [
    ['permission_request', isPermissionRequest],
    ['permission_response', isPermissionResponse],
    ['sandbox_permission_request', isSandboxPermissionRequest],
    ['sandbox_permission_response', isSandboxPermissionResponse],
    ['shutdown_request', isShutdownRequest],
    ['shutdown_approved', isShutdownApproved],
    ['team_permission_update', isTeamPermissionUpdate],
    ['mode_set_request', isModeSetRequest],
    ['plan_approval_request', isPlanApprovalRequest],
    ['plan_approval_response', isPlanApprovalResponse],
  ]

  for (const [type, classifier] of classifiers) {
    const parsed = classifier(message.text)
    if (parsed) {
      return { kind: 'structured', message, type, parsed }
    }
  }

  // The historical helper recognizes a closed `type` before all individual
  // parsers validate their required fields. Such a message is protocol data,
  // even when malformed, and must fail closed instead of entering model input.
  if (isStructuredProtocolMessage(message.text)) {
    const type = rawStructuredType(message.text)
    if (type) {
      return { kind: 'malformed_structured', message, type }
    }
  }

  return { kind: 'regular', message }
}

export async function routeDirectTeamInboxMessages(
  messages: readonly TeammateMessage[],
  handlers: DirectTeamStructuredHandlers,
): Promise<DirectTeamInboxRouteResult> {
  const result: DirectTeamInboxRouteResult = {
    consumed: [],
    modelContext: [],
    deferred: [],
  }

  for (const message of messages) {
    const classified = classifyDirectTeamInboxMessage(message)
    if (classified.kind === 'regular') {
      result.modelContext.push(message)
      continue
    }
    if (classified.kind === 'malformed_structured') {
      result.deferred.push({
        message,
        type: classified.type,
        reason: 'structured mailbox message failed its fixed parser',
      })
      continue
    }

    const handler = handlers[classified.type]
    if (!handler) {
      result.deferred.push({
        message,
        type: classified.type,
        reason: 'no exact direct process-local handler is available',
      })
      continue
    }

    try {
      const disposition = await handler(message, classified.parsed)
      if (disposition.kind === 'consumed') {
        result.consumed.push(message)
      } else if (disposition.kind === 'model_context') {
        result.modelContext.push(message)
      } else {
        result.deferred.push({
          message,
          type: classified.type,
          reason: disposition.reason,
        })
      }
    } catch (error) {
      result.deferred.push({
        message,
        type: classified.type,
        reason: error instanceof Error ? error.message : String(error),
      })
    }
  }

  return result
}

export function directTeamInboxMessageIdentity(
  message: Pick<
    TeammateMessage,
    'from' | 'text' | 'timestamp' | 'color' | 'summary'
  >,
): string {
  return JSON.stringify([
    message.from,
    message.timestamp,
    message.text,
    message.color ?? null,
    message.summary ?? null,
  ])
}

/**
 * Build a one-shot, occurrence-aware read predicate.
 *
 * Mailbox messages have no persisted ID, so identity alone is insufficient:
 * two byte-for-byte equivalent unread responses can have different routing
 * outcomes when the first consumes a process-local callback and the second no
 * longer has one. Counting occurrences prevents a handled duplicate from
 * marking a deferred duplicate read.
 */
export function createDirectTeamInboxReadPredicate(
  messages: readonly Pick<
    TeammateMessage,
    'from' | 'text' | 'timestamp' | 'color' | 'summary'
  >[],
): (candidate: TeammateMessage) => boolean {
  const remainingByIdentity = new Map<string, number>()
  for (const message of messages) {
    const identity = directTeamInboxMessageIdentity(message)
    remainingByIdentity.set(
      identity,
      (remainingByIdentity.get(identity) ?? 0) + 1,
    )
  }

  return candidate => {
    const identity = directTeamInboxMessageIdentity(candidate)
    const remaining = remainingByIdentity.get(identity) ?? 0
    if (remaining === 0) return false
    if (remaining === 1) {
      remainingByIdentity.delete(identity)
    } else {
      remainingByIdentity.set(identity, remaining - 1)
    }
    return true
  }
}

/**
 * Remove only the number of incoming occurrences already represented in the
 * process-local queue. A Set would collapse legitimate repeated messages.
 */
export function excludeQueuedDirectTeamInboxOccurrences<
  T extends Pick<
    TeammateMessage,
    'from' | 'text' | 'timestamp' | 'color' | 'summary'
  >,
>(incoming: readonly T[], queued: readonly T[]): T[] {
  const queuedCounts = new Map<string, number>()
  for (const message of queued) {
    const identity = directTeamInboxMessageIdentity(message)
    queuedCounts.set(identity, (queuedCounts.get(identity) ?? 0) + 1)
  }

  return incoming.filter(message => {
    const identity = directTeamInboxMessageIdentity(message)
    const queuedCount = queuedCounts.get(identity) ?? 0
    if (queuedCount === 0) return true
    if (queuedCount === 1) {
      queuedCounts.delete(identity)
    } else {
      queuedCounts.set(identity, queuedCount - 1)
    }
    return false
  })
}
