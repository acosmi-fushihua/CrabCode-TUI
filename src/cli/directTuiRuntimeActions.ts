import { z } from 'zod/v4'

import { logForDebugging } from '../utils/debug.js'
import {
  DirectTuiBugReportActionSchema,
  DirectTuiBugReportResultSchema,
  handleDirectTuiBugReportAction,
  type DirectTuiBugReportAction,
  type DirectTuiBugReportDependencies,
} from './directTuiBugReportRuntimeAction.js'
import {
  directTuiModelManagementActionSchema,
  directTuiModelManagementResultSchema,
  handleDirectTuiModelManagementAction,
  type DirectTuiModelManagementAction,
} from './directTuiModelManagementActions.js'
import {
  DIRECT_TUI_RETAINED_COMMAND_ACTION_KINDS,
  DirectTuiRetainedCommandActionSchema,
  DirectTuiRetainedCommandResultSchema,
  handleDirectTuiRetainedCommandAction,
  type DirectTuiRetainedCommandAction,
  type DirectTuiRetainedCommandDependencies,
  type DirectTuiRetainedCommandSurface,
} from './directTuiRetainedCommandActions.js'
import {
  handleUsagePluginRuntimeAction,
  USAGE_PLUGIN_RUNTIME_ACTION_KINDS,
  UsagePluginRuntimeActionSchema,
  UsagePluginRuntimeResultSchema,
  type UsagePluginRuntimeAction,
} from './directTuiUsagePluginRuntimeActions.js'

/**
 * Process-private post-setup request/result transport between the native Rust
 * TUI and its same-generation direct TypeScript runtime.
 *
 * This is deliberately not part of the public SDK/control schemas. The
 * request and result unions are closed and every action has an explicit
 * discriminator. Do not add an arbitrary method name or opaque arguments
 * object here.
 */
export const CRABCODE_TUI_RUNTIME_ACTION_TYPE =
  'crabcode_tui_runtime_action' as const
export const CRABCODE_TUI_RUNTIME_RESULT_TYPE =
  'crabcode_tui_runtime_result' as const
export const CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION = 1 as const

const RuntimeRequestIdSchema = z
  .string()
  .min(1)
  .max(160)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))

export const DirectTuiHealthSnapshotActionSchema = z
  .object({
    kind: z.literal('health_snapshot'),
  })
  .strict()

export const DirectTuiRuntimeActionSchema = z.union([
  DirectTuiHealthSnapshotActionSchema,
  DirectTuiBugReportActionSchema,
  directTuiModelManagementActionSchema,
  UsagePluginRuntimeActionSchema,
  DirectTuiRetainedCommandActionSchema,
])

export const DirectTuiHealthSnapshotResultSchema = z
  .object({
    kind: z.literal('health_snapshot'),
    status: z.literal('ready'),
  })
  .strict()

export const DirectTuiRuntimeActionErrorSchema = z
  .object({
    kind: z.literal('runtime_action_error'),
    code: z.enum(['invalid_request', 'unknown_action', 'action_failed']),
  })
  .strict()

export const DirectTuiRuntimeResultSchema = z.union([
  DirectTuiHealthSnapshotResultSchema,
  DirectTuiBugReportResultSchema,
  directTuiModelManagementResultSchema,
  UsagePluginRuntimeResultSchema,
  DirectTuiRetainedCommandResultSchema,
  DirectTuiRuntimeActionErrorSchema,
])

export const DirectTuiRuntimeActionRequestSchema = z
  .object({
    type: z.literal(CRABCODE_TUI_RUNTIME_ACTION_TYPE),
    protocol_version: z.literal(CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION),
    request_id: RuntimeRequestIdSchema,
    action: DirectTuiRuntimeActionSchema,
  })
  .strict()

export const DirectTuiRuntimeActionResultSchema = z
  .object({
    type: z.literal(CRABCODE_TUI_RUNTIME_RESULT_TYPE),
    protocol_version: z.literal(CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION),
    request_id: RuntimeRequestIdSchema,
    result: DirectTuiRuntimeResultSchema,
  })
  .strict()

export type DirectTuiRuntimeAction = z.infer<
  typeof DirectTuiRuntimeActionSchema
>
export type DirectTuiRuntimeResult = z.infer<
  typeof DirectTuiRuntimeResultSchema
>
export type DirectTuiRuntimeActionRequest = z.infer<
  typeof DirectTuiRuntimeActionRequestSchema
>
export type DirectTuiRuntimeActionResult = z.infer<
  typeof DirectTuiRuntimeActionResultSchema
>

export type DirectTuiRuntimeActionRoute = {
  handled: boolean
  response?: DirectTuiRuntimeActionResult
  /**
   * A process-private action whose authority settles independently of stdin.
   * StructuredIO attaches this promise to its existing outbound FIFO instead
   * of awaiting it in the sole input reader.
   */
  backgroundSettlement?: Promise<DirectTuiRuntimeActionRoute>
}

export type DirectTuiRuntimeActionDependencies = {
  bugReportDependencies?: DirectTuiBugReportDependencies
  retainedCommandSurface?: DirectTuiRetainedCommandSurface
  retainedCommandDependencies?: DirectTuiRetainedCommandDependencies
}

export function createDirectTuiRuntimeActionRouter(
  dependencies: DirectTuiRuntimeActionDependencies,
): (value: unknown) => Promise<DirectTuiRuntimeActionRoute> {
  return async value => {
    const parsed = DirectTuiRuntimeActionRequestSchema.safeParse(value)
    if (
      parsed.success &&
      parsed.data.action.kind === 'bug_report_submit'
    ) {
      // Bug submission performs a real network request. Return ownership of
      // the private line immediately, then settle the exact same correlated
      // route in the background. This keeps StructuredIO's sole stdin reader
      // available for input, control responses, and cancellation while the
      // service is slow. The settled response still enters the one outbound
      // FIFO; no second writer or transport is introduced.
      return {
        handled: true,
        backgroundSettlement: routeDirectTuiRuntimeAction(value, dependencies),
      }
    }
    return routeDirectTuiRuntimeAction(value, dependencies)
  }
}

/**
 * Route one already-parsed stdin value. A valid-JSON private request is always
 * consumed here and never reaches the public SDK/control parser.
 *
 * Malformed requests with a usable request_id receive one correlated closed
 * error result. If correlation itself is malformed, the request is discarded
 * with a diagnostic: inventing an id would answer a request that never
 * existed. In both cases the normal StructuredIO session stays alive.
 */
export async function routeDirectTuiRuntimeAction(
  value: unknown,
  dependencies: DirectTuiRuntimeActionDependencies = {},
): Promise<DirectTuiRuntimeActionRoute> {
  if (!isRecord(value) || value.type !== CRABCODE_TUI_RUNTIME_ACTION_TYPE) {
    return { handled: false }
  }

  const requestId = RuntimeRequestIdSchema.safeParse(value.request_id)
  const parsed = DirectTuiRuntimeActionRequestSchema.safeParse(value)
  if (!parsed.success) {
    if (!requestId.success) {
      logForDebugging(
        '[direct-tui-runtime-action] discarded malformed private request without a usable request_id',
        { level: 'warn' },
      )
      return { handled: true }
    }
    return {
      handled: true,
      response: resultEnvelope(requestId.data, {
        kind: 'runtime_action_error',
        code: observedUnknownAction(value)
          ? 'unknown_action'
          : 'invalid_request',
      }),
    }
  }

  try {
    return {
      handled: true,
      response: resultEnvelope(
        parsed.data.request_id,
        await handleDirectTuiRuntimeAction(parsed.data.action, dependencies),
      ),
    }
  } catch (error) {
    logForDebugging(
      `[direct-tui-runtime-action] handler failed: ${error instanceof Error ? error.name : 'unknown'}`,
      { level: 'warn' },
    )
    return {
      handled: true,
      response: resultEnvelope(parsed.data.request_id, {
        kind: 'runtime_action_error',
        code: 'action_failed',
      }),
    }
  }
}

export function isDirectTuiRuntimeActionResult(
  value: unknown,
): value is DirectTuiRuntimeActionResult {
  return DirectTuiRuntimeActionResultSchema.safeParse(value).success
}

async function handleDirectTuiRuntimeAction(
  action: DirectTuiRuntimeAction,
  dependencies: DirectTuiRuntimeActionDependencies,
): Promise<DirectTuiRuntimeResult> {
  if (action.kind === 'health_snapshot') {
    // Reaching this branch proves that the direct-only post-setup reader and
    // closed dispatcher are alive. It reads and mutates no backend state.
    return { kind: 'health_snapshot', status: 'ready' }
  }

  if (isDirectTuiBugReportAction(action)) {
    return handleDirectTuiBugReportAction(
      action,
      dependencies.bugReportDependencies,
    )
  }

  if (isDirectTuiModelManagementAction(action)) {
    return handleDirectTuiModelManagementAction(action)
  }
  if (isDirectTuiRetainedCommandAction(action)) {
    return handleDirectTuiRetainedCommandAction(
      action,
      dependencies.retainedCommandSurface,
      dependencies.retainedCommandDependencies,
    )
  }
  return handleUsagePluginRuntimeAction(action)
}

function resultEnvelope(
  requestId: string,
  result: DirectTuiRuntimeResult,
): DirectTuiRuntimeActionResult {
  return DirectTuiRuntimeActionResultSchema.parse({
    type: CRABCODE_TUI_RUNTIME_RESULT_TYPE,
    protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
    request_id: requestId,
    result,
  })
}

function observedUnknownAction(value: Record<string, unknown>): boolean {
  return (
    isRecord(value.action) &&
    typeof value.action.kind === 'string' &&
    value.action.kind !== 'health_snapshot' &&
    value.action.kind !== 'bug_report_submit' &&
    !isDirectTuiModelManagementActionKind(value.action.kind) &&
    !isDirectTuiRetainedCommandActionKind(value.action.kind) &&
    !isUsagePluginRuntimeActionKind(value.action.kind)
  )
}

function isDirectTuiBugReportAction(
  action: DirectTuiRuntimeAction,
): action is DirectTuiBugReportAction {
  return action.kind === 'bug_report_submit'
}

const DIRECT_TUI_MODEL_MANAGEMENT_ACTION_KINDS = {
  'model.custom.list': true,
  'model.custom.add': true,
  'model.custom.update': true,
  'model.custom.remove': true,
  'model.custom.toggle': true,
  'model.custom.test_saved': true,
  'model.custom.test_draft': true,
  'model.local.snapshot': true,
  'model.local.download_start': true,
  'model.local.download_progress': true,
  'model.local.download_cancel': true,
  'model.local.install_remove': true,
  'model.local.server_start': true,
  'model.local.server_stop': true,
  'model.local.server_status': true,
  'model.local.byo_add': true,
  'model.local.byo_remove': true,
  'model.account.snapshot': true,
  'model.account.consent': true,
  'model.account.runtime_ensure': true,
  'model.account.runtime_stop': true,
  'model.account.login_start': true,
  'model.account.login_poll': true,
  'model.account.login_cancel': true,
  'model.account.remove': true,
} satisfies Record<DirectTuiModelManagementAction['kind'], true>

function isDirectTuiModelManagementActionKind(
  kind: string,
): kind is DirectTuiModelManagementAction['kind'] {
  return Object.hasOwn(DIRECT_TUI_MODEL_MANAGEMENT_ACTION_KINDS, kind)
}

function isDirectTuiModelManagementAction(
  action: DirectTuiRuntimeAction,
): action is DirectTuiModelManagementAction {
  return isDirectTuiModelManagementActionKind(action.kind)
}

function isDirectTuiRetainedCommandActionKind(
  kind: string,
): kind is DirectTuiRetainedCommandAction['kind'] {
  return (
    DIRECT_TUI_RETAINED_COMMAND_ACTION_KINDS as readonly string[]
  ).includes(kind)
}

function isDirectTuiRetainedCommandAction(
  action: DirectTuiRuntimeAction,
): action is DirectTuiRetainedCommandAction {
  return isDirectTuiRetainedCommandActionKind(action.kind)
}

function isUsagePluginRuntimeActionKind(
  kind: string,
): kind is UsagePluginRuntimeAction['kind'] {
  return (USAGE_PLUGIN_RUNTIME_ACTION_KINDS as readonly string[]).includes(
    kind,
  )
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}
