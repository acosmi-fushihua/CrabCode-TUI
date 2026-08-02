import { randomUUID } from 'crypto'
import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import { getSessionId } from '../../bootstrap/state.js'
import { getAutoMemPath, isAutoMemoryEnabled } from '../../memdir/paths.js'
import type { ToolUseContext } from '../../Tool.js'
import type {
  Message,
  SystemLocalCommandMessage,
  SystemMessage,
} from '../../types/message.js'
import { getCwd } from '../../utils/cwd.js'
import { logForDebugging } from '../../utils/debug.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import {
  flushSessionStorage,
  getTranscriptPath,
} from '../../utils/sessionStorage.js'
import { feature } from '../../utils/featurePolyfill.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../analytics/growthbook.js'
import { isAutoDreamEnabled } from './dream/config.js'
import { runDreamMemoryTrigger } from './dream/runner.js'
import { runExtractMemoryTrigger } from './extract/runner.js'
import {
  MemoryRecoveryContextError,
  withAuthoritativeMemoryRecoveryContext,
} from './recoveryContext.js'
import {
  MEMORY_RECOVERY_SCHEMA_VERSION,
  type MemoryRecoveryLocator,
} from './recoveryProtocol.js'

/* eslint-disable @typescript-eslint/no-require-imports */
const teamMemPaths = feature('TEAMMEM')
  ? (require('../../memdir/teamMemPaths.js') as typeof import('../../memdir/teamMemPaths.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

type AppendSystemMessage = (
  msg: Exclude<SystemMessage, SystemLocalCommandMessage>,
) => void

export type MemoryRunnerStopHookContext = {
  messages: Message[]
  systemPrompt: unknown
  userContext: { [k: string]: string }
  systemContext: { [k: string]: string }
  toolUseContext: {
    abortController: ToolUseContext['abortController']
    agentId?: ToolUseContext['agentId']
    appendSystemMessage?: AppendSystemMessage
    getAppState?: ToolUseContext['getAppState']
    setAppState?: ToolUseContext['setAppState']
    setAppStateForTasks?: ToolUseContext['setAppStateForTasks']
    revalidateSideEffectAuthority?: ToolUseContext['revalidateSideEffectAuthority']
  }
  querySource?: string
}

export type MemoryRunnerKind = 'dream' | 'extract'

export type MemoryRunnerLeaderFence = {
  leaderToken: string
  leaderEpoch: number
}

export type MemoryTurnEndTrigger = {
  trigger_id: string
  kind: MemoryRunnerKind
  lock_token?: string
  runner_payload: Record<string, unknown>
}

type ClaimedMemoryTurnEndTrigger = MemoryTurnEndTrigger & {
  recovery: MemoryRecoveryLocator
  delivery_owner: string
  delivery_epoch: number
  lease_expires_at_ms: number
}

type MemoryRunnerCandidate =
  | (MemoryTurnEndTrigger & { recovery: MemoryRecoveryLocator })
  | {
      trigger_id: string
      invalid_reason: 'invalid_recovery_locator'
    }

export type MemoryTurnEndPayload = {
  recovery_schema_version: number
  session_id: string
  current_session_id: string
  last_assistant_uuid: string
  project_cwd: string
  transcript_path: string
  memory_dir: string
  team_memory_dir?: string
  message_counts: Record<string, number>
  feature_flags: Record<string, boolean>
  requested_kinds: MemoryRunnerKind[]
}

export type MemoryTriggerResult = {
  writtenPaths?: string[]
  usage?: Record<string, unknown>
}

type MemoryRunnerCompletedPayload = {
  trigger_id: string
  kind: MemoryRunnerKind
  written_paths: string[]
  usage?: Record<string, unknown>
  error?: {
    message: string
    name?: string
  }
  delivery_owner: string
  delivery_epoch: number
}

export type MemoryRunnerCacheSafeParams = {
  systemPrompt: MemoryRunnerStopHookContext['systemPrompt']
  userContext: MemoryRunnerStopHookContext['userContext']
  systemContext: MemoryRunnerStopHookContext['systemContext']
  toolUseContext: MemoryRunnerStopHookContext['toolUseContext']
  forkContextMessages: MemoryRunnerStopHookContext['messages']
}

export type MemoryTriggerRunnerInput = {
  trigger: MemoryTurnEndTrigger
  context: MemoryRunnerStopHookContext
  appendSystemMessage?: AppendSystemMessage
  cacheSafeParams: MemoryRunnerCacheSafeParams
  /**
   * Fail-closed delivery fence check. Runners must invoke this immediately
   * before every externally visible side effect (the memory runners wire it
   * into their CanUseToolFn).
   */
  assertDeliveryAuthority: () => void
}

type PendingTurnEnd = {
  kinds: Set<MemoryRunnerKind>
  appendSystemMessage?: AppendSystemMessage
  promise: Promise<void>
}

type MemoryTriggerRunner = (
  input: MemoryTriggerRunnerInput,
) => Promise<MemoryTriggerResult | void>

const EVALUATE_TIMEOUT_MS = 250
const RUNNER_CONTROL_TIMEOUT_MS = 2_000
const RUNNER_RENEW_INTERVAL_MS = 20_000
const COMPLETION_RETRY_DELAYS_MS = [0, 50, 200] as const
const RUNNER_CANDIDATE_LIMIT = 32
const MAX_TIMER_DELAY_MS = 2_147_483_647
const MEMORY_RUNNER_WORKER_ID = `memory-runner:${process.pid}:${randomUUID()}`

let pendingByContext = new WeakMap<MemoryRunnerStopHookContext, PendingTurnEnd>()
let inFlight = new Set<Promise<void>>()

type MemoryRunnerRecoveryOwner = {
  fence: MemoryRunnerLeaderFence
  abortController: AbortController
  accepting: boolean
  tasks: Set<Promise<void>>
  drainPromise: Promise<void> | null
  drainRequested: boolean
  wakeTimer: ReturnType<typeof setTimeout> | null
  wakeDeadlineMs: number | null
}

let recoveryOwner: MemoryRunnerRecoveryOwner | null = null

let triggerRunner: MemoryTriggerRunner = dispatchMemoryTrigger

async function dispatchMemoryTrigger(
  input: MemoryTriggerRunnerInput,
): Promise<MemoryTriggerResult> {
  return input.trigger.kind === 'dream'
    ? runDreamMemoryTrigger(input)
    : runExtractMemoryTrigger(input)
}

function normalizeKinds(kinds: readonly MemoryRunnerKind[]): MemoryRunnerKind[] {
  const out: MemoryRunnerKind[] = []
  for (const kind of kinds) {
    if ((kind === 'dream' || kind === 'extract') && !out.includes(kind)) {
      out.push(kind)
    }
  }
  return out
}

function findLastAssistantUuid(messages: Message[]): string | undefined {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i]
    if (message?.type === 'assistant') {
      return message.uuid
    }
  }
  return undefined
}

function buildMessageCounts(messages: Message[]): Record<string, number> {
  const counts: Record<string, number> = { total: messages.length }
  for (const message of messages) {
    counts[message.type] = (counts[message.type] ?? 0) + 1
  }
  return counts
}

function isTeamMemoryActive(): boolean {
  return feature('TEAMMEM') && (teamMemPaths?.isTeamMemoryEnabled() ?? false)
}

function buildFeatureFlags(): Record<string, boolean> {
  const teamMemoryEnabled = isTeamMemoryActive()
  return {
    EXTRACT_MEMORIES: feature('EXTRACT_MEMORIES'),
    KAIROS: feature('KAIROS'),
    KAIROS_DREAM: feature('KAIROS_DREAM'),
    TEAMMEM: feature('TEAMMEM'),
    auto_memory_enabled: isAutoMemoryEnabled(),
    auto_dream_enabled: isAutoDreamEnabled(),
    team_memory_enabled: teamMemoryEnabled,
    remote_mode: isEnvTruthy(process.env.CRABCODE_REMOTE),
    skip_index: getFeatureValue_CACHED_MAY_BE_STALE(
      'tengu_moth_copse',
      false,
    ),
  }
}

function buildTeamMemoryDir(): string | undefined {
  return isTeamMemoryActive() ? teamMemPaths!.getTeamMemPath() : undefined
}

function buildEvaluatePayload(
  context: MemoryRunnerStopHookContext,
  requestedKinds: MemoryRunnerKind[],
): MemoryTurnEndPayload {
  const sessionId = getSessionId()
  const lastAssistantUuid = findLastAssistantUuid(context.messages)
  if (!lastAssistantUuid) {
    throw new Error(
      'memory turn-end evaluation requires an authoritative assistant leaf',
    )
  }
  const teamMemoryDir = buildTeamMemoryDir()
  const payload: MemoryTurnEndPayload = {
    recovery_schema_version: MEMORY_RECOVERY_SCHEMA_VERSION,
    session_id: sessionId,
    current_session_id: sessionId,
    last_assistant_uuid: lastAssistantUuid,
    project_cwd: getCwd(),
    transcript_path: getTranscriptPath(),
    memory_dir: getAutoMemPath(),
    message_counts: buildMessageCounts(context.messages),
    feature_flags: buildFeatureFlags(),
    requested_kinds: requestedKinds,
  }
  if (teamMemoryDir) {
    payload.team_memory_dir = teamMemoryDir
  }
  return payload
}

function createStopHookCacheSafeParams(
  context: MemoryRunnerStopHookContext,
): MemoryRunnerCacheSafeParams {
  return {
    systemPrompt: context.systemPrompt,
    userContext: context.userContext,
    systemContext: context.systemContext,
    toolUseContext: context.toolUseContext,
    forkContextMessages: context.messages,
  }
}

function isTrigger(value: unknown): value is MemoryTurnEndTrigger {
  if (value === null || typeof value !== 'object') return false
  const candidate = value as Partial<MemoryTurnEndTrigger>
  return (
    typeof candidate.trigger_id === 'string' &&
    (candidate.kind === 'dream' || candidate.kind === 'extract') &&
    candidate.runner_payload !== null &&
    typeof candidate.runner_payload === 'object' &&
    !Array.isArray(candidate.runner_payload)
  )
}

function isRecoveryLocator(value: unknown): value is MemoryRecoveryLocator {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return false
  }
  const candidate = value as Partial<MemoryRecoveryLocator>
  return (
    candidate.recovery_schema_version === MEMORY_RECOVERY_SCHEMA_VERSION &&
    typeof candidate.trigger_id === 'string' &&
    (candidate.kind === 'dream' || candidate.kind === 'extract') &&
    typeof candidate.session_id === 'string' &&
    typeof candidate.current_session_id === 'string' &&
    typeof candidate.context_leaf_uuid === 'string' &&
    typeof candidate.project_cwd === 'string' &&
    typeof candidate.transcript_path === 'string' &&
    typeof candidate.project_state_dir === 'string' &&
    typeof candidate.memory_dir === 'string'
  )
}

function isClaimedTrigger(
  value: unknown,
): value is ClaimedMemoryTurnEndTrigger {
  if (!isTrigger(value)) return false
  const candidate = value as Partial<ClaimedMemoryTurnEndTrigger>
  return (
    isRecoveryLocator(candidate.recovery) &&
    typeof candidate.delivery_owner === 'string' &&
    Number.isSafeInteger(candidate.delivery_epoch) &&
    (candidate.delivery_epoch ?? -1) >= 0 &&
    Number.isSafeInteger(candidate.lease_expires_at_ms) &&
    (candidate.lease_expires_at_ms ?? -1) >= 0
  )
}

type RunnerReceipt = {
  received: boolean
  reason?: string
}

type RunnerLeaseReceipt =
  | {
      received: true
      lease_expires_at_ms: number
    }
  | {
      received: false
      reason?: string
      lease_expires_at_ms: null
    }

type RunnerReleaseReceipt =
  | {
      received: true
      next_attempt_at_ms: number
    }
  | {
      received: false
      reason?: string
      next_attempt_at_ms: null
    }

function parseRunnerReceipt(method: string, value: unknown): RunnerReceipt {
  if (
    value === null ||
    typeof value !== 'object' ||
    typeof (value as { received?: unknown }).received !== 'boolean'
  ) {
    throw new Error(`${method} returned an invalid receipt`)
  }
  const receipt = value as RunnerReceipt
  if (
    receipt.reason !== undefined &&
    typeof receipt.reason !== 'string'
  ) {
    throw new Error(`${method} returned an invalid reason`)
  }
  return receipt
}

function parseRunnerLeaseReceipt(
  method: string,
  value: unknown,
): RunnerLeaseReceipt {
  const receipt = parseRunnerReceipt(method, value)
  const leaseExpiresAtMs = (
    value as { lease_expires_at_ms?: unknown }
  ).lease_expires_at_ms
  if (
    receipt.received
      ? !Number.isSafeInteger(leaseExpiresAtMs) ||
        (leaseExpiresAtMs as number) <= 0
      : leaseExpiresAtMs !== null
  ) {
    throw new Error(
      `${method} returned an invalid lease_expires_at_ms correlation`,
    )
  }
  return receipt.received
    ? {
        received: true,
        lease_expires_at_ms: leaseExpiresAtMs as number,
      }
    : {
        received: false,
        reason: receipt.reason,
        lease_expires_at_ms: null,
      }
}

function parseRunnerReleaseReceipt(value: unknown): RunnerReleaseReceipt {
  const receipt = parseRunnerReceipt('memory.runner.release', value)
  const nextAttemptAtMs = (
    value as { next_attempt_at_ms?: unknown }
  ).next_attempt_at_ms
  if (
    receipt.received
      ? !Number.isSafeInteger(nextAttemptAtMs) ||
        (nextAttemptAtMs as number) < 0 ||
        receipt.reason !== undefined
      : nextAttemptAtMs !== null ||
        typeof receipt.reason !== 'string' ||
        receipt.reason.length === 0
  ) {
    throw new Error(
      'memory.runner.release returned an invalid next-attempt correlation',
    )
  }
  return receipt.received
    ? {
        received: true,
        next_attempt_at_ms: nextAttemptAtMs as number,
      }
    : {
        received: false,
        reason: receipt.reason,
        next_attempt_at_ms: null,
      }
}

function validateLeaderFence(fence: MemoryRunnerLeaderFence): void {
  if (
    fence.leaderToken.length === 0 ||
    !Number.isSafeInteger(fence.leaderEpoch) ||
    fence.leaderEpoch <= 0
  ) {
    throw new Error('memory runner leader fence is invalid')
  }
}

function assertRecoveryOwnerActive(owner: MemoryRunnerRecoveryOwner): void {
  if (
    recoveryOwner !== owner ||
    !owner.accepting ||
    owner.abortController.signal.aborted
  ) {
    throw new Error('memory runner leader authority is no longer active')
  }
}

function leaderFencePayload(
  owner: MemoryRunnerRecoveryOwner,
): {
  leader_token: string
  leader_epoch: number
} {
  assertRecoveryOwnerActive(owner)
  return rawLeaderFencePayload(owner)
}

function rawLeaderFencePayload(
  owner: MemoryRunnerRecoveryOwner,
): {
  leader_token: string
  leader_epoch: number
} {
  return {
    leader_token: owner.fence.leaderToken,
    leader_epoch: owner.fence.leaderEpoch,
  }
}

function scheduleRunnerRecoveryDrainAt(
  owner: MemoryRunnerRecoveryOwner,
  deadlineMs: number,
): void {
  if (
    recoveryOwner !== owner ||
    !owner.accepting ||
    owner.abortController.signal.aborted
  ) {
    return
  }
  if (
    owner.wakeDeadlineMs !== null &&
    owner.wakeDeadlineMs <= deadlineMs
  ) {
    return
  }
  if (owner.wakeTimer) {
    clearTimeout(owner.wakeTimer)
  }
  owner.wakeDeadlineMs = deadlineMs
  const wake = (): void => {
    if (
      recoveryOwner !== owner ||
      !owner.accepting ||
      owner.abortController.signal.aborted ||
      owner.wakeDeadlineMs !== deadlineMs
    ) {
      return
    }
    const remainingMs = deadlineMs - Date.now()
    if (remainingMs > 0) {
      owner.wakeTimer = setTimeout(
        wake,
        Math.min(remainingMs, MAX_TIMER_DELAY_MS),
      )
      owner.wakeTimer.unref?.()
      return
    }
    owner.wakeTimer = null
    owner.wakeDeadlineMs = null
    void requestRunnerRecoveryDrain(owner)
  }
  owner.wakeTimer = setTimeout(
    wake,
    Math.min(Math.max(0, deadlineMs - Date.now()), MAX_TIMER_DELAY_MS),
  )
  owner.wakeTimer.unref?.()
}

async function claimRunnerTrigger(
  owner: MemoryRunnerRecoveryOwner,
  hint: { trigger_id: string; kind?: MemoryRunnerKind },
): Promise<ClaimedMemoryTurnEndTrigger | null> {
  const response = await memoryBridgeIpc.send(
    'memory.runner.claim',
    {
      ...leaderFencePayload(owner),
      trigger_id: hint.trigger_id,
      worker_id: MEMORY_RUNNER_WORKER_ID,
    },
    { timeout_ms: RUNNER_CONTROL_TIMEOUT_MS },
  )
  const receipt = parseRunnerReceipt('memory.runner.claim', response)
  if (!receipt.received) {
    logForDebugging(
      `[memory/turnEnd] claim rejected; trigger=${hint.trigger_id}; reason=${receipt.reason ?? 'unknown'}`,
    )
    return null
  }
  const trigger = (response as { trigger?: unknown }).trigger
  if (
    !isClaimedTrigger(trigger) ||
    trigger.trigger_id !== hint.trigger_id ||
    (hint.kind !== undefined && trigger.kind !== hint.kind) ||
    trigger.recovery.trigger_id !== trigger.trigger_id ||
    trigger.recovery.kind !== trigger.kind
  ) {
    throw new Error('memory.runner.claim returned an invalid trigger')
  }
  return trigger
}

function deliveryFence(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
): {
  leader_token: string
  leader_epoch: number
  trigger_id: string
  delivery_owner: string
  delivery_epoch: number
} {
  return {
    ...leaderFencePayload(owner),
    trigger_id: trigger.trigger_id,
    delivery_owner: trigger.delivery_owner,
    delivery_epoch: trigger.delivery_epoch,
  }
}

function rawDeliveryFence(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
): ReturnType<typeof deliveryFence> {
  return {
    ...rawLeaderFencePayload(owner),
    trigger_id: trigger.trigger_id,
    delivery_owner: trigger.delivery_owner,
    delivery_epoch: trigger.delivery_epoch,
  }
}

async function acknowledgeRunnerTrigger(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
): Promise<number> {
  const response = await memoryBridgeIpc.send(
    'memory.runner.ack',
    deliveryFence(owner, trigger),
    { timeout_ms: RUNNER_CONTROL_TIMEOUT_MS },
  )
  const receipt = parseRunnerLeaseReceipt('memory.runner.ack', response)
  if (!receipt.received) {
    throw new Error(
      `memory.runner.ack rejected trigger ${trigger.trigger_id}: ${receipt.reason ?? 'unknown'}`,
    )
  }
  return receipt.lease_expires_at_ms
}

function startRunnerLeaseRenewal(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
  acknowledgedLeaseExpiresAtMs: number,
): {
  stopAndWait(): Promise<void>
  assertNotFenced(): void
  revalidate(): Promise<void>
} {
  let stopped = false
  let renewalInFlight: Promise<void> | null = null
  let fencedReason: string | undefined
  let authoritativeDeadlineMs = acknowledgedLeaseExpiresAtMs

  const assertNotFenced = (): void => {
    assertRecoveryOwnerActive(owner)
    if (!fencedReason && Date.now() >= authoritativeDeadlineMs) {
      fencedReason = `authoritative lease expired at ${authoritativeDeadlineMs}`
    }
    if (fencedReason) {
      throw new Error(
        `memory runner delivery was fenced before a side effect: ${fencedReason}`,
      )
    }
  }

  const renew = async (): Promise<void> => {
    if (stopped || fencedReason || renewalInFlight) return
    try {
      assertRecoveryOwnerActive(owner)
    } catch (error) {
      fencedReason = error instanceof Error ? error.message : String(error)
      return
    }
    renewalInFlight = memoryBridgeIpc
      .send('memory.runner.renew', deliveryFence(owner, trigger), {
        timeout_ms: RUNNER_CONTROL_TIMEOUT_MS,
      })
      .then(response => {
        const receipt = parseRunnerLeaseReceipt(
          'memory.runner.renew',
          response,
        )
        if (!receipt.received) {
          fencedReason = receipt.reason ?? 'unknown'
          return
        }
        if (receipt.lease_expires_at_ms <= Date.now()) {
          fencedReason = `renewed lease was already expired at ${receipt.lease_expires_at_ms}`
          return
        }
        authoritativeDeadlineMs = receipt.lease_expires_at_ms
      })
      .catch(error => {
        // A transport error never invents a new deadline. The existing
        // authoritative deadline remains in force and assertNotFenced()
        // fails closed as soon as it elapses.
        logForDebugging(
          `[memory/turnEnd] lease renew transport failure; trigger=${trigger.trigger_id}: ${error instanceof Error ? error.message : String(error)}`,
        )
      })
      .finally(() => {
        renewalInFlight = null
      })
    await renewalInFlight
  }

  const timer = setInterval(() => {
    void renew()
  }, RUNNER_RENEW_INTERVAL_MS)
  timer.unref()

  return {
    async stopAndWait() {
      stopped = true
      clearInterval(timer)
      await renewalInFlight
    },
    assertNotFenced,
    async revalidate() {
      assertNotFenced()
      await renew()
      assertNotFenced()
    },
  }
}

function createRunnerExecutionContext(
  context: MemoryRunnerStopHookContext,
  renewal: ReturnType<typeof startRunnerLeaseRenewal>,
): MemoryRunnerStopHookContext {
  return {
    ...context,
    toolUseContext: {
      ...context.toolUseContext,
      // Durable Memory work has its own journal delivery authority and can
      // legally outlive the foreground TUI turn that enqueued it. Do not
      // inherit that foreground turn's fence; revalidate the live delivery
      // immediately before every model/hook/tool side-effect boundary instead.
      revalidateSideEffectAuthority: async () => renewal.revalidate(),
    },
  }
}

async function sendRunnerCompletion(
  owner: MemoryRunnerRecoveryOwner,
  payload: MemoryRunnerCompletedPayload,
  assertDeliveryAuthority: () => void,
): Promise<void> {
  let lastTransportError: unknown
  for (const delayMs of COMPLETION_RETRY_DELAYS_MS) {
    if (delayMs > 0) {
      await new Promise<void>(resolve => setTimeout(resolve, delayMs))
    }
    assertDeliveryAuthority()
    let response: unknown
    try {
      response = await memoryBridgeIpc.send(
        'memory.runner.completed',
        {
          ...leaderFencePayload(owner),
          ...payload,
        },
        { timeout_ms: RUNNER_CONTROL_TIMEOUT_MS },
      )
    } catch (error) {
      lastTransportError = error
      continue
    }
    const receipt = parseRunnerReceipt('memory.runner.completed', response)
    if (!receipt.received) {
      throw new Error(
        `memory.runner.completed rejected trigger ${payload.trigger_id}: ${receipt.reason ?? 'unknown'}`,
      )
    }
    return
  }
  throw (
    lastTransportError ??
    new Error(`memory.runner.completed failed for ${payload.trigger_id}`)
  )
}

function serializeRunnerError(
  error: unknown,
): MemoryRunnerCompletedPayload['error'] {
  const rawName = error instanceof Error ? error.name : 'Error'
  const name = /^[A-Za-z][A-Za-z0-9_.-]{0,63}$/.test(rawName)
    ? rawName
    : 'Error'
  return {
    // Completion is durable journal state, not a private foreground log.
    // Never persist provider/tool/path payloads from an arbitrary Error.
    message: 'memory runner execution failed',
    name,
  }
}

async function releaseRunnerTrigger(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
  reasonCode: string,
): Promise<void> {
  const response = await memoryBridgeIpc.send(
    'memory.runner.release',
    {
      ...rawDeliveryFence(owner, trigger),
      reason_code: reasonCode,
    },
    { timeout_ms: RUNNER_CONTROL_TIMEOUT_MS },
  )
  const receipt = parseRunnerReleaseReceipt(response)
  if (receipt.received) {
    scheduleRunnerRecoveryDrainAt(owner, receipt.next_attempt_at_ms)
  } else {
    logForDebugging(
      `[memory/turnEnd] release not accepted; trigger=${trigger.trigger_id}; reason=${receipt.reason ?? 'unknown'}`,
    )
  }
}

async function deadLetterRunnerTrigger(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
  reasonCode: string,
): Promise<void> {
  const response = await memoryBridgeIpc.send(
    'memory.runner.dead_letter',
    {
      ...rawDeliveryFence(owner, trigger),
      reason_code: reasonCode,
    },
    { timeout_ms: RUNNER_CONTROL_TIMEOUT_MS },
  )
  const receipt = parseRunnerReceipt('memory.runner.dead_letter', response)
  if (
    !receipt.received &&
    receipt.reason !== 'already_dead_lettered' &&
    receipt.reason !== 'missing'
  ) {
    logForDebugging(
      `[memory/turnEnd] dead-letter not accepted; trigger=${trigger.trigger_id}; reason=${receipt.reason ?? 'unknown'}`,
    )
  }
}

async function runClaimedTriggerWithContext(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
  renewal: ReturnType<typeof startRunnerLeaseRenewal>,
  context: MemoryRunnerStopHookContext,
  appendSystemMessage?: AppendSystemMessage,
): Promise<MemoryTriggerResult | void> {
  assertRecoveryOwnerActive(owner)
  renewal.assertNotFenced()
  const runnerContext = createRunnerExecutionContext(context, renewal)
  return triggerRunner({
    trigger,
    context: runnerContext,
    appendSystemMessage,
    cacheSafeParams: createStopHookCacheSafeParams(runnerContext),
    assertDeliveryAuthority: renewal.assertNotFenced,
  })
}

async function executeClaimedRunnerTrigger(
  owner: MemoryRunnerRecoveryOwner,
  trigger: ClaimedMemoryTurnEndTrigger,
  live?: {
    context: MemoryRunnerStopHookContext
    appendSystemMessage?: AppendSystemMessage
  },
): Promise<void> {
  let acknowledgedDeadline: number
  try {
    acknowledgedDeadline = await acknowledgeRunnerTrigger(owner, trigger)
  } catch (error) {
    await releaseRunnerTrigger(owner, trigger, 'ack_failed')
    throw error
  }
  const renewal = startRunnerLeaseRenewal(
    owner,
    trigger,
    acknowledgedDeadline,
  )
  let contextEntered = false
  let result: MemoryTriggerResult | void = undefined
  let runnerError: unknown
  try {
    try {
      if (live) {
        contextEntered = true
        result = await runClaimedTriggerWithContext(
          owner,
          trigger,
          renewal,
          live.context,
          live.appendSystemMessage,
        )
      } else {
        result = await withAuthoritativeMemoryRecoveryContext(
          trigger.recovery,
          trigger.trigger_id,
          trigger.kind,
          owner.abortController.signal,
          async context => {
            contextEntered = true
            return runClaimedTriggerWithContext(
              owner,
              trigger,
              renewal,
              context,
            )
          },
        )
      }
    } catch (error) {
      if (!contextEntered) {
        throw error
      }
      runnerError = error
    }

    if (
      owner.abortController.signal.aborted ||
      recoveryOwner !== owner ||
      !owner.accepting
    ) {
      await renewal.stopAndWait()
      await releaseRunnerTrigger(owner, trigger, 'leader_stopped')
      return
    }

    const completedPayload: MemoryRunnerCompletedPayload = runnerError
      ? {
          trigger_id: trigger.trigger_id,
          kind: trigger.kind,
          written_paths: [],
          error: serializeRunnerError(runnerError),
          delivery_owner: trigger.delivery_owner,
          delivery_epoch: trigger.delivery_epoch,
        }
      : {
          trigger_id: trigger.trigger_id,
          kind: trigger.kind,
          written_paths: result?.writtenPaths ?? [],
          delivery_owner: trigger.delivery_owner,
          delivery_epoch: trigger.delivery_epoch,
          ...(result?.usage ? { usage: result.usage } : {}),
        }
    renewal.assertNotFenced()
    await sendRunnerCompletion(
      owner,
      completedPayload,
      renewal.assertNotFenced,
    )
  } catch (error) {
    await renewal.stopAndWait()
    if (error instanceof MemoryRecoveryContextError) {
      if (error.disposition === 'dead_letter') {
        await deadLetterRunnerTrigger(owner, trigger, error.code)
      } else {
        await releaseRunnerTrigger(owner, trigger, error.code)
      }
      return
    }
    await releaseRunnerTrigger(
      owner,
      trigger,
      owner.abortController.signal.aborted
        ? 'leader_stopped'
        : 'runner_control_error',
    )
    if (owner.abortController.signal.aborted) return
    throw error
  } finally {
    await renewal.stopAndWait()
  }
}

function extractTriggers(response: unknown): MemoryTurnEndTrigger[] {
  const raw =
    response !== null &&
    typeof response === 'object' &&
    'triggers' in response
      ? (response as { triggers?: unknown }).triggers
      : response
  return Array.isArray(raw) ? raw.filter(isTrigger) : []
}

function isRunnerCandidate(value: unknown): value is MemoryRunnerCandidate {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return false
  }
  const candidate = value as Partial<
    MemoryTurnEndTrigger & {
      recovery: MemoryRecoveryLocator
      invalid_reason: string
    }
  >
  if (
    typeof candidate.trigger_id === 'string' &&
    candidate.invalid_reason === 'invalid_recovery_locator'
  ) {
    return true
  }
  return (
    isTrigger(value) &&
    isRecoveryLocator(candidate.recovery) &&
    candidate.recovery.trigger_id === candidate.trigger_id &&
    candidate.recovery.kind === candidate.kind
  )
}

async function fetchRunnerCandidates(
  owner: MemoryRunnerRecoveryOwner,
): Promise<{
  candidates: MemoryRunnerCandidate[]
  hasMore: boolean
}> {
  const response = await memoryBridgeIpc.send(
    'memory.runner.candidates',
    {
      ...leaderFencePayload(owner),
      limit: RUNNER_CANDIDATE_LIMIT,
    },
    { timeout_ms: RUNNER_CONTROL_TIMEOUT_MS },
  )
  if (
    response === null ||
    typeof response !== 'object' ||
    !Array.isArray((response as { candidates?: unknown }).candidates) ||
    typeof (response as { has_more?: unknown }).has_more !== 'boolean' ||
    (response as { limit?: unknown }).limit !== RUNNER_CANDIDATE_LIMIT
  ) {
    throw new Error('memory.runner.candidates returned an invalid snapshot')
  }
  const candidates = (
    response as { candidates: unknown[] }
  ).candidates
  if (!candidates.every(isRunnerCandidate)) {
    throw new Error(
      'memory.runner.candidates returned an invalid recovery candidate',
    )
  }
  return {
    candidates,
    hasMore: (response as { has_more: boolean }).has_more,
  }
}

async function executeRunnerHint(
  owner: MemoryRunnerRecoveryOwner,
  hint: { trigger_id: string; kind?: MemoryRunnerKind },
  live?: {
    context: MemoryRunnerStopHookContext
    appendSystemMessage?: AppendSystemMessage
  },
): Promise<void> {
  if (!owner.accepting || owner.abortController.signal.aborted) return
  const trigger = await claimRunnerTrigger(owner, hint)
  if (!trigger) return
  await executeClaimedRunnerTrigger(owner, trigger, live)
}

async function drainRunnerCandidateBatch(
  owner: MemoryRunnerRecoveryOwner,
): Promise<boolean> {
  const snapshot = await fetchRunnerCandidates(owner)
  for (const candidate of snapshot.candidates) {
    if (!owner.accepting || owner.abortController.signal.aborted) break
    if ('invalid_reason' in candidate) {
      // Claiming a poison candidate grants a fenced delivery only long enough
      // for Rust to atomically dead-letter it; the response never authorizes
      // TS execution.
      await executeRunnerHint(owner, {
        trigger_id: candidate.trigger_id,
      })
      continue
    }
    await executeRunnerHint(owner, {
      trigger_id: candidate.trigger_id,
      kind: candidate.kind,
    })
  }
  return snapshot.hasMore
}

function requestRunnerRecoveryDrain(
  owner: MemoryRunnerRecoveryOwner,
): Promise<void> {
  if (
    recoveryOwner !== owner ||
    !owner.accepting ||
    owner.abortController.signal.aborted
  ) {
    return Promise.resolve()
  }
  owner.drainRequested = true
  if (owner.drainPromise) return owner.drainPromise
  const drain = (async () => {
    while (
      recoveryOwner === owner &&
      owner.accepting &&
      !owner.abortController.signal.aborted &&
      owner.drainRequested
    ) {
      owner.drainRequested = false
      const hasMore = await drainRunnerCandidateBatch(owner)
      if (hasMore) {
        owner.drainRequested = true
      }
    }
  })()
    .catch(error => {
      if (!owner.abortController.signal.aborted) {
        const name =
          error instanceof Error &&
          /^[A-Za-z][A-Za-z0-9_.-]{0,63}$/.test(error.name)
            ? error.name
            : 'Error'
        logForDebugging(
          `[memory/turnEnd] durable recovery drain paused (${name})`,
        )
      }
    })
    .finally(() => {
      if (owner.drainPromise === drain) {
        owner.drainPromise = null
      }
    })
  owner.drainPromise = drain
  return drain
}

function trackOwnerTask(
  owner: MemoryRunnerRecoveryOwner,
  task: Promise<void>,
): Promise<void> {
  owner.tasks.add(task)
  task.finally(() => owner.tasks.delete(task)).catch(() => {})
  return task
}

async function executeLiveRunnerHints(
  owner: MemoryRunnerRecoveryOwner,
  hints: MemoryTurnEndTrigger[],
  context: MemoryRunnerStopHookContext,
  appendSystemMessage?: AppendSystemMessage,
): Promise<void> {
  const task = (async () => {
    for (const hint of hints) {
      if (!owner.accepting || owner.abortController.signal.aborted) return
      await executeRunnerHint(
        owner,
        { trigger_id: hint.trigger_id, kind: hint.kind },
        { context, appendSystemMessage },
      )
    }
  })()
  await trackOwnerTask(owner, task)
}

/**
 * Installs the sole durable Memory runner consumer for one proven leader
 * epoch. Registration is synchronous so a concurrent turn-end wake cannot
 * slip between leader activation and startup recovery.
 */
export function startDurableMemoryRunnerRecovery(
  fence: MemoryRunnerLeaderFence,
): Promise<void> {
  validateLeaderFence(fence)
  if (recoveryOwner) {
    void stopDurableMemoryRunnerRecovery('leader_replaced')
  }
  const owner: MemoryRunnerRecoveryOwner = {
    fence,
    abortController: new AbortController(),
    accepting: true,
    tasks: new Set(),
    drainPromise: null,
    drainRequested: false,
    wakeTimer: null,
    wakeDeadlineMs: null,
  }
  recoveryOwner = owner
  return requestRunnerRecoveryDrain(owner)
}

/**
 * Closes admission and aborts recovered tool/model work synchronously, then
 * waits for every claimed delivery to complete or be released before the
 * caller gives up the leader token.
 */
export async function stopDurableMemoryRunnerRecovery(
  reason = 'leader_stopped',
): Promise<void> {
  const owner = recoveryOwner
  if (!owner) return
  owner.accepting = false
  recoveryOwner = null
  if (!owner.abortController.signal.aborted) {
    owner.abortController.abort(new Error(reason))
  }
  if (owner.wakeTimer) {
    clearTimeout(owner.wakeTimer)
    owner.wakeTimer = null
    owner.wakeDeadlineMs = null
  }
  const pending = [
    ...(owner.drainPromise ? [owner.drainPromise] : []),
    ...owner.tasks,
  ]
  if (pending.length > 0) {
    await Promise.allSettled(pending)
  }
}

/**
 * Rechecks the durable journal while retaining the current leader epoch.
 * The leader renewal tick calls this even when the enqueue happened in a
 * different follower process and no in-process wake hint could be delivered.
 */
export function wakeDurableMemoryRunnerRecovery(): Promise<void> {
  const owner = recoveryOwner
  return owner ? requestRunnerRecoveryDrain(owner) : Promise.resolve()
}

async function flushTurnEnd(
  context: MemoryRunnerStopHookContext,
  pending: PendingTurnEnd,
): Promise<void> {
  pendingByContext.delete(context)
  const requestedKinds = normalizeKinds([...pending.kinds])
  if (requestedKinds.length === 0) return

  let triggers: MemoryTurnEndTrigger[]
  try {
    // The durable row stores an exact transcript locator. Flush the current
    // session inside its AsyncLocalStorage scope before enqueue can commit so
    // every returned trigger is recoverable even if this process exits before
    // it receives or executes the wake hint.
    await flushSessionStorage()
    const response = await memoryBridgeIpc.send(
      'memory.turn_end.evaluate',
      buildEvaluatePayload(context, requestedKinds),
      { timeout_ms: EVALUATE_TIMEOUT_MS },
    )
    triggers = extractTriggers(response)
  } catch (e) {
    // W-MEMORY-EVOLUTION FIX #13 (2026-06-01) — make cold-start drops
    // observable. The first `memory.turn_end.evaluate` of a process used to
    // race the orchestrator's synchronous initial SE index pass and time out
    // (EVALUATE_TIMEOUT_MS), then get silently swallowed here. The Rust side
    // now runs that index in the background so this should be rare, but a
    // dropped evaluate (timeout or transport error) must be logged so it is
    // never silent again. Control flow is unchanged (still return).
    logForDebugging(
      `[memory/turnEnd] evaluate dropped (timeout_ms=${EVALUATE_TIMEOUT_MS}); kinds=${requestedKinds.join(
        ',',
      )}: ${e instanceof Error ? e.message : String(e)}`,
    )
    const owner = recoveryOwner
    if (owner) {
      await requestRunnerRecoveryDrain(owner)
    }
    return
  }

  const matchingTriggers = triggers.filter(hint =>
    requestedKinds.includes(hint.kind),
  )
  const owner = recoveryOwner
  if (
    owner &&
    owner.accepting &&
    !owner.abortController.signal.aborted &&
    matchingTriggers.length > 0
  ) {
    await executeLiveRunnerHints(
      owner,
      matchingTriggers,
      context,
      pending.appendSystemMessage,
    )
    await requestRunnerRecoveryDrain(owner)
  } else if (owner) {
    // Evaluate may have committed work but returned no usable wake hints.
    // Enumerating the durable journal closes that response-loss/malformed-hint
    // window without granting the foreground turn any execution authority.
    await requestRunnerRecoveryDrain(owner)
  }
}

export function onTurnEnd(
  context: MemoryRunnerStopHookContext,
  appendSystemMessage: AppendSystemMessage | undefined,
  requestedKinds: readonly MemoryRunnerKind[],
): Promise<void> {
  const kinds = normalizeKinds(requestedKinds)
  if (kinds.length === 0) return Promise.resolve()

  let pending = pendingByContext.get(context)
  if (!pending) {
    pending = {
      kinds: new Set<MemoryRunnerKind>(),
      appendSystemMessage,
      promise: Promise.resolve(),
    }
    pending.promise = Promise.resolve()
      .then(() => flushTurnEnd(context, pending!))
      .finally(() => {
        inFlight.delete(pending!.promise)
      })
    pendingByContext.set(context, pending)
    inFlight.add(pending.promise)
  }

  for (const kind of kinds) {
    pending.kinds.add(kind)
  }
  if (appendSystemMessage) {
    pending.appendSystemMessage = appendSystemMessage
  }

  return pending.promise
}

export async function drainPendingMemoryRunner(
  timeoutMs = 60_000,
): Promise<void> {
  const owner = recoveryOwner
  const pending = [
    ...inFlight,
    ...(owner?.drainPromise ? [owner.drainPromise] : []),
    ...(owner ? owner.tasks : []),
  ]
  if (pending.length === 0) return
  await Promise.race([
    Promise.allSettled(pending).then(() => {}),
    // eslint-disable-next-line no-restricted-syntax -- mirrors existing extraction drain behavior
    new Promise<void>(r => setTimeout(r, timeoutMs).unref()),
  ])
}

export function configureMemoryRunnersForTesting(options?: {
  triggerRunner?: MemoryTriggerRunner
  leaderFence?: MemoryRunnerLeaderFence
}): void {
  const owner = recoveryOwner
  if (owner) {
    owner.accepting = false
    recoveryOwner = null
    if (!owner.abortController.signal.aborted) {
      owner.abortController.abort(new Error('test_reset'))
    }
    if (owner.wakeTimer) {
      clearTimeout(owner.wakeTimer)
      owner.wakeTimer = null
      owner.wakeDeadlineMs = null
    }
  }
  triggerRunner = options?.triggerRunner ?? dispatchMemoryTrigger
  pendingByContext = new WeakMap<MemoryRunnerStopHookContext, PendingTurnEnd>()
  inFlight = new Set<Promise<void>>()
  if (options?.triggerRunner || options?.leaderFence) {
    void startDurableMemoryRunnerRecovery(
      options.leaderFence ?? {
        leaderToken: '00000000-0000-4000-8000-000000000000',
        leaderEpoch: 1,
      },
    )
  }
}
