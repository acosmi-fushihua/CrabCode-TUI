// UDS client for crabcode-cron daemon.
//
// Wire-compatible with `crates/crabcode-cron/src/ipc.rs`:
//   request:  {"id":"<str>","method":"cron.<sub>","params":<value>} + "\n"
//   response: {"id":"<id>","result":<value>} + "\n"
//   protocol error: {"id":<id|null>,"error":{"code":<int>,"message":"..."}} + "\n"
//
// Single-shot connection per call (single-conn single-request model — same as
// the Rust CLI client at acosmi-cmd-cron/src/client.rs:62-101). No pooling, no
// pipelining. Daemon lifecycle is the daemon's problem; this client just
// dials and reads one frame.
//
// Daemon lifecycle: NOT done by this transport. Read/cleanup helpers connect
// to an existing daemon only. Execution-producing methods are compile-closed
// until the exact-target live consumer ships; after release, lifecycle remains
// an explicit `crabcode cron ensure` / product-host responsibility.

import { randomUUID } from 'crypto'
import { Socket } from 'net'
import { join } from 'path'
import { createInterface } from 'readline'

import { sanitizePipeUserName } from './ipcPipeName.js'
import { DURABLE_CRON_LIVE_CONSUMER_READY } from '../tools/ScheduleCronTool/constants.js'
import { resolveCronStateIdentity } from './cronStateIdentity.js'
import { getCrabCodeRuntimeHomeDir } from './envUtils.js'

export const CRON_DAEMON_DEFAULT_TIMEOUT_MS = 5000

export type CronDaemonError = {
  code: number
  message: string
}

export class CronDaemonProtocolError extends Error {
  constructor(public readonly daemonError: CronDaemonError) {
    super(`cron daemon protocol error (${daemonError.code}): ${daemonError.message}`)
  }
}

export class CronDaemonConnectError extends Error {
  constructor(message: string, public readonly cause?: unknown) {
    super(message)
  }
}

/** The daemon was reachable but returned a malformed/ambiguous frame. This
 * is a protocol-integrity failure, not evidence that fail-open delivery is
 * safe. Callers must keep side effects closed. */
export class CronDaemonResponseError extends Error {}

export class CronDaemonTimeoutError extends Error {
  constructor(timeoutMs: number) {
    super(`cron daemon RPC timed out after ${timeoutMs}ms`)
  }
}

export class CronDurableReleaseError extends Error {
  readonly code = 'durable_cron_live_consumer_unreleased'

  constructor(method: string) {
    super(
      `${method} is unavailable until the exact-target durable live-consumer cutover is released`,
    )
  }
}

export type CronStateIdentityMetadata = {
  code?: string
  stateIdentity?: string
  expectedStateIdentity?: string | null
  stateIdentityMatches?: boolean | null
  ledgerEpoch?: string
  expectedLedgerEpoch?: string | null
  ledgerEpochMatches?: boolean | null
}

export class CronStateIdentityError extends Error {
  constructor(
    public readonly code: 'state_identity_required' | 'state_identity_mismatch',
    public readonly actualStateIdentity: string | null,
    public readonly expectedStateIdentity: string | null,
    message: string,
  ) {
    super(message)
  }
}

export class CronLedgerEpochError extends Error {
  constructor(
    public readonly code: 'ledger_epoch_required' | 'ledger_epoch_mismatch',
    public readonly actualLedgerEpoch: string | null,
    public readonly expectedLedgerEpoch: string | null,
    message: string,
  ) {
    super(message)
  }
}

/** A fail-closed delivery-state error returned by the daemon. Connectivity,
 * timeout, and old-daemon unknown-method failures remain transport errors and
 * are handled by the coordinator's explicitly bounded degraded path. */
export class CronDeliveryStateError extends Error {
  constructor(
    public readonly code: string,
    message: string,
  ) {
    super(message)
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function formatConnectError(socketPath: string, err: unknown): string {
  const message = err instanceof Error ? err.message : String(err)
  return `failed to connect cron daemon at ${socketPath}: ${message}`
}

/**
 * Pure response-envelope parser shared by the production socket transport and
 * protocol tests. It has no lifecycle or release authority: callers must pass
 * the request id/method that were already admitted by their own boundary.
 */
export function parseCronDaemonResponseLine<T>(
  line: string,
  expected: { requestId: string; method: string },
): T {
  const raw = line.trim()
  if (raw.length === 0) {
    throw new CronDaemonResponseError('cron daemon returned empty line')
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch {
    const responseHint = expected.method.startsWith('cron.delivery_')
      ? '[redacted delivery frame]'
      : raw.slice(0, 120)
    throw new CronDaemonResponseError(
      `cron daemon returned non-JSON: ${responseHint}`,
    )
  }
  if (!isRecord(parsed)) {
    throw new CronDaemonResponseError(
      'cron daemon returned a non-object response envelope',
    )
  }
  if (parsed.id !== expected.requestId) {
    throw new CronDaemonResponseError(
      'cron daemon returned a response with a mismatched request id',
    )
  }
  const hasError = Object.prototype.hasOwnProperty.call(parsed, 'error')
  const hasResult = Object.prototype.hasOwnProperty.call(parsed, 'result')
  if (hasError === hasResult) {
    throw new CronDaemonResponseError(
      'cron daemon returned an ambiguous response envelope',
    )
  }
  if (hasError) {
    const error = parsed.error
    if (
      !isRecord(error) ||
      !Number.isSafeInteger(error.code) ||
      typeof error.message !== 'string'
    ) {
      throw new CronDaemonResponseError(
        'cron daemon returned an invalid protocol error envelope',
      )
    }
    throw new CronDaemonProtocolError({
      code: error.code as number,
      message: error.message,
    })
  }
  return parsed.result as T
}

/**
 * Resolve cron daemon IPC path. Mirrors `acosmi_daemon_launcher::paths::socket_file("cron")`:
 *
 * - Unix: `$CRABCODE_CONFIG_DIR` ?? `$CRABCODE_HOME/.crabcode` ??
 *   `$HOME/.crabcode` + `/run/cron.sock` (UDS)
 * - Windows: `\\.\pipe\crabcode-<USERNAME>-cron` Named Pipe (per-user namespace).
 *   `<USERNAME>` 取自 `USERNAME` env，缺失退化为 `user`。Node `net.createConnection({path})`
 *   在 Windows 原生支持 named pipe；不要拼 `\\.\pipe\Local\` namespace。
 *
 * Sanitize 与 Rust 端 `paths.rs::sanitize_pipe_user_name` 完全对齐（TS 单源 =
 * `transport-default.ts::sanitizePipeUserName`，含非 ASCII 用户名 fnv1a32
 * 唯一性后缀；本文件不再自持镜像副本，防双 TS 镜像漂移）。
 */
export function getCronSocketPath(): string {
  if (process.platform === 'win32') {
    const rawUser = process.env.USERNAME ?? 'user'
    return `\\\\.\\pipe\\crabcode-${sanitizePipeUserName(rawUser)}-cron`
  }
  return join(getCrabCodeRuntimeHomeDir(), 'run', 'cron.sock')
}

/**
 * Send one policy-admitted cron.* RPC and return the daemon's `result` field.
 *
 * @throws {CronDaemonConnectError} socket missing / refused / EOF before reply
 * @throws {CronDaemonProtocolError} daemon returned `error` envelope (parse error / unknown method / missing method)
 * @throws {CronDaemonTimeoutError} daemon didn't reply within timeoutMs
 *
 * Business errors (`{status: "error", error: "..."}` inside `result`) are
 * returned as-is — callers that want bail behavior call `unwrapBusinessOk`.
 */
async function callCronDaemon<T = unknown>(
  method: string,
  params: unknown,
  opts?: { socketPath?: string; timeoutMs?: number },
): Promise<T> {
  assertCronDaemonMethodReleased(method, params)
  const socketPath = opts?.socketPath ?? getCronSocketPath()
  const timeoutMs = opts?.timeoutMs ?? CRON_DAEMON_DEFAULT_TIMEOUT_MS
  const id = `t-${randomUUID().slice(0, 8)}`
  const requestParams = attachExpectedStateIdentity(method, params, socketPath)
  const request = JSON.stringify({ id, method, params: requestParams }) + '\n'

  return new Promise<T>((resolve, reject) => {
    let settled = false
    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      socket.destroy()
      reject(new CronDaemonTimeoutError(timeoutMs))
    }, timeoutMs)

    const finish = (fn: () => void) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      // One request owns one connection. Tear it down on every terminal path,
      // including malformed/protocol-error replies whose peer might otherwise
      // keep the socket open and leak one descriptor per poll.
      try {
        socket.destroy()
      } catch {
        // best-effort cleanup; the original terminal result still wins
      }
      fn()
    }

    // Bun may surface UDS connect failures synchronously when the optional
    // cron daemon socket is missing. Build the socket first, install handlers,
    // then normalize both sync throws and async error events into the same
    // rejected Promise so callers can keep their fail-soft contract.
    const socket = new Socket()
    socket.setEncoding('utf-8')

    socket.once('error', err => {
      finish(() =>
        reject(
          new CronDaemonConnectError(
            formatConnectError(socketPath, err),
            err,
          ),
        ),
      )
    })

    socket.once('connect', () => {
      socket.write(request)
    })

    const reader = createInterface({ input: socket, crlfDelay: Infinity })
    reader.once('error', err => {
      finish(() =>
        reject(new CronDaemonConnectError(formatConnectError(socketPath, err), err)),
      )
    })
    reader.once('line', (line: string) => {
      try {
        const result = parseCronDaemonResponseLine<T>(line, {
          requestId: id,
          method,
        })
        finish(() => resolve(result))
      } catch (error) {
        finish(() =>
          reject(error instanceof Error ? error : new Error(String(error))),
        )
      }
    })

    reader.once('close', () => {
      finish(() =>
        reject(new CronDaemonConnectError('cron daemon closed connection before replying')),
      )
    })

    try {
      socket.connect({ path: socketPath })
    } catch (err) {
      try {
        socket.destroy()
      } catch {
        // best-effort
      }
      finish(() =>
        reject(new CronDaemonConnectError(formatConnectError(socketPath, err), err)),
      )
    }
  })
}

function isDisableOnlyCronUpdate(params: unknown): boolean {
  if (!isRecord(params) || !isRecord(params.patch)) return false
  const keys = Object.keys(params.patch)
  return keys.length === 1 && params.patch.enabled === false
}

/**
 * Lowest TS transport boundary. While the PR-5 live cutover is incomplete,
 * only observation and cleanup may reach an already-running daemon. Fire
 * consumers, delivery claims/cursor paths, and execution-producing mutations
 * fail before socket/path/request construction.
 */
function assertCronDaemonMethodReleased(method: string, params: unknown): void {
  if (DURABLE_CRON_LIVE_CONSUMER_READY) return
  if (
    method === 'cron.status' ||
    method === 'cron.list' ||
    method === 'cron.runs' ||
    method === 'cron.remove' ||
    (method === 'cron.update' && isDisableOnlyCronUpdate(params))
  ) {
    return
  }
  throw new CronDurableReleaseError(method)
}

function attachExpectedStateIdentity(
  method: string,
  params: unknown,
  socketPath: string,
): unknown {
  if (!method.startsWith('cron.')) return params
  if (params !== null && (typeof params !== 'object' || Array.isArray(params))) {
    throw new Error('cron daemon params must be an object or null')
  }
  const object = { ...((params ?? {}) as Record<string, unknown>) }
  if (
    typeof object.expectedStateIdentity !== 'string' ||
    object.expectedStateIdentity.length === 0
  ) {
    object.expectedStateIdentity = resolveCronStateIdentity(socketPath)
  }
  return object
}

/** Daemon business envelope: `{status:"ok",...}` or `{status:"error",error:"..."}`. */
export function unwrapBusinessOk<
  T extends { status?: string; error?: string } & CronStateIdentityMetadata,
>(
  result: T,
): T {
  if (!isRecord(result)) {
    throw new CronDaemonResponseError(
      'cron daemon returned a non-object business response',
    )
  }
  if (result?.status === 'ok') return result
  const msg = result?.error ?? '(unknown daemon business error)'
  if (
    result.code === 'state_identity_required' ||
    result.code === 'state_identity_mismatch'
  ) {
    throw new CronStateIdentityError(
      result.code,
      result.stateIdentity ?? null,
      result.expectedStateIdentity ?? null,
      `cron daemon: ${msg}`,
    )
  }
  if (
    result.code === 'ledger_epoch_required' ||
    result.code === 'ledger_epoch_mismatch'
  ) {
    throw new CronLedgerEpochError(
      result.code,
      result.ledgerEpoch ?? null,
      result.expectedLedgerEpoch ?? null,
      `cron daemon: ${msg}`,
    )
  }
  if (typeof result.code === 'string' && result.code.length > 0) {
    throw new CronDeliveryStateError(result.code, `cron daemon: ${msg}`)
  }
  throw new Error(`cron daemon: ${msg}`)
}

// ── High-level helpers (1:1 with crates/crabcode-cron/src/ipc.rs sub_*) ────

export type CronJobView = {
  id: string
  name?: string
  enabled?: boolean
  schedule?: { kind?: string; expr?: string; at?: string; everyMs?: number; anchorMs?: number; tz?: string }
  payload?: { message?: string; text?: string }
  permanent?: boolean
  // ...passthrough; full schema lives in Rust task_store::CronJob
  [k: string]: unknown
}

export async function cronList(opts?: {
  includeDisabled?: boolean
  socketPath?: string
}): Promise<CronJobView[]> {
  const result = await callCronDaemon<{ status: string; jobs?: CronJobView[]; error?: string }>(
    'cron.list',
    { includeDisabled: opts?.includeDisabled ?? true },
    { socketPath: opts?.socketPath },
  )
  unwrapBusinessOk(result)
  return result.jobs ?? []
}

export async function cronAdd(input: {
  name: string
  schedule: { kind: 'cron' | 'every' | 'at'; expr?: string; everyMs?: number; anchorMs?: number; at?: string; tz?: string }
  payload: { message?: string; text?: string }
  enabled?: boolean
  permanent?: boolean
  deleteAfterRun?: boolean
  sessionTarget?: 'main' | 'isolated' | 'continuation'
  wakeMode?: 'now' | 'next-heartbeat'
  /** Canonical JSON execution target for exact consumer eligibility. */
  sessionKey?: string
  socketPath?: string
}): Promise<{ id: string }> {
  if (!DURABLE_CRON_LIVE_CONSUMER_READY) {
    throw new CronDurableReleaseError('cron.add')
  }
  const { socketPath, ...rest } = input
  const result = await callCronDaemon<{ status: string; id?: string; error?: string }>(
    'cron.add',
    rest,
    { socketPath },
  )
  unwrapBusinessOk(result)
  if (typeof result.id !== 'string') {
    throw new Error('cron.add: daemon did not return an id')
  }
  return { id: result.id }
}

export async function cronRemove(id: string, opts?: { socketPath?: string }): Promise<void> {
  const result = await callCronDaemon<{ status: string; error?: string }>(
    'cron.remove',
    { id },
    { socketPath: opts?.socketPath },
  )
  unwrapBusinessOk(result)
}

export type CronFiredEvent = { type: 'cron.fired'; job: CronJobView }

export async function cronPollEvents(opts?: { socketPath?: string }): Promise<CronFiredEvent[]> {
  const result = await callCronDaemon<{
    status: string
    events?: CronFiredEvent[]
    error?: string
  }>('cron.poll_events', null, { socketPath: opts?.socketPath })
  unwrapBusinessOk(result)
  return result.events ?? []
}

// ── Durable outbox (W-CRON-AUTOMATION-E2E P7) ──────────────────────────────
//
// `cron.poll_events` / `cron.subscribe` are the low-latency *live* paths but
// non-durable: if the daemon restarts or the REPL is offline when a job
// fires, the event is gone. The outbox is an append-only JSONL log on the
// daemon side; `cron.outbox_read` is a *cursor* read (idempotent, not a
// drain) so a REPL that was offline / just restarted can recover the events
// it missed by reading from its last-consumed event id.

/** Outbox event type — mirrors Rust `OutboxEventType` (serde camelCase). */
export type CronOutboxEventType = 'fired' | 'missed' | 'skipped'

/** One durable outbox event — mirrors Rust `acosmi_scheduler::OutboxEvent`. */
export type CronOutboxEvent = {
  /** Monotonic event id. Consumer cursor = "highest id already consumed". */
  eventId: number
  jobId: string
  /** 'fired' (run it now — incl. late catch-up of a missed one-shot) |
   *  'missed' (neutral record) | 'skipped' (too stale, record only). */
  eventType: CronOutboxEventType
  /** The job's planned fire time (epoch ms). */
  scheduledAtMs: number
  /** When this event was written to the outbox (epoch ms). */
  emittedAtMs: number
  /** The job's payload message/text, for prompting. */
  payloadMessage?: string
  jobName?: string
  sessionTarget?: 'main' | 'isolated' | 'continuation'
  wakeMode?: 'now' | 'next-heartbeat'
  executionTarget?: {
    version: number
    ownerThreadId: string
    project: string
    cwd: string
    connectionId: string
    surface: string
  }
}

/** One cursor page plus the immutable identities of the daemon state +
 * logical outbox ledger that produced it. `ledgerEpoch` is additive on the
 * local cron IPC; null means an older daemon that cannot safely participate
 * in coordinated delivery. */
export type CronOutboxReadResult = {
  events: CronOutboxEvent[]
  lastEventId: number
  /** Bounds of the complete retained window, not just this cursor page. */
  oldestEventId: number | null
  newestEventId: number | null
  stateIdentity: string
  ledgerEpoch: string | null
}

type CronOutboxBusinessEnvelope = {
  status?: string
  events?: unknown
  lastEventId?: number
  oldestEventId?: number | null
  newestEventId?: number | null
  ledgerEpoch?: string
  error?: string
} & CronStateIdentityMetadata

/** Event ids cross the Rust/JSON/JavaScript boundary as JSON numbers. Keep
 * them inside JavaScript's exact integer domain until a future wire migration
 * can represent them as decimal strings. */
export function isSafeCronEventId(
  value: unknown,
  options?: { allowZero?: boolean },
): value is number {
  return (
    typeof value === 'number' &&
    Number.isSafeInteger(value) &&
    value >= (options?.allowZero ? 0 : 1)
  )
}

const MAX_U64 = 18_446_744_073_709_551_615n
const UUID_WIRE_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

function isUuidWire(value: unknown): value is string {
  return typeof value === 'string' && UUID_WIRE_PATTERN.test(value)
}

function isCanonicalPositiveU64(value: unknown): value is string {
  if (typeof value !== 'string' || !/^[1-9]\d*$/.test(value)) return false
  try {
    return BigInt(value) <= MAX_U64
  } catch {
    return false
  }
}

function isSafeEpochMs(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 0
}

/**
 * Cursor read of the durable cron outbox.
 *
 * @param afterEventId only events with `eventId > afterEventId` are returned.
 *        Pass `0` to read from the beginning of the current outbox window.
 * @returns `{events, lastEventId}` — `events` ascending by `eventId`;
 *          `lastEventId` is the max id returned, or `afterEventId` when empty
 *          (so the caller's cursor never moves backward).
 *
 * Idempotent: repeated calls with the same cursor return the same events.
 */
export async function cronOutboxRead(
  afterEventId: number,
  opts?: { socketPath?: string; limit?: number; timeoutMs?: number },
): Promise<CronOutboxReadResult> {
  if (!isSafeCronEventId(afterEventId, { allowZero: true })) {
    throw new RangeError('afterEventId must be a non-negative safe integer')
  }
  const params: { afterEventId: number; limit?: number } = { afterEventId }
  if (opts?.limit !== undefined) {
    if (!Number.isSafeInteger(opts.limit) || opts.limit <= 0) {
      throw new RangeError('cron outbox limit must be a positive safe integer')
    }
    params.limit = opts.limit
  }
  const result = await callCronDaemon<CronOutboxBusinessEnvelope>(
    'cron.outbox_read',
    params,
    {
      socketPath: opts?.socketPath,
      timeoutMs: opts?.timeoutMs,
    },
  )
  const socketPath = opts?.socketPath ?? getCronSocketPath()
  const expectedStateIdentity = resolveCronStateIdentity(socketPath)
  return parseCronOutboxReadResult(result, {
    afterEventId,
    expectedStateIdentity,
  })
}

/** Pure durable-page validator. It cannot open IPC or authorize consumption. */
export function parseCronOutboxReadResult(
  value: unknown,
  input: { afterEventId: number; expectedStateIdentity: string },
): CronOutboxReadResult {
  if (!isSafeCronEventId(input.afterEventId, { allowZero: true })) {
    throw new RangeError('afterEventId must be a non-negative safe integer')
  }
  if (!isRecord(value)) {
    throw new CronDaemonResponseError(
      'cron daemon returned a non-object outbox response',
    )
  }
  const result = value as CronOutboxBusinessEnvelope
  const { afterEventId, expectedStateIdentity } = input
  if (
    result.stateIdentityMatches === false ||
    (typeof result.stateIdentity === 'string' &&
      result.stateIdentity.length > 0 &&
      result.stateIdentity !== expectedStateIdentity)
  ) {
    throw new CronStateIdentityError(
      'state_identity_mismatch',
      typeof result.stateIdentity === 'string' ? result.stateIdentity : null,
      expectedStateIdentity,
      'cron daemon returned an outbox page from a different state identity',
    )
  }
  if (
    result.stateIdentity !== expectedStateIdentity ||
    result.expectedStateIdentity !== expectedStateIdentity ||
    result.stateIdentityMatches !== true
  ) {
    throw new CronDaemonResponseError(
      'cron daemon outbox response omitted verified state identity metadata',
    )
  }
  unwrapBusinessOk(result)

  if (!Array.isArray(result.events)) {
    throw new CronDaemonResponseError(
      'cron daemon returned a non-array outbox events field',
    )
  }
  const events: CronOutboxEvent[] = []
  let previousEventId = afterEventId
  for (const rawEvent of result.events) {
    if (!isRecord(rawEvent) || !isSafeCronEventId(rawEvent.eventId)) {
      throw new CronDaemonResponseError(
        'cron daemon returned an unsafe outbox event id',
      )
    }
    if (rawEvent.eventId <= previousEventId) {
      throw new CronDaemonResponseError(
        'cron daemon returned a non-monotonic outbox page',
      )
    }
    if (
      rawEvent.eventType !== 'fired' &&
      rawEvent.eventType !== 'missed' &&
      rawEvent.eventType !== 'skipped'
    ) {
      throw new CronDaemonResponseError(
        'cron daemon returned an unknown outbox event type',
      )
    }
    if (
      typeof rawEvent.jobId !== 'string' ||
      (rawEvent.eventType !== 'skipped' && rawEvent.jobId.trim().length === 0)
    ) {
      throw new CronDaemonResponseError(
        'cron daemon returned an invalid outbox job id',
      )
    }
    if (
      !isSafeEpochMs(rawEvent.scheduledAtMs) ||
      !isSafeEpochMs(rawEvent.emittedAtMs) ||
      (rawEvent.payloadMessage !== undefined &&
        typeof rawEvent.payloadMessage !== 'string') ||
      (rawEvent.eventType !== 'skipped' &&
        (typeof rawEvent.payloadMessage !== 'string' ||
          rawEvent.payloadMessage.trim().length === 0)) ||
      (rawEvent.jobName !== undefined && typeof rawEvent.jobName !== 'string')
    ) {
      throw new CronDaemonResponseError(
        'cron daemon returned an invalid outbox event payload',
      )
    }
    if (
      (rawEvent.sessionTarget !== undefined &&
        rawEvent.sessionTarget !== 'main' &&
        rawEvent.sessionTarget !== 'isolated' &&
        rawEvent.sessionTarget !== 'continuation') ||
      (rawEvent.wakeMode !== undefined &&
        rawEvent.wakeMode !== 'now' &&
        rawEvent.wakeMode !== 'next-heartbeat') ||
      (rawEvent.executionTarget !== undefined &&
        (!isRecord(rawEvent.executionTarget) ||
          rawEvent.executionTarget.version !== 1 ||
          typeof rawEvent.executionTarget.ownerThreadId !== 'string' ||
          typeof rawEvent.executionTarget.project !== 'string' ||
          typeof rawEvent.executionTarget.cwd !== 'string' ||
          typeof rawEvent.executionTarget.connectionId !== 'string' ||
          typeof rawEvent.executionTarget.surface !== 'string'))
    ) {
      throw new CronDaemonResponseError(
        'cron daemon returned invalid execution target metadata',
      )
    }
    previousEventId = rawEvent.eventId
    events.push(rawEvent as CronOutboxEvent)
  }
  const lastEventId = result.lastEventId
  if (
    !isSafeCronEventId(lastEventId, { allowZero: true }) ||
    lastEventId !== previousEventId
  ) {
    throw new CronDaemonResponseError(
      'cron daemon returned an invalid outbox lastEventId',
    )
  }
  const ledgerEpoch =
    result.ledgerEpoch === undefined
      ? null
      : isUuidWire(result.ledgerEpoch)
        ? result.ledgerEpoch
        : undefined
  if (ledgerEpoch === undefined) {
    throw new CronDaemonResponseError(
      'cron daemon returned an invalid outbox ledger epoch',
    )
  }
  const hasOldestEventId = Object.prototype.hasOwnProperty.call(
    result,
    'oldestEventId',
  )
  const hasNewestEventId = Object.prototype.hasOwnProperty.call(
    result,
    'newestEventId',
  )
  if (ledgerEpoch !== null && (!hasOldestEventId || !hasNewestEventId)) {
    throw new CronDaemonResponseError(
      'cron daemon omitted retained bounds for an identified ledger',
    )
  }
  const oldestEventId = result.oldestEventId ?? null
  const newestEventId = result.newestEventId ?? null
  if (
    (oldestEventId !== null && !isSafeCronEventId(oldestEventId)) ||
    (newestEventId !== null && !isSafeCronEventId(newestEventId)) ||
    (oldestEventId === null) !== (newestEventId === null) ||
    (ledgerEpoch !== null &&
      events.length > 0 &&
      (oldestEventId === null || newestEventId === null)) ||
    (oldestEventId !== null &&
      newestEventId !== null &&
      oldestEventId > newestEventId) ||
    (events.length > 0 &&
      oldestEventId !== null &&
      events[0]!.eventId < oldestEventId) ||
    (events.length > 0 &&
      newestEventId !== null &&
      events.at(-1)!.eventId > newestEventId)
  ) {
    throw new CronDaemonResponseError(
      'cron daemon returned invalid retained outbox bounds',
    )
  }
  return {
    events,
    lastEventId,
    oldestEventId,
    newestEventId,
    stateIdentity:
      typeof result.stateIdentity === 'string' && result.stateIdentity.length > 0
        ? result.stateIdentity
        : expectedStateIdentity,
    ledgerEpoch,
  }
}

// ── Durable delivery claim/accept (R5/C4) ───────────────────────────────

export type CronDeliveryConsumerKind = 'tui'

export type CronDeliveryConsumerWire = {
  kind: CronDeliveryConsumerKind
  id: string
}

export type CronDeliveryLeaseWire = {
  eventId: number
  /** Decimal u64 string. Never coerce this to JS number: doing so would
   * weaken generation fencing above Number.MAX_SAFE_INTEGER. */
  generation: string
  leaseToken: string
  expiresAtMs: number
}

export type CronDeliveryBeginWireResult =
  | { outcome: 'acquired'; lease: CronDeliveryLeaseWire }
  | { outcome: 'accepted'; eventId: number; acceptedAtMs: number }
  | { outcome: 'busy'; eventId: number; retryAtMs: number }

type DeliveryBusinessEnvelope = {
  status?: string
  error?: string
  code?: string
  stateIdentity?: string
  expectedStateIdentity?: string | null
  stateIdentityMatches?: boolean | null
  ledgerEpoch?: string
  expectedLedgerEpoch?: string | null
  ledgerEpochMatches?: boolean | null
  outcome?: string
  eventId?: number
  generation?: string
  leaseToken?: string
  expiresAtMs?: number
  retryAtMs?: number
  acceptedAtMs?: number
}

function deliveryParams(input: {
  stateIdentity: string
  ledgerEpoch: string
}): {
  expectedStateIdentity: string
  expectedLedgerEpoch: string
} {
  return {
    expectedStateIdentity: input.stateIdentity,
    expectedLedgerEpoch: input.ledgerEpoch,
  }
}

function isDeliveryLeaseToken(value: unknown): value is string {
  return isUuidWire(value)
}

function assertVerifiedDeliveryNamespace(
  result: DeliveryBusinessEnvelope,
  expected: { stateIdentity: string; ledgerEpoch: string },
): void {
  if (
    result.stateIdentityMatches === false ||
    (typeof result.stateIdentity === 'string' &&
      result.stateIdentity !== expected.stateIdentity)
  ) {
    throw new CronStateIdentityError(
      'state_identity_mismatch',
      typeof result.stateIdentity === 'string' ? result.stateIdentity : null,
      expected.stateIdentity,
      'cron delivery response came from a different state identity',
    )
  }
  if (
    result.ledgerEpochMatches === false ||
    (typeof result.ledgerEpoch === 'string' &&
      result.ledgerEpoch !== expected.ledgerEpoch)
  ) {
    throw new CronLedgerEpochError(
      'ledger_epoch_mismatch',
      typeof result.ledgerEpoch === 'string' ? result.ledgerEpoch : null,
      expected.ledgerEpoch,
      'cron delivery response came from a different ledger epoch',
    )
  }
  if (
    result.stateIdentity !== expected.stateIdentity ||
    result.expectedStateIdentity !== expected.stateIdentity ||
    result.stateIdentityMatches !== true ||
    result.ledgerEpoch !== expected.ledgerEpoch ||
    result.expectedLedgerEpoch !== expected.ledgerEpoch ||
    result.ledgerEpochMatches !== true
  ) {
    throw new CronDaemonResponseError(
      'cron delivery response omitted verified namespace metadata',
    )
  }
}

function describeDeliveryResponse(result: DeliveryBusinessEnvelope): string {
  const { leaseToken: _secretFence, ...safe } = result
  return JSON.stringify(safe)
}

export type CronDeliveryBeginInput = {
  stateIdentity: string
  ledgerEpoch: string
  event: CronOutboxEvent
  consumer: CronDeliveryConsumerWire
  socketPath?: string
}

/** Pure request builder; it performs fence validation but no socket I/O. */
export function buildCronDeliveryBeginRpc(input: CronDeliveryBeginInput): {
  method: 'cron.delivery_begin'
  params: Record<string, unknown>
} {
  if (!isSafeCronEventId(input.event.eventId)) {
    throw new RangeError('cron delivery eventId must be a positive safe integer')
  }
  return {
    method: 'cron.delivery_begin',
    params: {
      ...deliveryParams(input),
      event: input.event,
      consumer: input.consumer,
    },
  }
}

/** Pure begin-response validator; it cannot open IPC or authorize delivery. */
export function parseCronDeliveryBeginResult(
  value: unknown,
  expected: { stateIdentity: string; ledgerEpoch: string; eventId: number },
): CronDeliveryBeginWireResult {
  if (!isRecord(value)) {
    throw new CronDaemonResponseError(
      'cron daemon returned a non-object delivery response',
    )
  }
  const result = value as DeliveryBusinessEnvelope
  assertVerifiedDeliveryNamespace(result, expected)
  unwrapBusinessOk(result)
  if (
    result.outcome === 'acquired' &&
    isSafeCronEventId(result.eventId) &&
    result.eventId === expected.eventId &&
    isCanonicalPositiveU64(result.generation) &&
    isDeliveryLeaseToken(result.leaseToken) &&
    isSafeEpochMs(result.expiresAtMs)
  ) {
    return {
      outcome: 'acquired',
      lease: {
        eventId: result.eventId,
        generation: result.generation,
        leaseToken: result.leaseToken,
        expiresAtMs: result.expiresAtMs,
      },
    }
  }
  if (
    result.outcome === 'accepted' &&
    isSafeCronEventId(result.eventId) &&
    result.eventId === expected.eventId &&
    isSafeEpochMs(result.acceptedAtMs)
  ) {
    return {
      outcome: 'accepted',
      eventId: result.eventId,
      acceptedAtMs: result.acceptedAtMs,
    }
  }
  if (
    result.outcome === 'busy' &&
    isSafeCronEventId(result.eventId) &&
    result.eventId === expected.eventId &&
    isSafeEpochMs(result.retryAtMs)
  ) {
    return {
      outcome: 'busy',
      eventId: result.eventId,
      retryAtMs: result.retryAtMs,
    }
  }
  throw new CronDeliveryStateError(
    'delivery_response_invalid',
    `cron.delivery_begin returned an invalid response: ${describeDeliveryResponse(result)}`,
  )
}

/** Ask the daemon's single ledger writer to grant one durable admission
 * lease. The full event is bound by the daemon on generation 1 and must match
 * on every later observation of the same `(epoch,eventId)`. */
export async function cronDeliveryBegin(
  input: CronDeliveryBeginInput,
): Promise<CronDeliveryBeginWireResult> {
  const rpc = buildCronDeliveryBeginRpc(input)
  const result = await callCronDaemon<DeliveryBusinessEnvelope>(
    rpc.method,
    rpc.params,
    { socketPath: input.socketPath },
  )
  return parseCronDeliveryBeginResult(result, {
    stateIdentity: input.stateIdentity,
    ledgerEpoch: input.ledgerEpoch,
    eventId: input.event.eventId,
  })
}

export type CronDeliveryFinalizeWireResult =
  | { outcome: 'accepted'; eventId: number; acceptedAtMs: number }
  | { outcome: 'abandoned'; eventId: number }
  | { outcome: 'fenced'; eventId: number; reason?: string }
  | { outcome: 'absent'; eventId: number }

export type CronDeliveryFinalizeInput = {
  stateIdentity: string
  ledgerEpoch: string
  lease: CronDeliveryLeaseWire
  socketPath?: string
}

export type CronDeliveryFinalizeMethod =
  | 'cron.delivery_accept'
  | 'cron.delivery_abandon'

/** Pure finalization request builder; validates the full lease fence. */
export function buildCronDeliveryFinalizeRpc(
  method: CronDeliveryFinalizeMethod,
  input: CronDeliveryFinalizeInput,
): { method: CronDeliveryFinalizeMethod; params: Record<string, unknown> } {
  if (
    !isSafeCronEventId(input.lease.eventId) ||
    !isCanonicalPositiveU64(input.lease.generation) ||
    !isDeliveryLeaseToken(input.lease.leaseToken)
  ) {
    throw new RangeError('cron delivery lease is not a valid wire fence')
  }
  return {
    method,
    params: {
      ...deliveryParams(input),
      eventId: input.lease.eventId,
      generation: input.lease.generation,
      leaseToken: input.lease.leaseToken,
    },
  }
}

/** Pure finalization-response validator; it cannot open IPC or accept work. */
export function parseCronDeliveryFinalizeResult(
  method: CronDeliveryFinalizeMethod,
  value: unknown,
  expected: { stateIdentity: string; ledgerEpoch: string; eventId: number },
): CronDeliveryFinalizeWireResult {
  if (!isRecord(value)) {
    throw new CronDaemonResponseError(
      'cron daemon returned a non-object delivery response',
    )
  }
  const result = value as DeliveryBusinessEnvelope & { reason?: string }
  assertVerifiedDeliveryNamespace(result, expected)
  unwrapBusinessOk(result)
  if (
    (result.outcome === 'accepted' ||
      result.outcome === 'abandoned' ||
      result.outcome === 'fenced' ||
      result.outcome === 'absent') &&
    isSafeCronEventId(result.eventId) &&
    result.eventId === expected.eventId
  ) {
    if (result.outcome === 'accepted') {
      if (!isSafeEpochMs(result.acceptedAtMs)) {
        throw new CronDeliveryStateError(
          'delivery_response_invalid',
          `${method} omitted acceptedAtMs`,
        )
      }
      return {
        outcome: 'accepted',
        eventId: result.eventId,
        acceptedAtMs: result.acceptedAtMs,
      }
    }
    if (result.outcome === 'fenced') {
      return {
        outcome: 'fenced',
        eventId: result.eventId,
        ...(typeof result.reason === 'string' ? { reason: result.reason } : {}),
      }
    }
    return { outcome: result.outcome, eventId: result.eventId }
  }
  throw new CronDeliveryStateError(
    'delivery_response_invalid',
    `${method} returned an invalid response: ${describeDeliveryResponse(result)}`,
  )
}

async function cronDeliveryFinalize(
  method: CronDeliveryFinalizeMethod,
  input: CronDeliveryFinalizeInput,
): Promise<CronDeliveryFinalizeWireResult> {
  const rpc = buildCronDeliveryFinalizeRpc(method, input)
  const result = await callCronDaemon<DeliveryBusinessEnvelope & { reason?: string }>(
    rpc.method,
    rpc.params,
    { socketPath: input.socketPath },
  )
  return parseCronDeliveryFinalizeResult(method, result, {
    stateIdentity: input.stateIdentity,
    ledgerEpoch: input.ledgerEpoch,
    eventId: input.lease.eventId,
  })
}

export function cronDeliveryAccept(
  input: CronDeliveryFinalizeInput,
): Promise<CronDeliveryFinalizeWireResult> {
  return cronDeliveryFinalize('cron.delivery_accept', input)
}

export function cronDeliveryAbandon(
  input: CronDeliveryFinalizeInput,
): Promise<CronDeliveryFinalizeWireResult> {
  return cronDeliveryFinalize('cron.delivery_abandon', input)
}

/**
 * Subscribe to push-based cron fire events (Push 模型).
 *
 * Wire protocol (matches `crates/crabcode-cron/src/ipc.rs`:
 *   send: {"id":"sub-XXX","method":"cron.subscribe","params":{"expectedStateIdentity":"cron-state-v1:..."}} + "\n"
 *   ack:  {"id":"sub-XXX","result":{"status":"ok","subscribed":true,"stateIdentity":"cron-state-v1:...","stateIdentityMatches":true}} + "\n"
 *   then: every fire emits one line `{"type":"cron.fired","job":{...}}` until close
 *
 * The connection stays open for the lifetime of the subscription. Caller iterates
 * `events`; calling `close()` (or process exit) terminates the iterator. Daemon
 * shutdown / EOF also terminates with `done:true`.
 *
 * Failure semantics:
 * - Connect/EOF before ack → throws CronDaemonConnectError from this Promise.
 * - Protocol error in ack frame → throws CronDaemonProtocolError.
 * - State identity mismatch in ack → throws CronStateIdentityError.
 * - Ack timeout → throws CronDaemonTimeoutError.
 * - Mid-subscription socket error → iterator yields done:true; caller should
 *   reconnect with backoff (and may use `cronPollEvents` to bridge gap).
 */
export type CronSubscription = {
  events: AsyncIterable<CronFiredEvent>
  close: () => void
}

/** Pure subscription request builder; it cannot open the long-lived socket. */
export function buildCronSubscriptionRpc(
  socketPath: string,
  requestId: string,
): {
  id: string
  method: 'cron.subscribe'
  params: { expectedStateIdentity: string }
} {
  return {
    id: requestId,
    method: 'cron.subscribe',
    params: {
      expectedStateIdentity: resolveCronStateIdentity(socketPath),
    },
  }
}

/** Pure ACK business validator; it cannot subscribe or consume events. */
export function parseCronSubscriptionAckResult(value: unknown): void {
  if (!isRecord(value)) {
    throw new CronDaemonConnectError(
      'subscribe: unexpected non-object ack result',
    )
  }
  const result = value as {
    status?: string
    subscribed?: boolean
    error?: string
  } & CronStateIdentityMetadata
  if (result.subscribed === true) return
  if (
    result.code === 'state_identity_required' ||
    result.code === 'state_identity_mismatch'
  ) {
    throw new CronStateIdentityError(
      result.code,
      result.stateIdentity ?? null,
      result.expectedStateIdentity ?? null,
      `cron daemon: ${result.error ?? result.code}`,
    )
  }
  throw new CronDaemonConnectError('subscribe: unexpected ack result')
}

export async function subscribeCronEvents(opts?: {
  socketPath?: string
  ackTimeoutMs?: number
}): Promise<CronSubscription> {
  assertCronDaemonMethodReleased('cron.subscribe', null)
  const socketPath = opts?.socketPath ?? getCronSocketPath()
  const ackTimeoutMs = opts?.ackTimeoutMs ?? CRON_DAEMON_DEFAULT_TIMEOUT_MS
  const subId = `sub-${randomUUID().slice(0, 8)}`
  const subscribeReq =
    JSON.stringify(buildCronSubscriptionRpc(socketPath, subId)) + '\n'

  // See `callCronDaemon`: the optional daemon socket may be absent, and Bun
  // can report that synchronously from `connect`.
  const socket = new Socket()
  socket.setEncoding('utf-8')

  const queue: CronFiredEvent[] = []
  let waker: (() => void) | null = null
  let closed = false

  const wake = () => {
    const w = waker
    waker = null
    if (w) w()
  }

  let ackReceived = false
  let ackResolve: (() => void) | null = null
  let ackReject: ((err: Error) => void) | null = null

  const rejectConnect = (err: unknown) => {
    if (!ackReceived) {
      ackReceived = true
      ackReject?.(
        new CronDaemonConnectError(
          `subscribe connect error at ${socketPath}: ${
            err instanceof Error ? err.message : String(err)
          }`,
          err,
        ),
      )
    }
    closed = true
    wake()
  }

  socket.once('error', rejectConnect)

  const reader = createInterface({ input: socket, crlfDelay: Infinity })
  reader.once('error', rejectConnect)

  reader.on('line', (line: string) => {
    const raw = line.trim()
    if (raw.length === 0) return
    let parsed: Record<string, unknown>
    try {
      parsed = JSON.parse(raw) as Record<string, unknown>
    } catch (e) {
      if (!ackReceived) {
        ackReceived = true
        ackReject?.(
          new CronDaemonConnectError(
            `subscribe: non-JSON ack: ${raw.slice(0, 120)}`,
            e,
          ),
        )
      }
      return
    }
    if (!ackReceived) {
      ackReceived = true
      if (parsed.error !== undefined) {
        ackReject?.(new CronDaemonProtocolError(parsed.error as CronDaemonError))
      } else {
        try {
          parseCronSubscriptionAckResult(parsed.result)
          ackResolve?.()
        } catch (error) {
          ackReject?.(
            error instanceof Error ? error : new Error(String(error)),
          )
        }
      }
      return
    }
    // Post-ack: each line is a fire event. Daemon only pushes `cron.fired`,
    // but be defensive — drop unknown shapes silently.
    if (parsed.type === 'cron.fired') {
      queue.push(parsed as unknown as CronFiredEvent)
      wake()
    }
  })

  reader.once('close', () => {
    if (!ackReceived) {
      ackReceived = true
      ackReject?.(new CronDaemonConnectError('subscribe: closed before ack'))
    }
    closed = true
    wake()
  })

  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      if (!ackReceived) {
        ackReceived = true
        socket.destroy()
        reject(new CronDaemonTimeoutError(ackTimeoutMs))
      }
    }, ackTimeoutMs)
    socket.once('connect', () => {
      socket.write(subscribeReq)
    })
    ackResolve = () => {
      clearTimeout(timer)
      resolve()
    }
    ackReject = err => {
      clearTimeout(timer)
      reject(err)
    }
    try {
      socket.connect({ path: socketPath })
    } catch (err) {
      try {
        socket.destroy()
      } catch {
        // best-effort
      }
      rejectConnect(err)
    }
  })

  const events: AsyncIterable<CronFiredEvent> = {
    [Symbol.asyncIterator]() {
      return {
        async next(): Promise<IteratorResult<CronFiredEvent>> {
          for (;;) {
            const buffered = queue.shift()
            if (buffered !== undefined) {
              return { value: buffered, done: false }
            }
            if (closed) {
              return { value: undefined as unknown as CronFiredEvent, done: true }
            }
            await new Promise<void>(resolve => {
              waker = resolve
            })
          }
        },
      }
    },
  }

  return {
    events,
    close: () => {
      closed = true
      try {
        socket.destroy()
      } catch {
        // best-effort
      }
      wake()
    },
  }
}
