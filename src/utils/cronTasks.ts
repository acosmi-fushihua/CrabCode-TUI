// Scheduled prompts.
//
// Storage split:
//   - durable=true  → planned crabcode-cron daemon path (Rust
//     SchedulerDaemon). Creation is currently compile-time blocked until the
//     occurrence live-consumer cutover; read/remove cleanup remains available.
//   - durable=false → in-process session store (bootstrap/state.ts), dies
//     with the process. Daemon never sees session tasks.
//
// History (pre-2026-05-06): durable tasks were written directly to
// <project>/.crabcode/scheduled_tasks.json by TS. The Rust daemon read from
// a different path (<state_dir>/.crabcode/scheduled_tasks.json), so the
// scheduling chain was broken end-to-end. Switched to IPC so daemon is the
// single source of truth.
//
// Tasks still come in two flavors:
//   - One-shot (recurring: false/undefined) — fire once, then auto-delete.
//   - Recurring (recurring: true) — fire on schedule, reschedule from now,
//     persist until explicitly deleted via CronDelete or auto-expire after
//     a configurable limit (DEFAULT_CRON_JITTER_CONFIG.recurringMaxAgeMs).

import { randomUUID } from 'crypto'
import { readFileSync } from 'fs'
import { mkdir, writeFile } from 'fs/promises'
import { join } from 'path'
import {
  addSessionCronTask,
  getCurrentTurnThreadId,
  getCwdState,
  getProjectRoot,
  getSessionId,
  getSessionOverrideProjectRootCwd,
  getSessionCronTasks,
  removeSessionCronTasks,
} from '../bootstrap/state.js'
import { DURABLE_CRON_LIVE_CONSUMER_READY } from '../tools/ScheduleCronTool/constants.js'
import { computeNextCronRun, parseCronExpression } from './cron.js'
import {
  type CronJobView,
  cronAdd as daemonCronAdd,
  cronList as daemonCronList,
  cronRemove as daemonCronRemove,
  CronDaemonConnectError,
} from './cronDaemonClient.js'
import { ensureCronDaemon } from './cronEnsure.js'
import { logForDebugging } from './debug.js'
import { isFsInaccessible } from './errors.js'
import { getFsImplementation } from './fsOperations.js'
import { safeParseJSON } from './json.js'
import { logError } from './log.js'
import { jsonStringify } from './slowOperations.js'

export type CronTask = {
  id: string
  /**
   * 5-field cron string (local time) — validated on write, re-validated on
   * read. Empty string (`''`) for `at` (absolute-time) jobs, which carry no
   * cron expression; the field is kept present so cron-only consumers don't
   * crash. Inspect {@link CronTask.scheduleKind} to disambiguate.
   */
  cron: string
  /**
   * Schedule kind of the underlying daemon job. Absent ⇒ treat as `'cron'`
   * (back-compat: pre-RC3 tasks and all session-store tasks have no kind).
   * `'at'` jobs are one-shot absolute-time schedules — see {@link CronTask.at}.
   */
  scheduleKind?: 'cron' | 'at' | 'every'
  /**
   * ISO-8601 absolute fire time. Set ONLY for `at` jobs (scheduleKind 'at');
   * absent for cron jobs. P1 made `At` schedules always one-shot, so an
   * `at` task always has `recurring` false/absent.
   */
  at?: string
  /** Exact native interval/anchor for scheduleKind `every`. */
  everyMs?: number
  anchorMs?: number
  /** Prompt to enqueue when the task fires. */
  prompt: string
  /** Epoch ms when the task was created. Anchor for missed-task detection. */
  createdAt: number
  /**
   * Epoch ms of the most recent fire. Written back by the scheduler after
   * each recurring fire so next-fire computation survives process restarts.
   * The scheduler anchors first-sight from `lastFiredAt ?? createdAt` — a
   * never-fired task uses createdAt (correct for pinned crons like
   * `30 14 27 2 *` whose next-from-now is next year); a fired-before task
   * reconstructs the same `nextFireAt` the prior process had in memory.
   * Never set for one-shots (they're deleted on fire).
   */
  lastFiredAt?: number
  /** When true, the task reschedules after firing instead of being deleted. */
  recurring?: boolean
  /**
   * When true, the task is exempt from recurringMaxAgeMs auto-expiry.
   * Not settable via CronCreateTool; it only enters this file when written
   * directly to scheduled_tasks.json. (The historical writer,
   * src/assistant/install.ts, no longer exists.)
   *
   * This module is the per-project session
   * lane — `<project>/.crabcode/scheduled_tasks.json`, a normal
   * project-local dir like `.git`. It is distinct from the crabcode-cron
   * daemon's state-dir lane (rooted at resolve_state_dir(), reached from TS
   * only via IPC — see cronDaemonClient.ts); the daemon lane's on-disk
   * layout is enforced by Rust `layout_contract` tests.
   */
  permanent?: boolean
  /**
   * Runtime-only flag. false → session-scoped (never written to disk).
   * File-backed tasks leave this undefined; writeCronTasks strips it so
   * the on-disk shape stays { id, cron, prompt, createdAt, lastFiredAt?, recurring?, permanent? }.
   */
  durable?: boolean
  /**
   * Runtime-only. When set, the task was created by an in-process teammate.
   * The scheduler routes fires to that teammate's queue instead of the main
   * REPL's. Never written to disk (teammate crons are always session-only).
   */
  agentId?: string
}

type CronFile = { tasks: CronTask[] }

const CRON_FILE_REL = join('.crabcode', 'scheduled_tasks.json')

/**
 * Path to the cron file. `dir` defaults to getProjectRoot() — pass it
 * explicitly from contexts that don't run through main.tsx (e.g. the Agent
 * SDK daemon, which has no bootstrap state).
 */
export function getCronFilePath(dir?: string): string {
  return join(dir ?? getProjectRoot(), CRON_FILE_REL)
}

/**
 * Read and parse .crabcode/scheduled_tasks.json. Returns an empty task list if the file
 * is missing, empty, or malformed. Tasks with invalid cron strings are
 * silently dropped (logged at debug level) so a single bad entry never
 * blocks the whole file.
 */
export async function readCronTasks(dir?: string): Promise<CronTask[]> {
  const fs = getFsImplementation()
  let raw: string
  try {
    raw = await fs.readFile(getCronFilePath(dir), { encoding: 'utf-8' })
  } catch (e: unknown) {
    if (isFsInaccessible(e)) return []
    logError(e)
    return []
  }

  const parsed = safeParseJSON(raw, false)
  if (!parsed || typeof parsed !== 'object') return []
  const file = parsed as Partial<CronFile>
  if (!Array.isArray(file.tasks)) return []

  const out: CronTask[] = []
  for (const t of file.tasks) {
    if (
      !t ||
      typeof t.id !== 'string' ||
      typeof t.cron !== 'string' ||
      typeof t.prompt !== 'string' ||
      typeof t.createdAt !== 'number'
    ) {
      logForDebugging(
        `[ScheduledTasks] skipping malformed task: ${jsonStringify(t)}`,
      )
      continue
    }
    if (!parseCronExpression(t.cron)) {
      logForDebugging(
        `[ScheduledTasks] skipping task ${t.id} with invalid cron '${t.cron}'`,
      )
      continue
    }
    out.push({
      id: t.id,
      cron: t.cron,
      prompt: t.prompt,
      createdAt: t.createdAt,
      ...(typeof t.lastFiredAt === 'number'
        ? { lastFiredAt: t.lastFiredAt }
        : {}),
      ...(t.recurring ? { recurring: true } : {}),
      ...(t.permanent ? { permanent: true } : {}),
    })
  }
  return out
}

/**
 * Sync check for whether the cron file has any valid tasks. Used by
 * cronScheduler.start() to decide whether to auto-enable. One file read.
 */
export function hasCronTasksSync(dir?: string): boolean {
  let raw: string
  try {
    // eslint-disable-next-line custom-rules/no-sync-fs -- called once from cronScheduler.start()
    raw = readFileSync(getCronFilePath(dir), 'utf-8')
  } catch {
    return false
  }
  const parsed = safeParseJSON(raw, false)
  if (!parsed || typeof parsed !== 'object') return false
  const tasks = (parsed as Partial<CronFile>).tasks
  return Array.isArray(tasks) && tasks.length > 0
}

/**
 * Overwrite .crabcode/scheduled_tasks.json with the given tasks. Creates .crabcode/ if
 * missing. Empty task list writes an empty file (rather than deleting) so
 * the file watcher sees a change event on last-task-removed.
 */
export async function writeCronTasks(
  tasks: CronTask[],
  dir?: string,
): Promise<void> {
  const root = dir ?? getProjectRoot()
  await mkdir(join(root, '.crabcode'), { recursive: true })
  // Strip the runtime-only `durable` flag — everything on disk is durable
  // by definition, and keeping the flag out means readCronTasks() naturally
  // yields durable: undefined without having to set it explicitly.
  const body: CronFile = {
    tasks: tasks.map(({ durable: _durable, ...rest }) => rest),
  }
  await writeFile(
    getCronFilePath(root),
    jsonStringify(body, null, 2) + '\n',
    'utf-8',
  )
}

/**
 * `daemonCronAdd` with a single cold-start ensure+retry (W-CRON P4 / RC2).
 *
 * On `CronDaemonConnectError` the daemon is not running; invoke the active
 * process entry's Rust lifecycle command once (the shared ensure contract —
 * P3), then retry the add exactly once. If ensure is unavailable or the retry
 * still fails, the error propagates so the caller surfaces a typed tool
 * failure rather than silently dropping the task.
 */
async function daemonCronAddWithEnsure(
  input: Parameters<typeof daemonCronAdd>[0],
): Promise<{ id: string }> {
  try {
    return await daemonCronAdd(input)
  } catch (e) {
    if (!(e instanceof CronDaemonConnectError)) throw e
    const ensured = await ensureCronDaemon()
    if (!ensured) throw e
    return await daemonCronAdd(input)
  }
}

/**
 * Append a task. Returns the generated id.
 *
 * When `durable` is false the task is held in process memory only
 * (bootstrap/state.ts) — fires this session, dies with the process.
 *
 * When `durable` is true the current PR-6 compile-time release boundary throws
 * before daemon I/O. After the explicit cutover flip, the task is sent to the
 * crabcode-cron daemon; the daemon owns persistence and returns its own id.
 * A connection failure propagates and must never downgrade to session-only.
 *
 * `at` (ISO-8601, mutually exclusive with `cron`) routes to daemon
 * `kind:"at"` schedule with `deleteAfterRun:true`. `at` requires `durable:true`
 * — session-only one-shot at would need session-store schema upgrade for
 * absolute-time scheduling and is out of scope here. Caller must ensure
 * exactly one of {`cron`, `at`} is non-empty.
 */
export async function addCronTask(
  cron: string,
  prompt: string,
  recurring: boolean,
  durable: boolean,
  agentId?: string,
  at?: string,
  options: {
    everyMs?: number
    anchorMs?: number
    sessionTarget?: 'continuation' | 'main' | 'isolated'
    wakeMode?: 'now' | 'next-heartbeat'
    expiresAtMs?: number
  } = {},
): Promise<string> {
  const canonicalPrompt = prompt.trim()
  if (!canonicalPrompt) {
    throw new Error('cron prompt must contain non-whitespace text')
  }
  if (durable && !DURABLE_CRON_LIVE_CONSUMER_READY) {
    throw new Error(
      'durable cron is unavailable until the exact-target live-consumer cutover is released',
    )
  }
  const hasEvery = options.everyMs !== undefined
  if (at && hasEvery) {
    throw new Error('`at` and `everyMs` are mutually exclusive')
  }
  if (hasEvery && (!Number.isSafeInteger(options.everyMs) || options.everyMs! < 1000)) {
    throw new Error('everyMs must be an integer >= 1000')
  }
  const sessionTarget = options.sessionTarget ?? 'continuation'
  const wakeMode = options.wakeMode ?? 'next-heartbeat'
  if (sessionTarget === 'isolated') {
    throw new Error(
      'unsupported schedule target: isolated execution is not implemented',
    )
  }
  const ownerThreadId =
    getCurrentTurnThreadId() ?? `session:${String(getSessionId())}`
  const project = getSessionOverrideProjectRootCwd() ?? getProjectRoot()
  const cwd = getSessionOverrideProjectRootCwd() ?? getCwdState()
  const connectionId = `tui:${process.pid}:${String(getSessionId())}`
  const surface = 'tui' as const
  const executionTarget = {
    version: 1,
    ownerThreadId,
    project,
    cwd,
    connectionId,
    surface,
  } as const

  if (at) {
    if (!durable) {
      throw new Error(
        '`at` schedule requires durable:true (session-only at not implemented)',
      )
    }
    const { id } = await daemonCronAddWithEnsure({
      name: canonicalPrompt.slice(0, 64),
      schedule: { kind: 'at', at },
      payload: { message: canonicalPrompt },
      enabled: true,
      deleteAfterRun: true,
      sessionTarget,
      wakeMode,
      sessionKey: JSON.stringify(executionTarget),
    })
    return id
  }
  if (!durable) {
    // Session-only: keep TS-side short id, no daemon involvement.
    const id = randomUUID().slice(0, 8)
    const createdAt = Date.now()
    const task = {
      id,
      cron: hasEvery ? '' : cron,
      scheduleKind: hasEvery ? ('every' as const) : ('cron' as const),
      ...(hasEvery
        ? {
            everyMs: options.everyMs!,
            anchorMs: options.anchorMs ?? createdAt,
          }
        : {}),
      prompt: canonicalPrompt,
      createdAt,
      ...(recurring || hasEvery ? { recurring: true } : {}),
      ownerThreadId,
      project,
      cwd,
      connectionId,
      surface,
      sessionTarget,
      wakeMode,
      expiresAtMs:
        options.expiresAtMs ??
        createdAt + DEFAULT_CRON_JITTER_CONFIG.recurringMaxAgeMs,
      retryCount: 0,
      deadLetter: false,
    }
    addSessionCronTask({ ...task, ...(agentId ? { agentId } : {}) })
    return id
  }
  // Durable: daemon owns the lifecycle, returns its own id.
  const { id } = await daemonCronAddWithEnsure({
    name: canonicalPrompt.slice(0, 64),
    schedule: hasEvery
      ? {
          kind: 'every',
          everyMs: options.everyMs!,
          anchorMs: options.anchorMs ?? Date.now(),
        }
      : { kind: 'cron', expr: cron },
    payload: { message: canonicalPrompt },
    enabled: true,
    deleteAfterRun: hasEvery ? false : !recurring,
    sessionTarget,
    wakeMode,
    sessionKey: JSON.stringify(executionTarget),
  })
  return id
}

/**
 * Remove tasks by id. No-op if none match (e.g. another session raced us).
 * Used for both fire-once cleanup and explicit CronDelete.
 *
 * Sweeps session store first; any id not found there is sent to the daemon
 * (one IPC call per id; we don't batch because the daemon has no batch
 * remove and the count is bounded by MAX_JOBS=50).
 *
 * `dir` parameter is preserved for legacy callers but is now a no-op (the
 * daemon owns durable storage; there is no per-project file to target).
 */
export async function removeCronTasks(
  ids: string[],
  _dir?: string,
): Promise<void> {
  if (ids.length === 0) return
  const remaining = ids.filter(id => removeSessionCronTasks([id]) === 0)
  if (remaining.length === 0) return
  // Daemon-owned durable tasks. Errors propagate; CronDelete tool maps to bail.
  for (const id of remaining) {
    try {
      await daemonCronRemove(id)
    } catch (e) {
      // NotFound is benign (race against fire-once cleanup); only re-throw
      // for connect / protocol errors.
      if (e instanceof CronDaemonConnectError) throw e
      const msg = (e as Error).message ?? ''
      if (!msg.includes('not found') && !msg.includes('cron job not found')) {
        throw e
      }
    }
  }
}

/**
 * Stamp `lastFiredAt` on the given recurring tasks and write back. Batched
 * so N fires in one scheduler tick = one read-modify-write, not N. Only
 * touches file-backed tasks — session tasks die with the process, no point
 * persisting their fire time. No-op if none of the ids match (task was
 * deleted between fire and write — e.g. user ran CronDelete mid-tick).
 *
 * Scheduler lock means at most one process calls this; chokidar picks up
 * the write and triggers a reload which re-seeds `nextFireAt` from the
 * just-written `lastFiredAt` — idempotent (same computation, same answer).
 */
export async function markCronTasksFired(
  ids: string[],
  firedAt: number,
  dir?: string,
): Promise<void> {
  if (ids.length === 0) return
  const idSet = new Set(ids)
  const tasks = await readCronTasks(dir)
  let changed = false
  for (const t of tasks) {
    if (idSet.has(t.id)) {
      t.lastFiredAt = firedAt
      changed = true
    }
  }
  if (!changed) return
  await writeCronTasks(tasks, dir)
}

/**
 * Daemon-managed durable tasks + in-memory session tasks, merged.
 *
 * Daemon connect/protocol errors degrade to "no durable tasks visible" rather
 * than tool failure — listing should still show session tasks even if the
 * daemon is down. (CronCreate/CronDelete still bubble daemon errors because
 * those mutate.)
 */
export async function listAllCronTasks(_dir?: string): Promise<CronTask[]> {
  const sessionTasks = getSessionCronTasks().map(t => ({
    ...t,
    durable: false as const,
  }))
  let durableTasks: CronTask[] = []
  try {
    const jobs = await daemonCronList({ includeDisabled: true })
    durableTasks = jobs.map(jobToTask).filter((t): t is CronTask => t !== null)
  } catch (e) {
    if (e instanceof CronDaemonConnectError) {
      logForDebugging(`[ScheduledTasks] cron daemon not reachable: ${e.message}`)
    } else {
      logError(e)
    }
  }
  return [...durableTasks, ...sessionTasks]
}

/**
 * Map daemon CronJob → TS CronTask (best-effort).
 *
 * Handles `cron`, `at`, and native `every` schedule kinds without rounding.
 */
function jobToTask(job: CronJobView): CronTask | null {
  const kind = job.schedule?.kind
  if (kind !== 'cron' && kind !== 'at' && kind !== 'every') {
    return null
  }
  const message =
    typeof job.payload?.message === 'string' ? job.payload.message.trim() : ''
  const text =
    typeof job.payload?.text === 'string' ? job.payload.text.trim() : ''
  const prompt = message || text
  if (!prompt) return null
  const id = typeof job.id === 'string' ? job.id : ''
  if (!id) return null
  // daemon CronJob has `createdAtMs`; if absent fall back to now (rare during transition).
  const rawCreatedAt = (job as Record<string, unknown>).createdAtMs
  const createdAt = typeof rawCreatedAt === 'number' ? rawCreatedAt : Date.now()

  if (kind === 'at') {
    // `at` jobs are always one-shot (P1: ScheduleKind::At 天然 one-shot) —
    // never `recurring`. `cron` is left empty (no expression) but kept
    // present so cron-only consumers don't crash; do NOT fabricate one.
    if (typeof job.schedule?.at !== 'string' || job.schedule.at.length === 0) {
      return null
    }
    return {
      id,
      cron: '',
      scheduleKind: 'at',
      at: job.schedule.at,
      prompt,
      createdAt,
      ...(job.permanent ? { permanent: true } : {}),
    }
  }

  if (kind === 'every') {
    if (
      typeof job.schedule?.everyMs !== 'number' ||
      !Number.isSafeInteger(job.schedule.everyMs) ||
      job.schedule.everyMs < 1000
    ) {
      return null
    }
    const anchorRaw = job.schedule?.anchorMs
    return {
      id,
      cron: '',
      scheduleKind: 'every',
      everyMs: job.schedule.everyMs,
      anchorMs:
        typeof anchorRaw === 'number' && Number.isSafeInteger(anchorRaw)
          ? anchorRaw
          : createdAt,
      prompt,
      createdAt,
      recurring: true,
      ...(job.permanent ? { permanent: true } : {}),
    }
  }

  // kind === 'cron'
  if (typeof job.schedule?.expr !== 'string') {
    return null
  }
  const recurring = (job as Record<string, unknown>).deleteAfterRun !== true
  return {
    id,
    cron: job.schedule.expr,
    scheduleKind: 'cron',
    prompt,
    createdAt,
    ...(recurring ? { recurring: true } : {}),
    ...(job.permanent ? { permanent: true } : {}),
  }
}

/**
 * Next fire time in epoch ms for a cron string, strictly after `fromMs`.
 * Returns null if invalid or no match in the next 366 days.
 */
export function nextCronRunMs(cron: string, fromMs: number): number | null {
  const fields = parseCronExpression(cron)
  if (!fields) return null
  const next = computeNextCronRun(fields, new Date(fromMs))
  return next ? next.getTime() : null
}

/**
 * Cron scheduler tuning knobs. Sourced at runtime from the
 * `tengu_kairos_cron_config` GrowthBook JSON config (see cronJitterConfig.ts)
 * so ops can adjust behavior fleet-wide without shipping a client build.
 * Defaults here preserve the pre-config behavior exactly.
 */
export type CronJitterConfig = {
  /** Recurring-task forward delay as a fraction of the interval between fires. */
  recurringFrac: number
  /** Upper bound on recurring forward delay regardless of interval length. */
  recurringCapMs: number
  /** One-shot backward lead: maximum ms a task may fire early. */
  oneShotMaxMs: number
  /**
   * One-shot backward lead: minimum ms a task fires early when the minute-mod
   * gate matches. 0 = taskIds hashing near zero fire on the exact mark. Raise
   * this to guarantee nobody lands on the wall-clock boundary.
   */
  oneShotFloorMs: number
  /**
   * Jitter fires landing on minutes where `minute % N === 0`. 30 → :00/:30
   * (the human-rounding hotspots). 15 → :00/:15/:30/:45. 1 → every minute.
   */
  oneShotMinuteMod: number
  /**
   * Recurring tasks auto-expire this many ms after creation (unless marked
   * `permanent`). Cron is the primary driver of multi-day sessions (p99
   * uptime 61min → 53h post-#19931), and unbounded recurrence lets Tier-1
   * heap leaks compound indefinitely. The default (7 days) covers "check
   * my PRs every hour this week" workflows while capping worst-case
   * session lifetime. Permanent tasks (assistant mode's catch-up/
   * morning-checkin/dream) never age out — they can't be recreated if
   * deleted because install.ts's writeIfMissing() skips existing files.
   *
   * `0` = unlimited (tasks never auto-expire).
   */
  recurringMaxAgeMs: number
}

export const DEFAULT_CRON_JITTER_CONFIG: CronJitterConfig = {
  recurringFrac: 0.1,
  recurringCapMs: 15 * 60 * 1000,
  oneShotMaxMs: 90 * 1000,
  oneShotFloorMs: 0,
  oneShotMinuteMod: 30,
  recurringMaxAgeMs: 7 * 24 * 60 * 60 * 1000,
}

/**
 * taskId is an 8-hex-char UUID slice (see {@link addCronTask}) → parse as
 * u32 → [0, 1). Stable across restarts, uniformly distributed across the
 * fleet. Non-hex ids (hand-edited JSON) fall back to 0 = no jitter.
 */
function jitterFrac(taskId: string): number {
  const frac = parseInt(taskId.slice(0, 8), 16) / 0x1_0000_0000
  return Number.isFinite(frac) ? frac : 0
}

/**
 * Same as {@link nextCronRunMs}, plus a deterministic per-task delay to
 * avoid a thundering herd when many sessions schedule the same cron string
 * (e.g. `0 * * * *` → everyone hits inference at :00).
 *
 * The delay is proportional to the current gap between fires
 * ({@link CronJitterConfig.recurringFrac}, capped at
 * {@link CronJitterConfig.recurringCapMs}) so at defaults an hourly task
 * spreads across [:00, :06) but a per-minute task only spreads by a few
 * seconds.
 *
 * Only used for recurring tasks. One-shot tasks use
 * {@link oneShotJitteredNextCronRunMs} (backward jitter, minute-gated).
 */
export function jitteredNextCronRunMs(
  cron: string,
  fromMs: number,
  taskId: string,
  cfg: CronJitterConfig = DEFAULT_CRON_JITTER_CONFIG,
): number | null {
  const t1 = nextCronRunMs(cron, fromMs)
  if (t1 === null) return null
  const t2 = nextCronRunMs(cron, t1)
  // No second match in the next year (e.g. pinned date) → nothing to
  // proportion against, and near-certainly not a herd risk. Fire on t1.
  if (t2 === null) return t1
  const jitter = Math.min(
    jitterFrac(taskId) * cfg.recurringFrac * (t2 - t1),
    cfg.recurringCapMs,
  )
  return t1 + jitter
}

/**
 * Same as {@link nextCronRunMs}, minus a deterministic per-task lead time
 * when the fire time lands on a minute boundary matching
 * {@link CronJitterConfig.oneShotMinuteMod}.
 *
 * One-shot tasks are user-pinned ("remind me at 3pm") so delaying them
 * breaks the contract — but firing slightly early is invisible and spreads
 * the inference spike from everyone picking the same round wall-clock time.
 * At defaults (mod 30, max 90 s, floor 0) only :00 and :30 get jitter,
 * because humans round to the half-hour.
 *
 * During an incident, ops can push `tengu_kairos_cron_config` with e.g.
 * `{oneShotMinuteMod: 15, oneShotMaxMs: 300000, oneShotFloorMs: 30000}` to
 * spread :00/:15/:30/:45 fires across a [t-5min, t-30s] window — every task
 * gets at least 30 s of lead, so nobody lands on the exact mark.
 *
 * Checks the computed fire time rather than the cron string so
 * `0 15 * * *`, step expressions, and `0,30 9 * * *` all get jitter
 * when they land on a matching minute. Clamped to `fromMs` so a task created
 * inside its own jitter window doesn't fire before it was created.
 */
export function oneShotJitteredNextCronRunMs(
  cron: string,
  fromMs: number,
  taskId: string,
  cfg: CronJitterConfig = DEFAULT_CRON_JITTER_CONFIG,
): number | null {
  const t1 = nextCronRunMs(cron, fromMs)
  if (t1 === null) return null
  // Cron resolution is 1 minute → computed times always have :00 seconds,
  // so a minute-field check is sufficient to identify the hot marks.
  // getMinutes() (local), not getUTCMinutes(): cron is evaluated in local
  // time, and "user picked a round time" means round in *their* TZ. In
  // half-hour-offset zones (India UTC+5:30) local :00 is UTC :30 — the
  // UTC check would jitter the wrong marks.
  if (new Date(t1).getMinutes() % cfg.oneShotMinuteMod !== 0) return t1
  // floor + frac * (max - floor) → uniform over [floor, max). With floor=0
  // this reduces to the original frac * max. With floor>0, even a taskId
  // hashing to 0 gets `floor` ms of lead — nobody fires on the exact mark.
  const lead =
    cfg.oneShotFloorMs +
    jitterFrac(taskId) * (cfg.oneShotMaxMs - cfg.oneShotFloorMs)
  // t1 > fromMs is guaranteed by nextCronRunMs (strictly after), so the
  // max() only bites when the task was created inside its own lead window.
  return Math.max(t1 - lead, fromMs)
}

/**
 * A task is "missed" when its next scheduled run (computed from createdAt)
 * is in the past. Surfaced to the user at startup. Works for both one-shot
 * and recurring tasks — a recurring task whose window passed while CrabCode
 * was down is still "missed".
 */
export function findMissedTasks(tasks: CronTask[], nowMs: number): CronTask[] {
  return tasks.filter(t => {
    const next = nextCronRunMs(t.cron, t.createdAt)
    return next !== null && next < nowMs
  })
}

/**
 * Test-only surface. `jobToTask` is module-private (daemon CronJob → CronTask
 * mapping) — exported here so its `at` / `cron` / `every` handling can be
 * unit-tested directly. Mirrors the `_testables` pattern used in
 * `src/gateway/discovery.ts`. Not for production callers.
 */
export const _testables = { jobToTask }
