/** Client for the TUI-owned memory sidecar process. */
import { randomUUID } from 'crypto'
import { createConnection, type Socket } from 'net'

export type MemoryBridgeOptions = {
  timeout_ms?: number
}

export type MemoryBridgeDriver = {
  send(
    method: string,
    payload?: unknown,
    opts?: MemoryBridgeOptions,
  ): Promise<unknown>
  fire?(method: string, payload?: unknown): void
  close?(): void
  isAvailable?(): boolean
}

type Endpoint =
  | { kind: 'unix'; path: string }
  | { kind: 'npipe'; path: string }

type MethodMode = 'request' | 'notification'

type MethodSpec = {
  mode: MethodMode
  validatePayload: (payload: unknown) => string | null
  validateResponse?: (payload: unknown) => string | null
}

type PendingRequest = {
  method: string
  resolve: (value: unknown) => void
  reject: (error: Error) => void
  socket: Socket
  timer: ReturnType<typeof setTimeout>
}

type MemoryBridgeSocketFactory = (endpoint: Endpoint) => Socket

type WireRequest = {
  id?: string
  method: string
  payload: unknown
}

const DEFAULT_TIMEOUT_MS = 250
const NOTIFICATION_TIMEOUT_MS = 250
const MEMORY_IPC_ENDPOINT_ENV = 'CRABCODE_MEMORY_IPC_ENDPOINT'

let activeDriver: MemoryBridgeDriver | null = null
let socketFactoryForTesting: MemoryBridgeSocketFactory | null = null

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function hasOwn(value: Record<string, unknown>, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(value, key)
}

function normalizePayload(payload: unknown): unknown {
  return payload === undefined ? {} : payload
}

function validateObject(
  value: unknown,
  required: readonly string[],
  optional: readonly string[],
): Record<string, unknown> | string {
  if (!isRecord(value)) return 'payload must be an object'

  const allowed = new Set([...required, ...optional])
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) return `unexpected property: ${key}`
  }
  for (const key of required) {
    if (!hasOwn(value, key)) return `missing required property: ${key}`
  }
  return value
}

function expectString(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  return typeof obj[key] === 'string' ? null : `${key} must be a string`
}

function expectNonEmptyString(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  const typeError = expectString(obj, key, optional)
  if (typeError) return typeError
  if (optional && !hasOwn(obj, key)) return null
  return (obj[key] as string).length > 0
    ? null
    : `${key} must be a non-empty string`
}

function expectBoolean(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  return typeof obj[key] === 'boolean' ? null : `${key} must be a boolean`
}

function expectNumber(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  return typeof obj[key] === 'number' && Number.isFinite(obj[key])
    ? null
    : `${key} must be a finite number`
}

function expectNonNegativeInteger(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  return typeof obj[key] === 'number' &&
    Number.isSafeInteger(obj[key]) &&
    (obj[key] as number) >= 0
    ? null
    : `${key} must be a non-negative safe integer`
}

function expectPositiveSafeInteger(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  return typeof obj[key] === 'number' &&
    Number.isSafeInteger(obj[key]) &&
    (obj[key] as number) > 0
    ? null
    : `${key} must be a positive safe integer`
}

function expectNonZeroU32(
  obj: Record<string, unknown>,
  key: string,
): string | null {
  return typeof obj[key] === 'number' &&
    Number.isSafeInteger(obj[key]) &&
    (obj[key] as number) > 0 &&
    (obj[key] as number) <= 0xffff_ffff
    ? null
    : `${key} must be a non-zero u32 integer`
}

function expectNullableNumber(
  obj: Record<string, unknown>,
  key: string,
): string | null {
  return obj[key] === null || (typeof obj[key] === 'number' && Number.isFinite(obj[key]))
    ? null
    : `${key} must be a finite number or null`
}

function expectNullableNonNegativeInteger(
  obj: Record<string, unknown>,
  key: string,
): string | null {
  return obj[key] === null ? null : expectNonNegativeInteger(obj, key)
}

function expectNullableBoolean(
  obj: Record<string, unknown>,
  key: string,
): string | null {
  return obj[key] === null || typeof obj[key] === 'boolean'
    ? null
    : `${key} must be a boolean or null`
}

function expectStringArray(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  const value = obj[key]
  return Array.isArray(value) && value.every(item => typeof item === 'string')
    ? null
    : `${key} must be an array of strings`
}

function expectObject(
  obj: Record<string, unknown>,
  key: string,
  optional = false,
): string | null {
  if (optional && !hasOwn(obj, key)) return null
  return isRecord(obj[key]) ? null : `${key} must be an object`
}

function validateEmptyObject(payload: unknown): string | null {
  const obj = validateObject(normalizePayload(payload), [], [])
  return typeof obj === 'string' ? obj : null
}

function validateStringArrayObject(
  payload: unknown,
  key: string,
): string | null {
  const obj = validateObject(normalizePayload(payload), [key], [])
  if (typeof obj === 'string') return obj
  return expectStringArray(obj, key)
}

function validateOkResponse(payload: unknown): string | null {
  const obj = validateObject(normalizePayload(payload), ['ok'], [])
  if (typeof obj === 'string') return obj
  return expectBoolean(obj, 'ok')
}

function validateEnabledResponse(payload: unknown): string | null {
  const obj = validateObject(normalizePayload(payload), ['enabled'], [])
  if (typeof obj === 'string') return obj
  return expectBoolean(obj, 'enabled')
}

function validateKind(value: unknown): boolean {
  return value === 'dream' || value === 'extract'
}

function validateRecoveryLocator(
  value: unknown,
  expectedTriggerId?: string,
  expectedKind?: 'dream' | 'extract',
): string | null {
  const obj = validateObject(
    value,
    [
      'recovery_schema_version',
      'trigger_id',
      'kind',
      'session_id',
      'current_session_id',
      'context_leaf_uuid',
      'project_cwd',
      'transcript_path',
      'project_state_dir',
      'memory_dir',
    ],
    [],
  )
  if (typeof obj === 'string') return obj
  if (obj.recovery_schema_version !== 1) {
    return 'recovery_schema_version must be 1'
  }
  if (!validateKind(obj.kind)) return 'kind must be dream or extract'
  for (const key of [
    'trigger_id',
    'session_id',
    'current_session_id',
    'context_leaf_uuid',
    'project_cwd',
    'transcript_path',
    'project_state_dir',
    'memory_dir',
  ]) {
    const error = expectString(obj, key)
    if (error) return error
  }
  if (expectedTriggerId !== undefined && obj.trigger_id !== expectedTriggerId) {
    return 'trigger_id does not match the containing trigger'
  }
  if (expectedKind !== undefined && obj.kind !== expectedKind) {
    return 'kind does not match the containing trigger'
  }
  return null
}

function validateTrigger(
  value: unknown,
  expectedKind?: 'dream' | 'extract',
  claimed = false,
): string | null {
  const deliveryFenceFields = [
    'delivery_owner',
    'delivery_epoch',
    'lease_expires_at_ms',
  ] as const
  const obj = validateObject(
    value,
    [
      'trigger_id',
      'kind',
      'runner_payload',
      ...(claimed ? ['recovery'] : []),
      ...(claimed ? deliveryFenceFields : []),
    ],
    ['lock_token'],
  )
  if (typeof obj === 'string') return obj
  if (expectString(obj, 'trigger_id')) return 'trigger_id must be a string'
  if (!validateKind(obj.kind)) return 'kind must be dream or extract'
  if (expectedKind && obj.kind !== expectedKind) {
    return `kind must be ${expectedKind}`
  }
  if (expectString(obj, 'lock_token', true)) return 'lock_token must be a string'
  if (claimed && expectString(obj, 'delivery_owner')) {
    return 'delivery_owner must be a string'
  }
  if (claimed && expectNonNegativeInteger(obj, 'delivery_epoch')) {
    return 'delivery_epoch must be a non-negative safe integer'
  }
  if (claimed && expectNonNegativeInteger(obj, 'lease_expires_at_ms')) {
    return 'lease_expires_at_ms must be a non-negative safe integer'
  }
  if (expectObject(obj, 'runner_payload')) return 'runner_payload must be an object'
  if (claimed) {
    const recoveryError = validateRecoveryLocator(
      obj.recovery,
      obj.trigger_id as string,
      obj.kind as 'dream' | 'extract',
    )
    if (recoveryError) return `recovery.${recoveryError}`
  }
  return null
}

function validateTriggerArrayResponse(
  payload: unknown,
  expectedKind?: 'dream' | 'extract',
): string | null {
  const obj = validateObject(normalizePayload(payload), ['triggers'], [])
  if (typeof obj === 'string') return obj
  const triggers = obj.triggers
  if (!Array.isArray(triggers)) return 'triggers must be an array'
  for (const trigger of triggers) {
    const error = validateTrigger(trigger, expectedKind)
    if (error) return error
  }
  return null
}

function validateTurnEndPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [
      'recovery_schema_version',
      'session_id',
      'current_session_id',
      'last_assistant_uuid',
      'project_cwd',
      'transcript_path',
      'memory_dir',
      'message_counts',
      'feature_flags',
    ],
    ['team_memory_dir', 'requested_kinds'],
  )
  if (typeof obj === 'string') return obj
  if (obj.recovery_schema_version !== 1) {
    return 'recovery_schema_version must be 1'
  }
  for (const key of [
    'session_id',
    'current_session_id',
    'last_assistant_uuid',
    'project_cwd',
    'transcript_path',
    'memory_dir',
  ]) {
    const error = expectString(obj, key)
    if (error) return error
  }
  if (expectString(obj, 'team_memory_dir', true)) {
    return 'team_memory_dir must be a string'
  }
  if (!isRecord(obj.message_counts)) return 'message_counts must be an object'
  for (const [key, value] of Object.entries(obj.message_counts)) {
    if (typeof value !== 'number' || !Number.isInteger(value) || value < 0) {
      return `message_counts.${key} must be a non-negative integer`
    }
  }
  if (!isRecord(obj.feature_flags)) return 'feature_flags must be an object'
  for (const [key, value] of Object.entries(obj.feature_flags)) {
    if (typeof value !== 'boolean') {
      return `feature_flags.${key} must be a boolean`
    }
  }
  if (hasOwn(obj, 'requested_kinds')) {
    const value = obj.requested_kinds
    if (
      !Array.isArray(value) ||
      !value.every(validateKind) ||
      new Set(value).size !== value.length
    ) {
      return 'requested_kinds must be a unique array of dream/extract'
    }
  }
  return null
}

function validateRunnerCompletedPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [
      'leader_token',
      'leader_epoch',
      'trigger_id',
      'kind',
      'written_paths',
      'delivery_owner',
      'delivery_epoch',
    ],
    ['usage', 'error', 'completed_at_ms'],
  )
  if (typeof obj === 'string') return obj
  if (expectNonEmptyString(obj, 'leader_token')) {
    return 'leader_token must be a non-empty string'
  }
  if (expectPositiveSafeInteger(obj, 'leader_epoch')) {
    return 'leader_epoch must be a positive safe integer'
  }
  if (expectString(obj, 'trigger_id')) return 'trigger_id must be a string'
  if (expectString(obj, 'delivery_owner')) {
    return 'delivery_owner must be a string'
  }
  if (expectNonNegativeInteger(obj, 'delivery_epoch')) {
    return 'delivery_epoch must be a non-negative safe integer'
  }
  if (!validateKind(obj.kind)) return 'kind must be dream or extract'
  const writtenPathsError = expectStringArray(obj, 'written_paths')
  if (writtenPathsError) return writtenPathsError
  if (expectObject(obj, 'usage', true)) return 'usage must be an object'
  if (expectNonNegativeInteger(obj, 'completed_at_ms', true)) {
    return 'completed_at_ms must be a non-negative safe integer'
  }
  if (hasOwn(obj, 'error')) {
    if (!isRecord(obj.error)) return 'error must be an object'
    if (expectString(obj.error, 'message')) return 'error.message must be a string'
    if (expectString(obj.error, 'name', true)) return 'error.name must be a string'
  }
  return null
}

function validateRunnerClaimPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['leader_token', 'leader_epoch', 'trigger_id', 'worker_id'],
    [],
  )
  if (typeof obj === 'string') return obj
  return (
    expectNonEmptyString(obj, 'leader_token') ??
    expectPositiveSafeInteger(obj, 'leader_epoch') ??
    expectString(obj, 'trigger_id') ??
    expectString(obj, 'worker_id')
  )
}

function validateRunnerClaimResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['received'],
    ['reason', 'trigger'],
  )
  if (typeof obj === 'string') return obj
  const receivedError = expectBoolean(obj, 'received')
  if (receivedError) return receivedError
  if (expectString(obj, 'reason', true)) return 'reason must be a string'
  if (obj.received === true) {
    if (hasOwn(obj, 'reason')) {
      return 'reason must be absent when received=true'
    }
    if (!hasOwn(obj, 'trigger')) return 'trigger is required when received is true'
    const triggerError = validateTrigger(obj.trigger, undefined, true)
    if (triggerError) return `trigger.${triggerError}`
    return null
  }
  if (hasOwn(obj, 'trigger')) {
    return 'trigger must be absent when received=false'
  }
  return typeof obj.reason === 'string' && obj.reason.length > 0
    ? null
    : 'received=false requires a non-empty reason'
}

function validateRunnerFencePayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [
      'leader_token',
      'leader_epoch',
      'trigger_id',
      'delivery_owner',
      'delivery_epoch',
    ],
    [],
  )
  if (typeof obj === 'string') return obj
  return (
    expectNonEmptyString(obj, 'leader_token') ??
    expectPositiveSafeInteger(obj, 'leader_epoch') ??
    expectString(obj, 'trigger_id') ??
    expectString(obj, 'delivery_owner') ??
    expectNonNegativeInteger(obj, 'delivery_epoch')
  )
}

function validateRunnerCandidatesPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['leader_token', 'leader_epoch'],
    ['limit'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectNonEmptyString(obj, 'leader_token') ??
    expectPositiveSafeInteger(obj, 'leader_epoch') ??
    expectPositiveSafeInteger(obj, 'limit', true)
  )
}

function validateRunnerCandidate(value: unknown): string | null {
  if (isRecord(value) && hasOwn(value, 'invalid_reason')) {
    const poison = validateObject(
      value,
      ['trigger_id', 'invalid_reason'],
      [],
    )
    if (typeof poison === 'string') return poison
    if (expectString(poison, 'trigger_id')) {
      return 'trigger_id must be a string'
    }
    return poison.invalid_reason === 'invalid_recovery_locator'
      ? null
      : 'invalid_reason must be invalid_recovery_locator'
  }
  const obj = validateObject(
    value,
    ['trigger_id', 'kind', 'runner_payload', 'recovery'],
    ['lock_token'],
  )
  if (typeof obj === 'string') return obj
  if (expectString(obj, 'trigger_id')) return 'trigger_id must be a string'
  if (!validateKind(obj.kind)) return 'kind must be dream or extract'
  if (expectString(obj, 'lock_token', true)) {
    return 'lock_token must be a string'
  }
  if (expectObject(obj, 'runner_payload')) {
    return 'runner_payload must be an object'
  }
  const recoveryError = validateRecoveryLocator(
    obj.recovery,
    obj.trigger_id as string,
    obj.kind as 'dream' | 'extract',
  )
  return recoveryError ? `recovery.${recoveryError}` : null
}

function validateRunnerCandidatesResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['candidates', 'has_more', 'limit'],
    [],
  )
  if (typeof obj === 'string') return obj
  if (!Array.isArray(obj.candidates)) {
    return 'candidates must be an array'
  }
  for (const candidate of obj.candidates) {
    const error = validateRunnerCandidate(candidate)
    if (error) return `candidate.${error}`
  }
  const fieldError =
    expectBoolean(obj, 'has_more') ??
    expectPositiveSafeInteger(obj, 'limit')
  if (fieldError) return fieldError
  if ((obj.limit as number) > 128) {
    return 'limit must not exceed the Rust server maximum of 128'
  }
  return obj.candidates.length <= (obj.limit as number)
    ? null
    : 'candidates length must not exceed limit'
}

function validateReasonCode(
  obj: Record<string, unknown>,
  key = 'reason_code',
): string | null {
  const error = expectString(obj, key)
  if (error) return error
  return /^[a-z0-9._-]{1,64}$/.test(obj[key] as string)
    ? null
    : `${key} must match [a-z0-9._-]{1,64}`
}

function validateRunnerReleasePayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [
      'leader_token',
      'leader_epoch',
      'trigger_id',
      'delivery_owner',
      'delivery_epoch',
      'reason_code',
    ],
    [],
  )
  if (typeof obj === 'string') return obj
  return (
    expectNonEmptyString(obj, 'leader_token') ??
    expectPositiveSafeInteger(obj, 'leader_epoch') ??
    expectString(obj, 'trigger_id') ??
    expectString(obj, 'delivery_owner') ??
    expectNonNegativeInteger(obj, 'delivery_epoch') ??
    validateReasonCode(obj)
  )
}

function validateRunnerReleaseResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['received', 'next_attempt_at_ms'],
    ['reason'],
  )
  if (typeof obj === 'string') return obj
  const error =
    expectBoolean(obj, 'received') ??
    expectString(obj, 'reason', true) ??
    expectNullableNonNegativeInteger(obj, 'next_attempt_at_ms')
  if (error) return error
  if (obj.received === true) {
    if (obj.reason !== undefined) {
      return 'reason must be absent when received=true'
    }
    return obj.next_attempt_at_ms !== null
      ? null
      : 'next_attempt_at_ms must be an integer when received=true'
  }
  return obj.next_attempt_at_ms === null &&
    typeof obj.reason === 'string' &&
    obj.reason.length > 0
    ? null
    : 'received=false requires reason and next_attempt_at_ms=null'
}

function validateRunnerDeadLetterResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['received'],
    ['reason'],
  )
  if (typeof obj === 'string') return obj
  const error =
    expectBoolean(obj, 'received') ?? expectString(obj, 'reason', true)
  if (error) return error
  if (obj.received === true) {
    return obj.reason === undefined
      ? null
      : 'reason must be absent when received=true'
  }
  return typeof obj.reason === 'string' && obj.reason.length > 0
    ? null
    : 'received=false requires a non-empty reason'
}

function validateRunnerLeaseResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['received', 'lease_expires_at_ms'],
    ['reason'],
  )
  if (typeof obj === 'string') return obj
  const fieldError =
    expectBoolean(obj, 'received') ??
    expectString(obj, 'reason', true) ??
    expectNullableNonNegativeInteger(obj, 'lease_expires_at_ms')
  if (fieldError) return fieldError
  if (obj.received === true) {
    if (
      !Number.isSafeInteger(obj.lease_expires_at_ms) ||
      (obj.lease_expires_at_ms as number) <= 0
    ) {
      return 'lease_expires_at_ms must be a positive safe integer when received=true'
    }
    if (obj.reason !== undefined) {
      return 'reason must be absent when received=true'
    }
  } else {
    if (obj.lease_expires_at_ms !== null) {
      return 'lease_expires_at_ms must be null when received=false'
    }
    if (typeof obj.reason !== 'string' || obj.reason.length === 0) {
      return 'reason must be a non-empty string when received=false'
    }
  }
  return null
}

function validateRunnerCompletionResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [],
    [
      'received',
      'ok',
      'reason',
      'settled',
      'known_trigger',
      'lock_released',
      'rolled_back',
      'cursor_updated',
      'indexed_path_count',
    ],
  )
  if (typeof obj === 'string') return obj
  if (hasOwn(obj, 'ok')) {
    const legacyFieldError =
      expectBoolean(obj, 'ok') ??
      expectBoolean(obj, 'known_trigger') ??
      expectBoolean(obj, 'lock_released') ??
      expectBoolean(obj, 'rolled_back') ??
      expectBoolean(obj, 'cursor_updated') ??
      expectNonNegativeInteger(obj, 'indexed_path_count')
    if (legacyFieldError) return legacyFieldError
    if (obj.ok !== true) return 'ok must be true'
    if (
      hasOwn(obj, 'received') ||
      hasOwn(obj, 'reason') ||
      hasOwn(obj, 'settled')
    ) {
      return 'legacy ok response must not contain journal receipt fields'
    }
    return null
  }
  if (!hasOwn(obj, 'received')) {
    return 'missing required property: received'
  }
  const fieldError =
    expectBoolean(obj, 'received') ??
    expectString(obj, 'reason', true) ??
    expectBoolean(obj, 'settled', true) ??
    expectBoolean(obj, 'known_trigger', true) ??
    expectBoolean(obj, 'lock_released', true) ??
    expectBoolean(obj, 'rolled_back', true) ??
    expectBoolean(obj, 'cursor_updated', true) ??
    expectNonNegativeInteger(obj, 'indexed_path_count', true)
  if (fieldError) return fieldError
  const successFields = [
    'settled',
    'known_trigger',
    'lock_released',
    'rolled_back',
    'cursor_updated',
    'indexed_path_count',
  ]
  if (obj.received === true) {
    if (obj.reason !== undefined) {
      return 'reason must be absent when received=true'
    }
    return obj.settled === true
      ? null
      : 'settled must be true when received=true'
  }
  if (successFields.some(field => hasOwn(obj, field))) {
    return 'success fields must be absent when received=false'
  }
  return typeof obj.reason === 'string' && obj.reason.length > 0
    ? null
    : 'received=false requires a non-empty reason'
}

function validateTaskDonePayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['task_id', 'task_type'],
    ['transcript_path'],
  )
  if (typeof obj === 'string') return obj
  if (expectString(obj, 'task_id')) return 'task_id must be a string'
  if (
    obj.task_type !== 'DreamTask' &&
    obj.task_type !== 'MainSessionTask' &&
    obj.task_type !== 'AgentTask'
  ) {
    return 'task_type must be DreamTask, MainSessionTask, or AgentTask'
  }
  if (expectString(obj, 'transcript_path', true)) {
    return 'transcript_path must be a string'
  }
  return null
}

function validateSessionClosePayload(payload: unknown): string | null {
  const obj = validateObject(normalizePayload(payload), ['session_id', 'exit_kind'], [])
  if (typeof obj === 'string') return obj
  return expectString(obj, 'session_id') ?? expectString(obj, 'exit_kind')
}

function validateSetEnabledPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['enabled'],
    ['memory_dir', 'project_state_dir'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectBoolean(obj, 'enabled') ??
    expectString(obj, 'memory_dir', true) ??
    expectString(obj, 'project_state_dir', true)
  )
}

function validateMemoryScopePayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [],
    ['memory_dir', 'project_state_dir'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'memory_dir', true) ??
    expectString(obj, 'project_state_dir', true)
  )
}

function validateLastConsolidatedAtResponse(payload: unknown): string | null {
  const obj = validateObject(normalizePayload(payload), ['mtime_ms'], [])
  if (typeof obj === 'string') return obj
  return expectNumber(obj, 'mtime_ms')
}

// W-MEMORY-DATA-COMPLETION A2.1 (2026-06-20) — `memory.search` request bridge.
// Routes a memory search to the orchestrator SE keyed per-project. Since
// W-MEMORY-ALIVE PR-2b the orchestrator side is hybrid retrieval: BM25F
// (charabia-tokenized) lexical floor fused with dense SDK-embedding recall via
// RRF, degrading honestly to lexical-only (`engine: "text"`). `memory_dir` is
// optional here (the orchestrator derives `project_state_dir` from it and
// falls back to last-seen dirs), but recall passes it for correct per-project
// keying. Mirrors the orchestrator handler at
// acosmi-memory-orchestrator/src/ipc_handler.rs (`"memory.search"` case).
//
// W-MEMORY-LIFECYCLE K4/K9 (2026-07-09) — multi-scope retrieval: optional
// `scopes` selects which roots to search (defaults to all three on the
// orchestrator side), and the four dir fields point the orchestrator at the
// user-global memory root and the personal knowledge root (each with its own
// SE state dir). All snake_case, matching the orchestrator IPC wire.
const MEMORY_SEARCH_SCOPES = ['project', 'global', 'knowledge'] as const

function validateMemorySearchPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['query'],
    [
      'memory_dir',
      'project_state_dir',
      'top_k',
      'mode',
      'scopes',
      'global_memory_dir',
      'global_state_dir',
      'knowledge_dir',
      'knowledge_state_dir',
      // W-MEMORY-KB-UPLIFT P0 — `injection: manual` entries are only visible
      // to explicit MemorySearch tool requests; passive recall omits
      // the flag.
      'include_manual',
    ],
  )
  if (typeof obj === 'string') return obj
  const fieldError =
    expectString(obj, 'query') ??
    expectString(obj, 'memory_dir', true) ??
    expectString(obj, 'project_state_dir', true) ??
    expectNumber(obj, 'top_k', true) ??
    expectString(obj, 'mode', true) ??
    expectString(obj, 'global_memory_dir', true) ??
    expectString(obj, 'global_state_dir', true) ??
    expectString(obj, 'knowledge_dir', true) ??
    expectString(obj, 'knowledge_state_dir', true) ??
    expectBoolean(obj, 'include_manual', true)
  if (fieldError) return fieldError
  if (hasOwn(obj, 'scopes')) {
    const scopesError = expectStringArray(obj, 'scopes')
    if (scopesError) return scopesError
    for (const scope of obj.scopes as string[]) {
      if (!(MEMORY_SEARCH_SCOPES as readonly string[]).includes(scope)) {
        return 'scopes entries must be project, global, or knowledge'
      }
    }
  }
  return null
}

function validateMemorySearchResponse(payload: unknown): string | null {
  // Handler returns {ok, results, query, top_k, mode, engine[, reason]}.
  const obj = validateObject(
    normalizePayload(payload),
    ['ok', 'results'],
    ['query', 'top_k', 'mode', 'engine', 'reason'],
  )
  if (typeof obj === 'string') return obj
  const okError = expectBoolean(obj, 'ok')
  if (okError) return okError
  return Array.isArray(obj.results) ? null : 'results must be an array'
}

function validateMemoryStatusPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['memory_dir', 'cwd', 'project_state_dir', 'transcript_dir'],
    ['stale_days'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'memory_dir') ??
    expectString(obj, 'cwd') ??
    expectString(obj, 'project_state_dir') ??
    expectString(obj, 'transcript_dir') ??
    expectNumber(obj, 'stale_days', true)
  )
}

function expectNumberMap(value: unknown, field: string): string | null {
  if (!isRecord(value)) return `${field} must be an object`
  for (const [key, item] of Object.entries(value)) {
    if (typeof item !== 'number' || !Number.isFinite(item)) {
      return `${field}.${key} must be a finite number`
    }
  }
  return null
}

function validateMemoryStatusResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [
      'generated_at_ms',
      'paths',
      'dedup',
      'stale',
      'memory_md',
      'daily_log',
      'transcript_index',
      'lock',
    ],
    // W-MEMORY-LIFECYCLE-GATE (2026-07-27) — dense 半环健康快照。**必须列为
    // 可选键**：`validateObject` 对未知键是 fail-closed 的（见其实现），
    // 所以 orchestrator 侧每加一个响应字段，不在这里登记就会让整个
    // `memory.status` 校验失败。反过来它是可选的，旧版 orchestrator（不发
    // 这个字段）也照样通过。
    ['dense'],
  )
  if (typeof obj === 'string') return obj
  const generatedAtError = expectNumber(obj, 'generated_at_ms')
  if (generatedAtError) return generatedAtError

  // `dense` 内部**刻意不做键名白名单**：它是一个会持续增补的诊断块，
  // 对它套 fail-closed 白名单等于把上面那个坑原样复制一份。只校验形状。
  if (hasOwn(obj, 'dense')) {
    if (!isRecord(obj.dense)) return 'dense must be an object'
    const availableError = expectBoolean(obj.dense, 'available')
    if (availableError) return `dense.${availableError}`
  }

  const paths = validateObject(
    obj.paths,
    [
      'memory_dir',
      'cwd',
      'project_state_dir',
      'rust_derived_root',
      'legacy_rust_derived_root',
    ],
    [],
  )
  if (typeof paths === 'string') return `paths.${paths}`
  for (const key of [
    'memory_dir',
    'cwd',
    'project_state_dir',
    'rust_derived_root',
    'legacy_rust_derived_root',
  ]) {
    const error = expectString(paths, key)
    if (error) return `paths.${error}`
  }

  const dedup = validateObject(
    obj.dedup,
    [
      'scanned_files',
      'duplicate_group_count',
      'duplicate_file_count',
      'duplicate_body_bytes',
    ],
    [],
  )
  if (typeof dedup === 'string') return `dedup.${dedup}`
  for (const key of [
    'scanned_files',
    'duplicate_group_count',
    'duplicate_file_count',
    'duplicate_body_bytes',
  ]) {
    const error = expectNumber(dedup, key)
    if (error) return `dedup.${error}`
  }

  const stale = validateObject(
    obj.stale,
    ['scanned_with_findings', 'stale_file_count', 'reason_counts'],
    [],
  )
  if (typeof stale === 'string') return `stale.${stale}`
  for (const key of ['scanned_with_findings', 'stale_file_count']) {
    const error = expectNumber(stale, key)
    if (error) return `stale.${error}`
  }
  const reasonCountsError = expectNumberMap(stale.reason_counts, 'stale.reason_counts')
  if (reasonCountsError) return reasonCountsError

  const memoryMd = validateObject(
    obj.memory_md,
    [
      'path',
      'exists',
      'line_count',
      'byte_size',
      'overflow_ratio',
      'link_count',
      'long_entry_count',
      'dangling_ref_count',
      'missing_index_count',
      'duplicate_target_count',
    ],
    [],
  )
  if (typeof memoryMd === 'string') return `memory_md.${memoryMd}`
  for (const key of ['path']) {
    const error = expectString(memoryMd, key)
    if (error) return `memory_md.${error}`
  }
  const existsError = expectBoolean(memoryMd, 'exists')
  if (existsError) return `memory_md.${existsError}`
  for (const key of [
    'line_count',
    'byte_size',
    'overflow_ratio',
    'link_count',
    'long_entry_count',
    'dangling_ref_count',
    'missing_index_count',
    'duplicate_target_count',
  ]) {
    const error = expectNumber(memoryMd, key)
    if (error) return `memory_md.${error}`
  }

  const dailyLog = validateObject(
    obj.daily_log,
    ['path', 'exists', 'parent_exists', 'size_bytes', 'line_count'],
    [],
  )
  if (typeof dailyLog === 'string') return `daily_log.${dailyLog}`
  for (const key of ['path']) {
    const error = expectString(dailyLog, key)
    if (error) return `daily_log.${error}`
  }
  for (const key of ['exists', 'parent_exists']) {
    const error = expectBoolean(dailyLog, key)
    if (error) return `daily_log.${error}`
  }
  for (const key of ['size_bytes', 'line_count']) {
    const error = expectNumber(dailyLog, key)
    if (error) return `daily_log.${error}`
  }

  const transcriptIndex = validateObject(
    obj.transcript_index,
    [
      'path',
      'transcript_count',
      'main_session_count',
      'agent_task_count',
      'unknown_task_count',
      'total_size_bytes',
      'latest_mtime_ms',
    ],
    [],
  )
  if (typeof transcriptIndex === 'string') return `transcript_index.${transcriptIndex}`
  const transcriptPathError = expectString(transcriptIndex, 'path')
  if (transcriptPathError) return `transcript_index.${transcriptPathError}`
  for (const key of [
    'transcript_count',
    'main_session_count',
    'agent_task_count',
    'unknown_task_count',
    'total_size_bytes',
  ]) {
    const error = expectNumber(transcriptIndex, key)
    if (error) return `transcript_index.${error}`
  }
  const latestError = expectNullableNumber(transcriptIndex, 'latest_mtime_ms')
  if (latestError) return `transcript_index.${latestError}`

  const lock = validateObject(
    obj.lock,
    [
      'path',
      'exists',
      'last_consolidated_at_ms',
      'holder_pid',
      'holder_running',
      'stale_by_mtime',
    ],
    [],
  )
  if (typeof lock === 'string') return `lock.${lock}`
  return (
    expectString(lock, 'path') ??
    expectBoolean(lock, 'exists') ??
    expectNumber(lock, 'last_consolidated_at_ms') ??
    expectNullableNumber(lock, 'holder_pid') ??
    expectNullableBoolean(lock, 'holder_running') ??
    expectNullableBoolean(lock, 'stale_by_mtime')
  )
}

function validateLockAcquirePayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['holder'],
    ['memory_dir', 'project_state_dir'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'holder') ??
    expectString(obj, 'memory_dir', true) ??
    expectString(obj, 'project_state_dir', true)
  )
}

function validateLockAcquireResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [],
    ['lock_token', 'prior_mtime_ms', 'error'],
  )
  if (typeof obj === 'string') return obj
  if (hasOwn(obj, 'error')) {
    return typeof obj.error === 'string' ? null : 'error must be a string'
  }
  if (!hasOwn(obj, 'lock_token') || !hasOwn(obj, 'prior_mtime_ms')) {
    return 'response must contain lock_token and prior_mtime_ms, or error'
  }
  return expectString(obj, 'lock_token') ?? expectNumber(obj, 'prior_mtime_ms')
}

function validateLockReleasePayload(payload: unknown): string | null {
  const obj = validateObject(normalizePayload(payload), ['lock_token', 'success'], [])
  if (typeof obj === 'string') return obj
  return expectString(obj, 'lock_token') ?? expectBoolean(obj, 'success')
}

// TS-side validators for the four leader_lock IPC methods. Mirror lock.acquire
// validators but use `owner_pid` (numeric) instead of `holder` (string).
function validateLeaderClaimPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['memory_dir', 'owner_pid'],
    ['ttl_ms'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'memory_dir') ??
    expectNonZeroU32(obj, 'owner_pid') ??
    expectPositiveSafeInteger(obj, 'ttl_ms', true)
  )
}

function validateLeaderClaimResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['granted'],
    [
      'holder_pid',
      'leader_token',
      'leader_epoch',
      'claimed_at_ms',
      'lease_expires_at_ms',
    ],
  )
  if (typeof obj === 'string') return obj
  const grantedErr = expectBoolean(obj, 'granted')
  if (grantedErr) return grantedErr
  if (obj.granted === true) {
    return (
      expectNonZeroU32(obj, 'holder_pid') ??
      expectNonEmptyString(obj, 'leader_token') ??
      expectPositiveSafeInteger(obj, 'leader_epoch') ??
      expectNonNegativeInteger(obj, 'claimed_at_ms') ??
      expectNonNegativeInteger(obj, 'lease_expires_at_ms')
    )
  }
  if (
    hasOwn(obj, 'holder_pid') ||
    hasOwn(obj, 'leader_token') ||
    hasOwn(obj, 'leader_epoch') ||
    hasOwn(obj, 'claimed_at_ms') ||
    hasOwn(obj, 'lease_expires_at_ms')
  ) {
    return 'denied claim must not contain lease fields'
  }
  return null
}

function validateLeaderOwnerPidPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['memory_dir', 'owner_pid', 'leader_token', 'leader_epoch'],
    [],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'memory_dir') ??
    expectNonZeroU32(obj, 'owner_pid') ??
    expectNonEmptyString(obj, 'leader_token') ??
    expectPositiveSafeInteger(obj, 'leader_epoch')
  )
}

function validateLeaderRenewPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    [
      'memory_dir',
      'owner_pid',
      'leader_token',
      'leader_epoch',
      'ttl_ms',
    ],
    [],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'memory_dir') ??
    expectNonZeroU32(obj, 'owner_pid') ??
    expectNonEmptyString(obj, 'leader_token') ??
    expectPositiveSafeInteger(obj, 'leader_epoch') ??
    expectPositiveSafeInteger(obj, 'ttl_ms')
  )
}

function validateLeaderRenewResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['still_leader', 'leader_epoch', 'lease_expires_at_ms'],
    [],
  )
  if (typeof obj === 'string') return obj
  const leaderError = expectBoolean(obj, 'still_leader')
  if (leaderError) return leaderError
  if (obj.still_leader === true) {
    return (
      expectPositiveSafeInteger(obj, 'leader_epoch') ??
      expectNonNegativeInteger(obj, 'lease_expires_at_ms')
    )
  }
  return obj.leader_epoch === null && obj.lease_expires_at_ms === null
    ? null
    : 'leader_epoch and lease_expires_at_ms must be null when still_leader is false'
}

function validateLeaderReleaseResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['ok', 'released'],
    [],
  )
  if (typeof obj === 'string') return obj
  const fieldError = expectBoolean(obj, 'ok') ?? expectBoolean(obj, 'released')
  if (fieldError) return fieldError
  return obj.ok === true ? null : 'ok must be true'
}

function validateLeaderQueryPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['memory_dir', 'my_pid'],
    ['ttl_ms'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectString(obj, 'memory_dir') ??
    expectNonZeroU32(obj, 'my_pid') ??
    expectPositiveSafeInteger(obj, 'ttl_ms', true)
  )
}

function validateLeaderQueryResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['kind'],
    ['claim', 'stale_claim'],
  )
  if (typeof obj === 'string') return obj
  const kindErr = expectString(obj, 'kind')
  if (kindErr) return kindErr
  const allowedKinds = new Set([
    'Vacant',
    'HeldByMe',
    'HeldByOther',
    'StaleAvailable',
  ])
  if (typeof obj.kind === 'string' && !allowedKinds.has(obj.kind)) {
    return `unknown leader status kind: ${obj.kind}`
  }
  const validateClaim = (
    value: unknown,
    field: 'claim' | 'stale_claim',
  ): string | null => {
    const claim = validateObject(
      value,
      [
        'holder_pid',
        'leader_epoch',
        'ttl_ms',
        'claimed_at_ms',
        'lease_expires_at_ms',
      ],
      [],
    )
    if (typeof claim === 'string') return `${field}.${claim}`
    const error =
      expectNonZeroU32(claim, 'holder_pid') ??
      expectPositiveSafeInteger(claim, 'leader_epoch') ??
      expectPositiveSafeInteger(claim, 'ttl_ms') ??
      expectNonNegativeInteger(claim, 'claimed_at_ms') ??
      expectNonNegativeInteger(claim, 'lease_expires_at_ms')
    return error ? `${field}.${error}` : null
  }
  if (obj.kind === 'Vacant') {
    return hasOwn(obj, 'claim') || hasOwn(obj, 'stale_claim')
      ? 'Vacant status must not contain claim fields'
      : null
  }
  if (obj.kind === 'HeldByMe' || obj.kind === 'HeldByOther') {
    if (!hasOwn(obj, 'claim')) return 'claim is required for a held leader'
    if (hasOwn(obj, 'stale_claim')) {
      return 'stale_claim must be absent for a held leader'
    }
    return validateClaim(obj.claim, 'claim')
  }
  if (!hasOwn(obj, 'stale_claim')) {
    return 'stale_claim is required for StaleAvailable'
  }
  if (hasOwn(obj, 'claim')) {
    return 'claim must be absent for StaleAvailable'
  }
  return validateClaim(obj.stale_claim, 'stale_claim')
}

function validateLockRollbackPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['prior_mtime_ms'],
    ['memory_dir', 'project_state_dir'],
  )
  if (typeof obj === 'string') return obj
  return (
    expectNumber(obj, 'prior_mtime_ms') ??
    expectString(obj, 'memory_dir', true) ??
    expectString(obj, 'project_state_dir', true)
  )
}

function validateDreamRunNowPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['session_id', 'current_session_id', 'memory_dir'],
    ['now_ms'],
  )
  if (typeof obj === 'string') return obj
  for (const key of ['session_id', 'current_session_id', 'memory_dir']) {
    const error = expectString(obj, key)
    if (error) return error
  }
  return expectNonNegativeInteger(obj, 'now_ms', true)
}

function validateExtractRunNowPayload(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['session_id', 'last_assistant_uuid', 'memory_dir'],
    ['team_memory_dir', 'message_counts', 'now_ms'],
  )
  if (typeof obj === 'string') return obj
  for (const key of ['session_id', 'last_assistant_uuid', 'memory_dir']) {
    const error = expectString(obj, key)
    if (error) return error
  }
  if (expectString(obj, 'team_memory_dir', true)) {
    return 'team_memory_dir must be a string'
  }
  if (hasOwn(obj, 'message_counts')) {
    if (!isRecord(obj.message_counts)) return 'message_counts must be an object'
    for (const [key, value] of Object.entries(obj.message_counts)) {
      if (
        typeof value !== 'number' ||
        !Number.isSafeInteger(value) ||
        value < 0
      ) {
        return `message_counts.${key} must be a non-negative safe integer`
      }
    }
  }
  return expectNonNegativeInteger(obj, 'now_ms', true)
}

function validateDreamRunNowResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['triggers', 'dream_run'],
    ['gate_skip_reason'],
  )
  if (typeof obj === 'string') return obj
  const triggerError = validateTriggerArrayResponse(
    { triggers: obj.triggers },
    'dream',
  )
  if (triggerError) return triggerError
  if (expectString(obj, 'gate_skip_reason', true)) {
    return 'gate_skip_reason must be a string'
  }
  const dreamRun = validateObject(obj.dream_run, ['started'], ['skip_reason'])
  if (typeof dreamRun === 'string') return `dream_run.${dreamRun}`
  return (
    expectBoolean(dreamRun, 'started') ??
    expectString(dreamRun, 'skip_reason', true)
  )
}

function validateExtractRunNowResponse(payload: unknown): string | null {
  const obj = validateObject(
    normalizePayload(payload),
    ['triggers'],
    ['gate_skip_reason'],
  )
  if (typeof obj === 'string') return obj
  const triggerError = validateTriggerArrayResponse(
    { triggers: obj.triggers },
    'extract',
  )
  if (triggerError) return triggerError
  return expectString(obj, 'gate_skip_reason', true)
}

const METHOD_SPECS: Record<string, MethodSpec> = {
  'memory.ping': {
    mode: 'request',
    validatePayload: validateEmptyObject,
    validateResponse(payload) {
      const obj = validateObject(normalizePayload(payload), ['ok', 'service'], [])
      if (typeof obj === 'string') return obj
      return expectBoolean(obj, 'ok') ?? expectString(obj, 'service')
    },
  },
  'memory.turn_end.evaluate': {
    mode: 'request',
    validatePayload: validateTurnEndPayload,
    validateResponse: payload => validateTriggerArrayResponse(payload),
  },
  'memory.runner.candidates': {
    mode: 'request',
    validatePayload: validateRunnerCandidatesPayload,
    validateResponse: validateRunnerCandidatesResponse,
  },
  'memory.runner.claim': {
    mode: 'request',
    validatePayload: validateRunnerClaimPayload,
    validateResponse: validateRunnerClaimResponse,
  },
  'memory.runner.ack': {
    mode: 'request',
    validatePayload: validateRunnerFencePayload,
    validateResponse: validateRunnerLeaseResponse,
  },
  'memory.runner.renew': {
    mode: 'request',
    validatePayload: validateRunnerFencePayload,
    validateResponse: validateRunnerLeaseResponse,
  },
  'memory.runner.release': {
    mode: 'request',
    validatePayload: validateRunnerReleasePayload,
    validateResponse: validateRunnerReleaseResponse,
  },
  'memory.runner.dead_letter': {
    mode: 'request',
    validatePayload: validateRunnerReleasePayload,
    validateResponse: validateRunnerDeadLetterResponse,
  },
  'memory.runner.completed': {
    mode: 'request',
    validatePayload: validateRunnerCompletedPayload,
    validateResponse: validateRunnerCompletionResponse,
  },
  'memory.archive.task_done': {
    mode: 'notification',
    validatePayload: validateTaskDonePayload,
  },
  'memory.archive.session_close': {
    mode: 'request',
    validatePayload: validateSessionClosePayload,
    validateResponse: validateOkResponse,
  },
  'memory.index.changed_paths': {
    mode: 'notification',
    validatePayload: payload => validateStringArrayObject(payload, 'written_paths'),
  },
  'memory.dream.is_enabled': {
    mode: 'request',
    validatePayload: validateMemoryScopePayload,
    validateResponse: validateEnabledResponse,
  },
  'memory.dream.set_enabled': {
    mode: 'request',
    validatePayload: validateSetEnabledPayload,
    validateResponse: validateEnabledResponse,
  },
  'memory.lock.last_consolidated_at': {
    mode: 'request',
    validatePayload: validateMemoryScopePayload,
    validateResponse: validateLastConsolidatedAtResponse,
  },
  'memory.status': {
    mode: 'request',
    validatePayload: validateMemoryStatusPayload,
    validateResponse: validateMemoryStatusResponse,
  },
  'memory.lock.acquire': {
    mode: 'request',
    validatePayload: validateLockAcquirePayload,
    validateResponse: validateLockAcquireResponse,
  },
  'memory.lock.release': {
    mode: 'request',
    validatePayload: validateLockReleasePayload,
    validateResponse: validateOkResponse,
  },
  'memory.lock.rollback': {
    mode: 'request',
    validatePayload: validateLockRollbackPayload,
    validateResponse: validateOkResponse,
  },
  'memory.dream.run_now': {
    mode: 'request',
    validatePayload: validateDreamRunNowPayload,
    validateResponse: validateDreamRunNowResponse,
  },
  'memory.extract.run_now': {
    mode: 'request',
    validatePayload: validateExtractRunNowPayload,
    validateResponse: validateExtractRunNowResponse,
  },
  // Memory leader-election IPC. Detail contract:
  //   libs/acosmi-memory/acosmi-memory-orchestrator/src/leader_lock.rs
  'memory.leader.claim': {
    mode: 'request',
    validatePayload: validateLeaderClaimPayload,
    validateResponse: validateLeaderClaimResponse,
  },
  'memory.leader.renew': {
    mode: 'request',
    validatePayload: validateLeaderRenewPayload,
    validateResponse: validateLeaderRenewResponse,
  },
  'memory.leader.release': {
    mode: 'request',
    validatePayload: validateLeaderOwnerPidPayload,
    validateResponse: validateLeaderReleaseResponse,
  },
  'memory.leader.query': {
    mode: 'request',
    validatePayload: validateLeaderQueryPayload,
    validateResponse: validateLeaderQueryResponse,
  },
  // W-MEMORY-DATA-COMPLETION A2.1 (2026-06-20) — lexical memory recall over the
  // orchestrator SE (replaces the markdown-scan + per-turn LLM selector).
  'memory.search': {
    mode: 'request',
    validatePayload: validateMemorySearchPayload,
    validateResponse: validateMemorySearchResponse,
  },
}

function methodSpec(method: string, mode: MethodMode): MethodSpec | Error {
  const spec = METHOD_SPECS[method]
  if (!spec) return new Error(`memory IPC unknown method: ${method}`)
  if (spec.mode !== mode) {
    const expected = spec.mode === 'request' ? 'send' : 'fire'
    return new Error(`memory IPC method ${method} must use ${expected}()`)
  }
  return spec
}

function wireResult(message: unknown): unknown {
  if (!isRecord(message)) return message
  if (hasOwn(message, 'result')) return message.result
  if (hasOwn(message, 'payload')) return message.payload
  if (!hasOwn(message, 'id') && !hasOwn(message, 'jsonrpc')) return message

  const out: Record<string, unknown> = {}
  for (const [key, value] of Object.entries(message)) {
    if (key !== 'id' && key !== 'jsonrpc') out[key] = value
  }
  return out
}

class SocketMemoryBridgeDriver implements MemoryBridgeDriver {
  private closed = false
  private readonly pendingRequests = new Map<string, PendingRequest>()

  constructor(private readonly endpoint: Endpoint) {}

  isAvailable(): boolean {
    return !this.closed
  }

  pendingCountForTesting(): number {
    return this.pendingRequests.size
  }

  send(
    method: string,
    payload?: unknown,
    opts?: MemoryBridgeOptions,
  ): Promise<unknown> {
    if (this.closed) {
      return Promise.reject(new Error('memory IPC is closed'))
    }

    const spec = methodSpec(method, 'request')
    if (spec instanceof Error) return Promise.reject(spec)

    const normalizedPayload = normalizePayload(payload)
    const payloadError = spec.validatePayload(normalizedPayload)
    if (payloadError) {
      return Promise.reject(
        new Error(`memory IPC invalid ${method} payload: ${payloadError}`),
      )
    }

    const timeoutMs = opts?.timeout_ms ?? DEFAULT_TIMEOUT_MS
    const id = randomUUID()
    const request: WireRequest = {
      id,
      method,
      payload: normalizedPayload,
    }

    return new Promise((resolve, reject) => {
      const socket = this.openSocket()
      let buffer = ''
      let settled = false

      const finish = (error: Error | null, value?: unknown) => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        this.pendingRequests.delete(id)
        socket.destroy()
        if (error) {
          reject(error)
        } else {
          resolve(value)
        }
      }

      const timer = setTimeout(() => {
        finish(new Error(`memory IPC request timed out after ${timeoutMs}ms`))
      }, timeoutMs)

      this.pendingRequests.set(id, {
        method,
        resolve,
        reject,
        socket,
        timer,
      })

      const handleFrame = (message: unknown) => {
        const responseId = isRecord(message) && typeof message.id === 'string'
          ? message.id
          : id
        const pending = this.pendingRequests.get(responseId)
        if (!pending) return

        if (isRecord(message) && hasOwn(message, 'error') && hasOwn(message, 'id')) {
          const error = message.error
          const messageText = isRecord(error) && typeof error.message === 'string'
            ? error.message
            : String(error)
          finish(new Error(`memory IPC ${pending.method} failed: ${messageText}`))
          return
        }

        const result = wireResult(message)
        const responseError = spec.validateResponse?.(result)
        if (responseError) {
          finish(
            new Error(
              `memory IPC invalid ${pending.method} response: ${responseError}`,
            ),
          )
          return
        }

        finish(null, result)
      }

      // W-MEMORY-LIFECYCLE K1(b) (2026-07-09): Windows named pipes have no
      // half-close — after the orchestrator writes its one-shot response
      // (historically without a trailing '\n', so the 'data' framing never
      // fires) and calls disconnect(), the client read fails with EPIPE and
      // then 'close', while the complete response is already sitting in
      // `buffer`. Before treating the transport error as fatal, try to parse
      // the buffered bytes as the final frame — mirroring the Rust client fix
      // in crates/acosmi-app-server/src/dispatcher/memory.rs::
      // try_decode_memory_ipc_response ("read_to_end would drop the buffered
      // response together with ERROR_BROKEN_PIPE"). Only a successful parse
      // settles; a parse failure falls through to the original error.
      const settledFromBufferedResponse = (): boolean => {
        if (settled) return true
        if (buffer.trim().length === 0) return false
        consumeFinalFrame(buffer, handleFrame, () => {})
        return settled
      }

      socket.setTimeout(timeoutMs, () => {
        finish(new Error(`memory IPC request timed out after ${timeoutMs}ms`))
      })
      socket.on('connect', () => {
        socket.write(JSON.stringify(request) + '\n')
      })
      socket.on('data', chunk => {
        buffer += Buffer.isBuffer(chunk) ? chunk.toString('utf8') : String(chunk)
        buffer = consumeDelimitedFrames(buffer, handleFrame, finish)
      })
      socket.on('end', () => {
        if (settled) return
        if (buffer.trim().length === 0) {
          finish(new Error('memory IPC connection ended without a response'))
          return
        }
        consumeFinalFrame(buffer, handleFrame, finish)
      })
      socket.on('close', () => {
        if (settledFromBufferedResponse()) return
        finish(new Error('memory IPC connection closed before response'))
      })
      socket.on('error', err => {
        if (settledFromBufferedResponse()) return
        finish(err instanceof Error ? err : new Error(String(err)))
      })
    })
  }

  fire(method: string, payload?: unknown): void {
    if (this.closed) return

    const spec = methodSpec(method, 'notification')
    if (spec instanceof Error) throw spec

    const normalizedPayload = normalizePayload(payload)
    const payloadError = spec.validatePayload(normalizedPayload)
    if (payloadError) {
      throw new Error(`memory IPC invalid ${method} payload: ${payloadError}`)
    }

    const socket = this.openSocket()
    const timer = setTimeout(() => socket.destroy(), NOTIFICATION_TIMEOUT_MS)
    socket.on('connect', () => {
      socket.end(JSON.stringify({ method, payload: normalizedPayload }) + '\n')
    })
    socket.on('error', () => {})
    socket.on('close', () => clearTimeout(timer))
  }

  close(): void {
    if (this.closed) return
    this.closed = true
    for (const [id, pending] of this.pendingRequests) {
      clearTimeout(pending.timer)
      pending.socket.destroy()
      pending.reject(new Error('memory IPC is closed'))
      this.pendingRequests.delete(id)
    }
  }

  private openSocket(): Socket {
    return socketFactoryForTesting?.(this.endpoint) ?? createConnection(this.endpoint.path)
  }
}

function consumeDelimitedFrames(
  buffer: string,
  onFrame: (message: unknown) => void,
  onError: (error: Error) => void,
): string {
  let rest = buffer
  while (true) {
    const newlineIndex = rest.indexOf('\n')
    if (newlineIndex < 0) return rest
    const rawLine = rest.slice(0, newlineIndex).trim()
    rest = rest.slice(newlineIndex + 1)
    if (!rawLine) continue
    try {
      onFrame(JSON.parse(rawLine))
    } catch (error) {
      onError(
        error instanceof Error
          ? error
          : new Error(`memory IPC invalid JSON frame: ${String(error)}`),
      )
      return ''
    }
  }
}

function consumeFinalFrame(
  buffer: string,
  onFrame: (message: unknown) => void,
  onError: (error: Error) => void,
): void {
  const raw = buffer.trim()
  if (!raw) return
  try {
    onFrame(JSON.parse(raw))
  } catch (error) {
    onError(
      error instanceof Error
        ? error
        : new Error(`memory IPC invalid JSON response: ${String(error)}`),
    )
  }
}

function createDriverForEndpoint(endpoint: string): MemoryBridgeDriver | null {
  if (endpoint.startsWith('unix:')) {
    const socketPath = endpoint.slice('unix:'.length)
    return socketPath.length > 0
      ? new SocketMemoryBridgeDriver({ kind: 'unix', path: socketPath })
      : null
  }

  if (endpoint.startsWith('npipe:')) {
    const pipePath = endpoint.slice('npipe:'.length)
    return pipePath.length > 0
      ? new SocketMemoryBridgeDriver({ kind: 'npipe', path: pipePath })
      : null
  }

  return null
}

export const memoryBridgeIpc = {
  init(endpoint: string): boolean {
    const driver = createDriverForEndpoint(endpoint)
    activeDriver = driver
    return driver !== null
  },

  initFromEnv(): boolean {
    const endpoint = process.env[MEMORY_IPC_ENDPOINT_ENV]
    return endpoint ? this.init(endpoint) : false
  },

  isAvailable(): boolean {
    return activeDriver?.isAvailable?.() ?? activeDriver !== null
  },

  async send(
    method: string,
    payload?: unknown,
    opts?: MemoryBridgeOptions,
  ): Promise<unknown> {
    if (!activeDriver) {
      throw new Error('memory IPC is not initialized')
    }
    return activeDriver.send(method, payload, opts)
  },

  fire(method: string, payload?: unknown): void {
    if (!activeDriver) return
    if (activeDriver.fire) {
      activeDriver.fire(method, payload)
      return
    }
    void activeDriver.send(method, payload).catch(() => {})
  },

  close(): void {
    activeDriver?.close?.()
    activeDriver = null
  },
}

export function setMemoryBridgeIpcDriverForTesting(
  driver: MemoryBridgeDriver | null,
): void {
  activeDriver = driver
}

export function setMemoryBridgeIpcSocketFactoryForTesting(
  factory: MemoryBridgeSocketFactory | null,
): void {
  socketFactoryForTesting = factory
}

export function getMemoryBridgeIpcPendingRequestCountForTesting(): number {
  return activeDriver instanceof SocketMemoryBridgeDriver
    ? activeDriver.pendingCountForTesting()
    : 0
}
