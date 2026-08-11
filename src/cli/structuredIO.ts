import { feature } from '../utils/featurePolyfill.js'
import type {
  ElicitResult,
  JSONRPCMessage,
} from '@modelcontextprotocol/sdk/types.js'
import { randomUUID } from 'crypto'
import type { AssistantMessage } from 'src//types/message.js'
import type {
  HookInput,
  HookJSONOutput,
  PermissionUpdate,
  SDKMessage,
  SDKUserMessage,
} from 'src/entrypoints/agentSdkTypes.js'
import {
  SDKControlElicitationResponseSchema,
  SDK_PERMISSION_DECISION_REASON_CODES,
} from 'src/entrypoints/sdk/controlSchemas.js'
import type {
  SDKControlRequest,
  SDKControlResponse,
  SDKControlPermissionRequest,
  StdinMessage,
  StdoutMessage,
} from 'src/entrypoints/sdk/controlTypes.js'
import type { CanUseToolFn } from 'src/types/canUseTool.js'
import type { Tool, ToolUseContext } from 'src/Tool.js'
import { ASK_USER_QUESTION_TOOL_NAME } from 'src/tools/AskUserQuestionTool/prompt.js'
import { type HookCallback, hookJSONOutputSchema } from 'src/types/hooks.js'
import { logForDebugging } from 'src/utils/debug.js'
import { logForDiagnosticsNoPII } from 'src/utils/diagLogs.js'
import { AbortError } from 'src/utils/errors.js'
import {
  type Output as PermissionToolOutput,
  permissionPromptToolResultToPermissionDecision,
  outputSchema as permissionToolOutputSchema,
} from 'src/utils/permissions/PermissionPromptToolResultSchema.js'
import type {
  PermissionDecision,
  PermissionDecisionReason,
} from 'src/utils/permissions/PermissionResult.js'
import {
  createPermissionRequestMessage,
  hasPermissionsToUseTool,
} from 'src/utils/permissions/permissions.js'
import { writeToStdout } from 'src/utils/process.js'
import { jsonStringify } from 'src/utils/slowOperations.js'
import { z } from 'zod/v4'
import { notifyCommandLifecycle } from '../utils/commandLifecycle.js'
import { executePermissionRequestHooks } from '../utils/hooks.js'
import {
  applyPermissionUpdates,
  persistPermissionUpdates,
} from '../utils/permissions/PermissionUpdate.js'
import {
  notifySessionStateChanged,
  type RequiresActionDetails,
} from '../utils/sessionState.js'
import { jsonParse } from '../utils/slowOperations.js'
import { Stream } from '../utils/stream.js'
import type { DirectTuiRendererEvent } from './directTuiQueryEvents.js'
import {
  type DirectTuiRuntimeActionResult,
  type DirectTuiRuntimeActionRoute,
} from './directTuiRuntimeActions.js'
import { ndjsonSafeStringify } from './ndjsonSafeStringify.js'
import {
  buildDirectTuiCommandCatalogChangedRequest,
  DirectTuiCommandCatalogChangedResponseSchema,
  type DirectTuiCommandCatalogChangedResponse,
} from './directTuiCommandCatalogRefresh.js'
import type { CommandCatalogEntry } from './commandCatalogProjection.js'

export type StructuredIoOutboundMessage =
  | StdoutMessage
  | DirectTuiRendererEvent
  | DirectTuiRuntimeActionResult

export type DirectTuiRuntimeActionRouter = (
  value: unknown,
) => Promise<DirectTuiRuntimeActionRoute>

/**
 * Synthetic tool name used when forwarding sandbox network permission
 * requests via the can_use_tool control_request protocol. SDK hosts
 * see this as a normal tool permission prompt.
 */
export const SANDBOX_NETWORK_ACCESS_TOOL_NAME = 'SandboxNetworkAccess'

function serializeDecisionReason(
  reason: PermissionDecisionReason | undefined,
): string | undefined {
  if (!reason) {
    return undefined
  }

  if (
    (feature('BASH_CLASSIFIER') || feature('TRANSCRIPT_CLASSIFIER')) &&
    reason.type === 'classifier'
  ) {
    return reason.reason
  }
  switch (reason.type) {
    case 'rule':
    case 'mode':
    case 'subcommandResults':
    case 'permissionPromptTool':
      return undefined
    case 'hook':
    case 'asyncAgent':
    case 'workingDir':
    case 'safetyCheck':
    case 'other':
      return reason.reason
  }
}

type PermissionDecisionReasonCode =
  (typeof SDK_PERMISSION_DECISION_REASON_CODES)[number]

const PERMISSION_DECISION_REASON_CODE_BY_TYPE = {
  rule: 'rule',
  mode: 'mode',
  subcommandResults: 'subcommand_results',
  permissionPromptTool: 'permission_prompt_tool',
  hook: 'hook',
  asyncAgent: 'async_agent',
  classifier: 'classifier',
  workingDir: 'working_directory',
  safetyCheck: 'safety_check',
  other: 'other',
} as const satisfies Record<
  PermissionDecisionReason['type'],
  PermissionDecisionReasonCode
>

export function permissionDecisionReasonCode(
  reason: PermissionDecisionReason | undefined,
): PermissionDecisionReasonCode | undefined {
  return reason
    ? PERMISSION_DECISION_REASON_CODE_BY_TYPE[reason.type]
    : undefined
}

export function isPermissionRequestHookAllowAuthoritative(
  toolName: string,
): boolean {
  return toolName !== ASK_USER_QUESTION_TOOL_NAME
}

export function buildCanUseToolControlRequest(
  tool: Pick<Tool, 'name'>,
  input: Record<string, unknown>,
  permissionResult: Extract<PermissionDecision, { behavior: 'ask' }>,
  toolUseID: string,
  agentID?: string,
): SDKControlPermissionRequest {
  const explicitMessage = permissionResult.message?.trim()
  let policyExplanation: string | undefined
  try {
    policyExplanation = createPermissionRequestMessage(
      tool.name,
      permissionResult.decisionReason,
    )
  } catch {
    // Presentation formatting must never break the permission bridge. The
    // decision's own message is already safe to use as the fallback copy.
  }
  const description =
    explicitMessage &&
    policyExplanation &&
    explicitMessage !== policyExplanation
      ? `${explicitMessage}\n${policyExplanation}`
      : (explicitMessage ?? policyExplanation)
  return {
    subtype: 'can_use_tool',
    tool_name: tool.name,
    input,
    permission_suggestions: permissionResult.suggestions,
    blocked_path: permissionResult.blockedPath,
    decision_reason_code: permissionDecisionReasonCode(
      permissionResult.decisionReason,
    ),
    // Kept for compatibility with existing SDK hosts that consume the
    // free-form reason. New consumers should branch on decision_reason_code.
    decision_reason: serializeDecisionReason(permissionResult.decisionReason),
    // Carry both the concrete tool message and the stable policy explanation
    // when they differ. This keeps protected-path context visible without
    // reducing the reason code to presentation copy.
    description,
    tool_use_id: toolUseID,
    agent_id: agentID,
  }
}

function buildRequiresActionDetails(
  tool: Tool,
  input: Record<string, unknown>,
  toolUseID: string,
  requestId: string,
): RequiresActionDetails {
  // Per-tool summary methods may throw on malformed input; permission
  // handling must not break because of a bad description.
  let description: string
  try {
    description =
      tool.getActivityDescription?.(input) ??
      tool.getToolUseSummary?.(input) ??
      tool.userFacingName(input)
  } catch {
    description = tool.name
  }
  return {
    tool_name: tool.name,
    action_description: description,
    tool_use_id: toolUseID,
    request_id: requestId,
    input,
  }
}

type PendingRequest<T> = {
  resolve: (result: T) => void
  reject: (error: unknown) => void
  schema?: z.Schema
  request: SDKControlRequest
}

/**
 * Provides a structured way to read and write SDK messages from stdio,
 * capturing the SDK protocol.
 */
// Maximum number of resolved correlations to track. Once exceeded, the oldest
// entry is evicted. This bounds memory in very long sessions while keeping
// enough history to catch duplicate control_response deliveries.
const MAX_RESOLVED_CONTROL_REQUESTS = 1000

export class StructuredIO {
  readonly structuredInput: AsyncGenerator<StdinMessage | SDKMessage>
  private readonly pendingRequests = new Map<string, PendingRequest<unknown>>()

  private inputClosed = false
  private unexpectedResponseCallback?: (
    response: SDKControlResponse,
  ) => Promise<void>

  // Tracks tool_use IDs that have been resolved through the normal permission
  // flow (or aborted by a hook). When a duplicate control_response arrives
  // after the original was already handled, this Set prevents the orphan
  // handler from re-processing it — which would push duplicate assistant
  // messages into mutableMessages and cause a 400 "tool_use ids must be unique"
  // error from the API.
  private readonly resolvedToolUseIds = new Set<string>()
  private readonly resolvedControlRequestIds = new Set<string>()
  private prependedLines: string[] = []
  private onControlRequestSent?: (request: SDKControlRequest) => void
  private onControlRequestResolved?: (requestId: string) => void
  private directTuiRuntimeActionRouter?: DirectTuiRuntimeActionRouter

  // sendRequest() and print.ts both enqueue here; the drain loop is the
  // only writer. Prevents control_request from overtaking queued stream_events.
  readonly outbound = new Stream<StructuredIoOutboundMessage>()

  constructor(
    private readonly input: AsyncIterable<string>,
    private readonly replayUserMessages?: boolean,
  ) {
    this.input = input
    this.structuredInput = this.read()
  }

  /** Records both exact request and tool-use correlations for late responses. */
  private trackResolvedControlRequest(request: SDKControlRequest): void {
    this.resolvedControlRequestIds.add(request.request_id)
    if (this.resolvedControlRequestIds.size > MAX_RESOLVED_CONTROL_REQUESTS) {
      const first = this.resolvedControlRequestIds.values().next().value
      if (first !== undefined) {
        this.resolvedControlRequestIds.delete(first)
      }
    }
    const payload = request.request
    if (payload.subtype === 'can_use_tool') {
      this.resolvedToolUseIds.add(payload.tool_use_id)
      if (this.resolvedToolUseIds.size > MAX_RESOLVED_CONTROL_REQUESTS) {
        // Evict the oldest entry (Sets iterate in insertion order)
        const first = this.resolvedToolUseIds.values().next().value
        if (first !== undefined) {
          this.resolvedToolUseIds.delete(first)
        }
      }
    }
  }

  /**
   * Queue a user turn to be yielded before the next message from this.input.
   * Works before iteration starts and mid-stream — read() re-checks
   * prependedLines between each yielded message.
   */
  prependUserMessage(content: string): void {
    this.prependedLines.push(
      jsonStringify({
        type: 'user',
        session_id: '',
        message: { role: 'user', content },
        parent_tool_use_id: null,
      } satisfies SDKUserMessage) + '\n',
    )
  }

  private async *read() {
    let content = ''

    // Called once before for-await (an empty this.input otherwise skips the
    // loop body entirely), then again per block. prependedLines re-check is
    // inside the while so a prepend pushed between two messages in the SAME
    // block still lands first.
    const splitAndProcess = async function* (this: StructuredIO) {
      for (;;) {
        if (this.prependedLines.length > 0) {
          content = this.prependedLines.join('') + content
          this.prependedLines = []
        }
        const newline = content.indexOf('\n')
        if (newline === -1) break
        const line = content.slice(0, newline)
        content = content.slice(newline + 1)
        const message = await this.processLine(line)
        if (message) {
          logForDiagnosticsNoPII('info', 'cli_stdin_message_parsed', {
            type: message.type,
          })
          yield message
        }
      }
    }.bind(this)

    yield* splitAndProcess()

    for await (const block of this.input) {
      content += block
      yield* splitAndProcess()
    }
    if (content) {
      const message = await this.processLine(content)
      if (message) {
        yield message
      }
    }
    this.inputClosed = true
    for (const request of this.pendingRequests.values()) {
      // Reject all pending requests if the input stream
      request.reject(
        new Error('Tool permission stream closed before response received'),
      )
    }
  }

  getPendingPermissionRequests(): SDKControlRequest[] {
    return Array.from(this.pendingRequests.values())
      .map(entry => entry.request)
      .filter(
        (request): request is SDKControlRequest =>
          request.request.subtype === 'can_use_tool',
      )
  }

  setUnexpectedResponseCallback(
    callback: (response: SDKControlResponse) => Promise<void>,
  ): void {
    this.unexpectedResponseCallback = callback
  }

  /**
   * Inject a control_response message to resolve a pending permission request.
   * Used by the bridge to feed permission responses from acosmi.com into the
   * SDK permission flow.
   *
   * Also sends a control_cancel_request to the SDK consumer so its canUseTool
   * callback is aborted via the signal — otherwise the callback hangs.
   */
  injectControlResponse(response: SDKControlResponse): void {
    const requestId = response.response?.request_id
    if (!requestId) return
    const request = this.pendingRequests.get(requestId)
    if (!request) return
    this.trackResolvedControlRequest(request.request)
    this.pendingRequests.delete(requestId)
    // Cancel the SDK consumer's canUseTool callback — the bridge won.
    void this.write({
      type: 'control_cancel_request',
      request_id: requestId,
    })
    if (response.response.subtype === 'error') {
      request.reject(new Error(response.response.error))
    } else {
      const result = response.response.response
      if (request.schema) {
        try {
          request.resolve(request.schema.parse(result))
        } catch (error) {
          request.reject(error)
        }
      } else {
        request.resolve({})
      }
    }
  }

  /**
   * Register a callback invoked whenever a can_use_tool control_request
   * is written to stdout. Used by the bridge to forward permission
   * requests to acosmi.com.
   */
  setOnControlRequestSent(
    callback: ((request: SDKControlRequest) => void) | undefined,
  ): void {
    this.onControlRequestSent = callback
  }

  /**
   * Register a callback invoked when a can_use_tool control_response arrives
   * from the SDK consumer (via stdin). Used by the bridge to cancel the
   * stale permission prompt on acosmi.com when the SDK consumer wins the race.
   */
  setOnControlRequestResolved(
    callback: ((requestId: string) => void) | undefined,
  ): void {
    this.onControlRequestResolved = callback
  }

  /**
   * Install the post-setup, process-private native TUI request router.
   *
   * The direct route calls this exactly once before iteration starts. Remote
   * and standard SDK routes never call it, so their accepted input surface is
   * unchanged. The existing read() generator remains the sole stdin reader.
   */
  setDirectTuiRuntimeActionRouter(
    router: DirectTuiRuntimeActionRouter,
  ): void {
    if (this.directTuiRuntimeActionRouter) {
      throw new Error('Direct TUI runtime action router may be installed once')
    }
    this.directTuiRuntimeActionRouter = router
  }

  private async processLine(
    line: string,
  ): Promise<StdinMessage | SDKMessage | undefined> {
    // Skip empty lines (e.g. from double newlines in piped stdin)
    if (!line) {
      return undefined
    }
    try {
      const parsedValue: unknown = jsonParse(line)
      if (this.directTuiRuntimeActionRouter) {
        const routed = await this.directTuiRuntimeActionRouter(parsedValue)
        if (routed.handled) {
          if (routed.response) this.outbound.enqueue(routed.response)
          if (routed.backgroundSettlement) {
            void this.enqueueBackgroundPrivateRuntimeSettlement(
              routed.backgroundSettlement,
            )
          }
          return undefined
        }
      }
      const message = parsedValue as StdinMessage | SDKMessage
      if (message.type === 'keep_alive') {
        // Silently ignore keep-alive messages
        return undefined
      }
      if (message.type === 'update_environment_variables') {
        // Apply environment variable updates directly to process.env.
        // The owning process must observe these values as well as child
        // commands launched later in the session.
        const keys = Object.keys(message.variables)
        for (const [key, value] of Object.entries(message.variables)) {
          process.env[key] = value
        }
        logForDebugging(
          `[structuredIO] applied update_environment_variables: ${keys.join(', ')}`,
        )
        return undefined
      }
      if (message.type === 'control_response') {
        // Close lifecycle for every control_response, including duplicates
        // and orphans — orphans don't yield to print.ts's main loop, so this
        // is the only path that sees them. uuid is server-injected into the
        // payload.
        const uuid =
          'uuid' in message && typeof message.uuid === 'string'
            ? message.uuid
            : undefined
        if (uuid) {
          notifyCommandLifecycle(uuid, 'completed')
        }
        const request = this.pendingRequests.get(message.response.request_id)
        if (!request) {
          if (
            this.resolvedControlRequestIds.has(message.response.request_id)
          ) {
            logForDebugging(
              `Ignoring duplicate control_response for already-resolved request_id=${message.response.request_id}`,
            )
            return undefined
          }
          // Check if this tool_use was already resolved through the normal
          // permission flow. Duplicate control_response deliveries (e.g. from
          // WebSocket reconnects) arrive after the original was handled, and
          // re-processing them would push duplicate assistant messages into
          // the conversation, causing API 400 errors.
          const responsePayload =
            message.response.subtype === 'success'
              ? message.response.response
              : undefined
          const toolUseID = responsePayload?.toolUseID
          if (
            typeof toolUseID === 'string' &&
            this.resolvedToolUseIds.has(toolUseID)
          ) {
            logForDebugging(
              `Ignoring duplicate control_response for already-resolved toolUseID=${toolUseID} request_id=${message.response.request_id}`,
            )
            return undefined
          }
          if (this.unexpectedResponseCallback) {
            await this.unexpectedResponseCallback(message)
          }
          return undefined // Ignore responses for requests we don't know about
        }
        this.trackResolvedControlRequest(request.request)
        this.pendingRequests.delete(message.response.request_id)
        // Notify the bridge when the SDK consumer resolves a can_use_tool
        // request, so it can cancel the stale permission prompt on acosmi.com.
        if (
          request.request.request.subtype === 'can_use_tool' &&
          this.onControlRequestResolved
        ) {
          try {
            this.onControlRequestResolved(message.response.request_id)
          } catch (error) {
            logForDebugging(
              `[StructuredIO] resolved-request observer failed for ${message.response.request_id}: ${error}`,
              { level: 'warn' },
            )
          }
        }

        if (message.response.subtype === 'error') {
          request.reject(new Error(message.response.error))
          return undefined
        }
        const result = message.response.response
        if (request.schema) {
          try {
            request.resolve(request.schema.parse(result))
          } catch (error) {
            request.reject(error)
          }
        } else {
          request.resolve({})
        }
        // Propagate control responses when replay is enabled
        if (this.replayUserMessages) {
          return message
        }
        return undefined
      }
      if (
        message.type !== 'user' &&
        message.type !== 'control_request' &&
        message.type !== 'assistant' &&
        message.type !== 'system'
      ) {
        logForDebugging(`Ignoring unknown message type: ${message.type}`, {
          level: 'warn',
        })
        return undefined
      }
      if (message.type === 'control_request') {
        if (!message.request) {
          exitWithMessage(`Error: Missing request on control_request`)
        }
        return message
      }
      if (message.type === 'assistant' || message.type === 'system') {
        return message
      }
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      if ((message.message as any).role !== 'user') {
        exitWithMessage(
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          `Error: Expected message role 'user', got '${(message.message as any).role}'`,
        )
      }
      return message
    } catch (error) {
      console.error(`Error parsing streaming input line: ${line}: ${error}`)
      // eslint-disable-next-line custom-rules/no-process-exit
      process.exit(1)
    }
  }

  private async enqueueBackgroundPrivateRuntimeSettlement(
    settlement: Promise<DirectTuiRuntimeActionRoute>,
  ): Promise<void> {
    try {
      const settled = await settlement
      if (settled.response) this.outbound.enqueue(settled.response)
    } catch (error) {
      // The closed runtime router normally converts handler errors into a
      // correlated action_failed result. Keep an explicit rejection observer
      // at this async boundary so an unexpected implementation failure
      // (including after stdin closes) cannot become an unhandled rejection
      // or terminate input. Diagnostics are deliberately best-effort too.
      try {
        logForDebugging(
          `[StructuredIO] background private runtime action failed: ${error instanceof Error ? error.name : 'unknown'}`,
          { level: 'warn' },
        )
      } catch {
        // Never allow diagnostic infrastructure to reopen the rejection.
      }
    }
  }

  async write(message: StructuredIoOutboundMessage): Promise<void> {
    writeToStdout(ndjsonSafeStringify(message) + '\n')
  }

  private async sendRequest<Response>(
    request: SDKControlRequest['request'],
    schema: z.Schema,
    signal?: AbortSignal,
    requestId: string = randomUUID(),
    onQueued?: () => void,
  ): Promise<Response> {
    const message: SDKControlRequest = {
      type: 'control_request',
      request_id: requestId,
      request,
    }
    return this.sendCorrelatedRequest<Response>(
      message,
      schema,
      signal,
      onQueued,
    )
  }

  private async sendCorrelatedRequest<Response>(
    message: SDKControlRequest,
    schema: z.Schema,
    signal?: AbortSignal,
    onQueued?: () => void,
  ): Promise<Response> {
    const requestId = message.request_id
    if (this.inputClosed) {
      throw new Error('Stream closed')
    }
    if (signal?.aborted) {
      throw new AbortError()
    }
    if (this.pendingRequests.has(requestId)) {
      throw new Error(`Duplicate control request ID: ${requestId}`)
    }

    let requestWasQueued = false
    const aborted = () => {
      // Immediately reject the outstanding promise, without
      // waiting for the host to acknowledge the cancellation.
      const request = this.pendingRequests.get(requestId)
      if (request) {
        // Track the tool_use ID as resolved before rejecting, so that a
        // late response from the host is ignored by the orphan handler.
        this.trackResolvedControlRequest(request.request)
        this.pendingRequests.delete(requestId)
        if (requestWasQueued) {
          this.outbound.enqueue({
            type: 'control_cancel_request',
            request_id: requestId,
          })
        }
        request.reject(new AbortError())
      }
    }

    // Install correlation state before publishing the request. The direct TUI
    // bridge is allowed to answer synchronously from onControlRequestSent; if
    // the map were populated afterwards, that valid response would be lost.
    const response = new Promise<Response>((resolve, reject) => {
      this.pendingRequests.set(requestId, {
        request: message,
        resolve: result => {
          resolve(result as Response)
        },
        reject,
        schema,
      })
    })
    signal?.addEventListener('abort', aborted, { once: true })
    try {
      // The signal may have transitioned between the initial check and
      // listener registration. Re-check before exposing any request to a host.
      if (signal?.aborted) {
        aborted()
        return await response
      }

      this.outbound.enqueue(message)
      requestWasQueued = true
      if (
        message.request.subtype === 'can_use_tool' &&
        this.onControlRequestSent
      ) {
        try {
          this.onControlRequestSent(message)
        } catch (error) {
          logForDebugging(
            `[StructuredIO] sent-request observer failed for ${requestId}: ${error}`,
            { level: 'warn' },
          )
        }
      }
      try {
        onQueued?.()
      } catch (error) {
        logForDebugging(
          `[StructuredIO] queued-request observer failed for ${requestId}: ${error}`,
          { level: 'warn' },
        )
      }
      return await response
    } finally {
      if (signal) {
        signal.removeEventListener('abort', aborted)
      }
      this.pendingRequests.delete(requestId)
    }
  }

  /**
   * Route an already-authorized process-local permission source through the
   * existing `can_use_tool` reverse-control lifecycle.
   *
   * The native direct TUI uses this for teammate mailbox permission requests:
   * it does not add a protocol subtype or reinterpret the request as a user
   * prompt. The caller remains responsible for returning the decision through
   * the mailbox authority that originated it.
   */
  async requestDirectPermission(
    request: SDKControlPermissionRequest,
    onPermissionPrompt?: (details: RequiresActionDetails) => void,
    onQueued?: () => void,
  ): Promise<PermissionToolOutput> {
    const requestId = randomUUID()
    return this.sendRequest<PermissionToolOutput>(
      request,
      permissionToolOutputSchema(),
      undefined,
      requestId,
      () => {
        try {
          onPermissionPrompt?.({
            tool_name: request.tool_name,
            action_description: request.description ?? request.tool_name,
            tool_use_id: request.tool_use_id,
            request_id: requestId,
            input: request.input,
          })
        } finally {
          // Once the correlated request is in the existing outbound queue,
          // caller bookkeeping must observe that fact even if a secondary
          // state/telemetry observer throws.
          onQueued?.()
        }
      },
    )
  }

  /**
   * Notify the same-generation native renderer that its discovery catalog is
   * stale. This exact helper is deliberately process-private; standard SDK
   * routes never call it and the request is not added to public schemas.
   */
  async requestDirectTuiCommandCatalogChanged(
    commands: readonly CommandCatalogEntry[],
  ): Promise<DirectTuiCommandCatalogChangedResponse> {
    const request = buildDirectTuiCommandCatalogChangedRequest(commands)
    return this.sendRequest<DirectTuiCommandCatalogChangedResponse>(
      // The public SDK union is intentionally closed. StructuredIO's wire
      // envelope is reused only inside the direct process pair, while the
      // dedicated builder and response schema keep this extension closed.
      request as unknown as SDKControlRequest['request'],
      DirectTuiCommandCatalogChangedResponseSchema,
    )
  }

  createCanUseTool(
    onPermissionPrompt?: (details: RequiresActionDetails) => void,
  ): CanUseToolFn {
    return async (
      tool: Tool,
      input: { [key: string]: unknown },
      toolUseContext: ToolUseContext,
      assistantMessage: AssistantMessage,
      toolUseID: string,
      forceDecision?: PermissionDecision,
    ): Promise<PermissionDecision> => {
      const mainPermissionResult =
        forceDecision ??
        (await hasPermissionsToUseTool(
          tool,
          input,
          toolUseContext,
          assistantMessage,
          toolUseID,
        ))
      // If the tool is allowed or denied, return the result
      if (
        mainPermissionResult.behavior === 'allow' ||
        mainPermissionResult.behavior === 'deny'
      ) {
        return mainPermissionResult
      }

      // Run PermissionRequest hooks in parallel with the SDK permission
      // prompt.  In the terminal CLI, hooks race against the interactive
      // prompt so that e.g. a hook with --delay 20 doesn't block the UI.
      // We need the same behavior here: the SDK host (VS Code, etc.) shows
      // its permission dialog immediately while hooks run in the background.
      // Whichever resolves first wins; the loser is cancelled/ignored.

      // One controller owns both sides of the race. Once either side wins,
      // cancel the loser so a late hook cannot continue doing work after the
      // user has already made a decision in the SDK/TUI.
      const permissionRaceAbortController = new AbortController()
      const parentSignal = toolUseContext.abortController.signal
      const cancelledDecision = () =>
        permissionPromptToolResultToPermissionDecision(
          {
            behavior: 'deny',
            message:
              'Tool permission request was cancelled before a decision could be committed',
            toolUseID,
          },
          tool,
          input,
          toolUseContext,
        )
      if (parentSignal.aborted) {
        return cancelledDecision()
      }
      // Forward parent abort to our local controller
      const onParentAbort = () => permissionRaceAbortController.abort()
      parentSignal.addEventListener('abort', onParentAbort, { once: true })

      try {
        // Start the hook evaluation (runs in background)
        const hookPromise = executePermissionRequestHooksForSDK(
          tool.name,
          toolUseID,
          input,
          toolUseContext,
          mainPermissionResult.suggestions,
          permissionRaceAbortController.signal,
        ).then(decision => ({ source: 'hook' as const, decision }))

        // Start the SDK permission prompt immediately (don't wait for hooks)
        const requestId = randomUUID()
        onPermissionPrompt?.(
          buildRequiresActionDetails(tool, input, toolUseID, requestId),
        )
        const sdkPromise = this.sendRequest<PermissionToolOutput>(
          buildCanUseToolControlRequest(
            tool,
            input,
            mainPermissionResult,
            toolUseID,
            toolUseContext.agentId,
          ),
          permissionToolOutputSchema(),
          permissionRaceAbortController.signal,
          requestId,
        ).then(result => ({ source: 'sdk' as const, result }))

        // Race: hook completion vs SDK prompt response.
        // The hook promise always resolves (never rejects), returning
        // undefined if no hook made a decision.
        const winner = await Promise.race([hookPromise, sdkPromise])
        if (parentSignal.aborted) {
          return cancelledDecision()
        }

        if (winner.source === 'hook') {
          if (winner.decision) {
            // Hook decided — abort the pending SDK request.
            // Suppress the expected AbortError rejection from sdkPromise.
            sdkPromise.catch(() => {})
            permissionRaceAbortController.abort()
            return commitWinningPermissionHookDecision(
              winner.decision,
              toolUseContext,
            )
          }
          // Hook passed through (no decision) — wait for the SDK prompt
          const sdkResult = await sdkPromise
          if (parentSignal.aborted) {
            return cancelledDecision()
          }
          return permissionPromptToolResultToPermissionDecision(
            sdkResult.result,
            tool,
            input,
            toolUseContext,
          )
        }

        // SDK prompt responded first. Cancel the hook before committing the
        // SDK result; hook-produced permission updates are intentionally
        // side-effect free until the hook is selected as the winner.
        permissionRaceAbortController.abort()
        return permissionPromptToolResultToPermissionDecision(
          winner.result,
          tool,
          input,
          toolUseContext,
        )
      } catch (error) {
        if (parentSignal.aborted) {
          return cancelledDecision()
        }
        return permissionPromptToolResultToPermissionDecision(
          {
            behavior: 'deny',
            message: `Tool permission request failed: ${error}`,
            toolUseID,
          },
          tool,
          input,
          toolUseContext,
        )
      } finally {
        permissionRaceAbortController.abort()
        // Only transition back to 'running' if no other permission prompts
        // are pending (concurrent tool execution can have multiple in-flight).
        if (
          !parentSignal.aborted &&
          this.getPendingPermissionRequests().length === 0
        ) {
          notifySessionStateChanged('running')
        }
        parentSignal.removeEventListener('abort', onParentAbort)
      }
    }
  }

  createHookCallback(callbackId: string, timeout?: number): HookCallback {
    return {
      type: 'callback',
      timeout,
      callback: async (
        input: HookInput,
        toolUseID: string | null,
        abort: AbortSignal | undefined,
      ): Promise<HookJSONOutput> => {
        try {
          const result = await this.sendRequest<HookJSONOutput>(
            {
              subtype: 'hook_callback',
              callback_id: callbackId,
              input,
              tool_use_id: toolUseID || undefined,
            },
            hookJSONOutputSchema(),
            abort,
          )
          return result
        } catch (error) {
          console.error(`Error in hook callback ${callbackId}:`, error)
          return {}
        }
      },
    }
  }

  /**
   * Sends an elicitation request to the SDK consumer and returns the response.
   */
  async handleElicitation(
    serverName: string,
    message: string,
    requestedSchema?: Record<string, unknown>,
    signal?: AbortSignal,
    mode?: 'form' | 'url',
    url?: string,
    elicitationId?: string,
  ): Promise<ElicitResult> {
    try {
      const result = await this.sendRequest<ElicitResult>(
        {
          subtype: 'elicitation',
          mcp_server_name: serverName,
          message,
          mode,
          url,
          elicitation_id: elicitationId,
          requested_schema: requestedSchema,
        },
        SDKControlElicitationResponseSchema(),
        signal,
      )
      return result
    } catch {
      return { action: 'cancel' as const }
    }
  }

  /**
   * Creates a SandboxAskCallback that forwards sandbox network permission
   * requests to the SDK host as can_use_tool control_requests.
   *
   * This piggybacks on the existing can_use_tool protocol with a synthetic
   * tool name so that SDK hosts (VS Code, CCR, etc.) can prompt the user
   * for network access without requiring a new protocol subtype.
   */
  createSandboxAskCallback(): (hostPattern: {
    host: string
    port?: number
  }) => Promise<boolean> {
    return async (hostPattern): Promise<boolean> => {
      try {
        const result = await this.sendRequest<PermissionToolOutput>(
          {
            subtype: 'can_use_tool',
            tool_name: SANDBOX_NETWORK_ACCESS_TOOL_NAME,
            input: { host: hostPattern.host },
            tool_use_id: randomUUID(),
            description: `Allow network connection to ${hostPattern.host}?`,
          },
          permissionToolOutputSchema(),
        )
        return result.behavior === 'allow'
      } catch {
        // If the request fails (stream closed, abort, etc.), deny the connection
        return false
      }
    }
  }

  /**
   * Sends an MCP message to an SDK server and waits for the response
   */
  async sendMcpMessage(
    serverName: string,
    message: JSONRPCMessage,
  ): Promise<JSONRPCMessage> {
    const response = await this.sendRequest<{ mcp_response: JSONRPCMessage }>(
      {
        subtype: 'mcp_message',
        server_name: serverName,
        message,
      },
      z.object({
        mcp_response: z.any() as z.Schema<JSONRPCMessage>,
      }),
    )
    return response.mcp_response
  }
}

function exitWithMessage(message: string): never {
  console.error(message)
  // eslint-disable-next-line custom-rules/no-process-exit
  process.exit(1)
}

/**
 * Execute PermissionRequest hooks and return a decision if one is made.
 * Returns undefined if no hook made a decision.
 */
async function executePermissionRequestHooksForSDK(
  toolName: string,
  toolUseID: string,
  input: Record<string, unknown>,
  toolUseContext: ToolUseContext,
  suggestions: PermissionUpdate[] | undefined,
  signal: AbortSignal,
): Promise<PermissionHookEvaluation | undefined> {
  const appState = toolUseContext.getAppState()
  const permissionMode = appState.toolPermissionContext.mode

  // Iterate directly over the generator instead of using `all`
  const hookGenerator = executePermissionRequestHooks(
    toolName,
    toolUseID,
    input,
    toolUseContext,
    permissionMode,
    suggestions,
    signal,
  )

  for await (const hookResult of hookGenerator) {
    if (
      hookResult.permissionRequestResult &&
      (hookResult.permissionRequestResult.behavior === 'allow' ||
        hookResult.permissionRequestResult.behavior === 'deny')
    ) {
      const decision = hookResult.permissionRequestResult
      if (decision.behavior === 'allow') {
        // PermissionRequest hooks are policy automation, not the correlated
        // human interaction host. They may deny an interactive question, but
        // an allow result must not manufacture answers or suppress the prompt.
        if (!isPermissionRequestHookAllowAuthoritative(toolName)) {
          logForDebugging(
            'Ignoring PermissionRequest hook allow for AskUserQuestion; a correlated host response is required',
            { level: 'warn' },
          )
          continue
        }
        const finalInput = decision.updatedInput || input
        return {
          decision: {
            behavior: 'allow',
            updatedInput: finalInput,
            userModified: false,
            decisionReason: {
              type: 'hook',
              hookName: 'PermissionRequest',
            },
          },
          permissionUpdates: decision.updatedPermissions ?? [],
          interrupt: false,
        }
      } else {
        // Hook denied the permission
        return {
          decision: {
            behavior: 'deny',
            message:
              decision.message || 'Permission denied by PermissionRequest hook',
            decisionReason: {
              type: 'hook',
              hookName: 'PermissionRequest',
              reason: decision.message,
            },
          },
          permissionUpdates: [],
          interrupt: decision.interrupt ?? false,
        }
      }
    }
  }

  return undefined
}

type PermissionHookEvaluation = {
  decision: PermissionDecision
  permissionUpdates: PermissionUpdate[]
  interrupt: boolean
}

/**
 * Commit the only local side effects produced by a PermissionRequest hook.
 * Evaluation itself is pure so losing a race against an SDK/TUI decision can
 * never persist an allow rule or abort the active turn after the user's choice.
 */
function commitWinningPermissionHookDecision(
  evaluation: PermissionHookEvaluation,
  toolUseContext: ToolUseContext,
): PermissionDecision {
  if (evaluation.permissionUpdates.length > 0) {
    persistPermissionUpdates(evaluation.permissionUpdates)
    toolUseContext.setAppState(prev => ({
      ...prev,
      toolPermissionContext: applyPermissionUpdates(
        prev.toolPermissionContext,
        evaluation.permissionUpdates,
      ),
    }))
  }
  if (evaluation.interrupt && evaluation.decision.behavior === 'deny') {
    logForDebugging(
      `PermissionRequest hook interrupted tool use: ${evaluation.decision.message}`,
    )
    toolUseContext.abortController.abort()
  }
  return evaluation.decision
}
