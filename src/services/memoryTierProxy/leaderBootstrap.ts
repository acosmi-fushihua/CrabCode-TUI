// Global leader election for the TUI memory Tier proxy. The orchestrator
// broadcasts unscoped work, so one account-level lease prevents duplicate
// execution and write-back. IPC failures are fail-soft and never crash TUI
// startup.
import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import { getGlobalExecutorLeaseDir } from '../../memdir/rustDerivedPaths.js'
import { getMembershipGateInput } from '../../utils/auth.js'
import { canUseKbPremium } from '../../utils/entitlements/knowledgeBase.js'
import { maybeRunPeriodicKnowledgeSync } from '../knowledgeSync/index.js'
import * as memoryRunners from '../memoryRunners/index.js'
import { startMemoryTierProxy, stopMemoryTierProxy } from './index.js'
import {
  startMemoryTierToolProxy,
  stopMemoryTierToolProxy,
} from './toolIndex.js'

function positiveSafeDurationFromEnv(name: string, fallback: number): number {
  const raw = process.env[name]
  if (raw === undefined) return fallback
  const parsed = Number(raw)
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : fallback
}

const LEADER_TTL_MS = positiveSafeDurationFromEnv(
  'CRABCODE_MEMORY_LEADER_TTL_MS',
  60_000,
)
const LEADER_RENEW_INTERVAL_MS = positiveSafeDurationFromEnv(
  'CRABCODE_MEMORY_LEADER_RENEW_MS',
  30_000,
)
const MAX_TIMER_DELAY_MS = 2_147_483_647

type LeaderLease = {
  leaderToken: string
  leaderEpoch: number
  leaseExpiresAtMs: number
}

let leaderHeld = false
let leaderMemoryDir: string | null = null
let leaderToken: string | null = null
let leaderEpoch: number | null = null
let leaderLeaseExpiresAtMs: number | null = null
let leaderGeneration = 0
let leaderRenewTimer: ReturnType<typeof setInterval> | null = null
let leaderWatchdogTimer: ReturnType<typeof setInterval> | null = null
let leaderExpiryTimer: ReturnType<typeof setTimeout> | null = null
let leaderRenewInFlight = false
let leaderWatchdogInFlight = false
let runnersInitOnce = false

function logStderr(msg: string): void {
  process.stderr.write(`[memory-leader] ${msg}\n`)
}

/**
 * W-MEMORY-EVOLUTION PR-4 (2026-05-29) — production leader-state getter.
 *
 * The memory Tier proxy (`src/services/memoryTierProxy`) gates Tier LLM-call
 * execution on this: the orchestrator broadcasts each `llmCallRequest` to ALL
 * events subscribers, but only the lease holder must execute (else the same
 * `req_id` double-spends quota + races two write-backs). Distinct from
 * `_testOnly_isLeader()` — that one is a test accessor, this is a stable
 * production export consumed cross-module.
 */
export function isLeaderHeld(): boolean {
  return leaderHeld
}

async function tryClaimLeader(memoryDir: string): Promise<LeaderLease | null> {
  try {
    const result = (await memoryBridgeIpc.send('memory.leader.claim', {
      memory_dir: memoryDir,
      owner_pid: process.pid,
      ttl_ms: LEADER_TTL_MS,
    })) as
      | {
          granted: boolean
          leader_token?: string
          leader_epoch?: number
          lease_expires_at_ms?: number
        }
      | undefined
    if (result?.granted !== true) return null
    if (
      !Number.isSafeInteger(result.lease_expires_at_ms) ||
      (result.lease_expires_at_ms ?? -1) < 0
    ) {
      throw new Error('granted claim omitted a valid lease_expires_at_ms')
    }
    if (
      typeof result.leader_token !== 'string' ||
      result.leader_token.length === 0 ||
      !Number.isSafeInteger(result.leader_epoch) ||
      (result.leader_epoch ?? 0) <= 0
    ) {
      throw new Error('granted claim omitted a valid leader token/epoch')
    }
    return {
      leaderToken: result.leader_token,
      leaderEpoch: result.leader_epoch as number,
      leaseExpiresAtMs: result.lease_expires_at_ms as number,
    }
  } catch (err) {
    logStderr(
      `claim failed (fail-soft): ${err instanceof Error ? err.message : String(err)}`,
    )
    return null
  }
}

function initRunnersOnce(): void {
  if (runnersInitOnce) return
  runnersInitOnce = true
  memoryRunners.initMemoryRunners()
  // W-MEMORY-EVOLUTION PR-4 (2026-05-29): install the Tier reverse-IPC proxy
  // alongside the existing runners. Idempotent + leader-gated; the proxy
  // reads the live leader state via `isLeaderHeld()` per-frame. Fail-soft — a
  // thrown install must not abort runner init.
  try {
    startMemoryTierProxy()
  } catch (err) {
    logStderr(
      `tier proxy install failed (fail-soft): ${err instanceof Error ? err.message : String(err)}`,
    )
  }
  // W-MEMORY-EVOLUTION PR-7b (2026-05-29): install the Tier tool (evidence-
  // gathering) proxy alongside the LLM proxy. Same idempotent + leader-gated
  // + fail-soft contract.
  try {
    startMemoryTierToolProxy()
  } catch (err) {
    logStderr(
      `tier tool proxy install failed (fail-soft): ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

function stopLeaderRenew(): void {
  if (leaderRenewTimer !== null) {
    clearInterval(leaderRenewTimer)
    leaderRenewTimer = null
  }
}

function stopLeaderWatchdog(): void {
  if (leaderWatchdogTimer !== null) {
    clearInterval(leaderWatchdogTimer)
    leaderWatchdogTimer = null
  }
}

function stopLeaderExpiryTimer(): void {
  if (leaderExpiryTimer !== null) {
    clearTimeout(leaderExpiryTimer)
    leaderExpiryTimer = null
  }
}

function stepDownFromLeader(
  memoryDir: string,
  expectedGeneration: number,
  reason: string,
): void {
  if (
    !leaderHeld ||
    leaderGeneration !== expectedGeneration ||
    leaderMemoryDir !== memoryDir
  ) {
    return
  }
  // This call closes admission and aborts recovered work synchronously before
  // any local leader flag changes. The promise settles claimed deliveries in
  // the background; the old token is already fenced by Rust when renew says
  // the lease was lost.
  void memoryRunners.stopDurableMemoryRunnerRecovery('leader_lost')
  logStderr(reason)
  leaderHeld = false
  leaderToken = null
  leaderEpoch = null
  leaderLeaseExpiresAtMs = null
  leaderGeneration += 1
  stopLeaderRenew()
  stopLeaderExpiryTimer()
  startLeaderWatchdog(memoryDir)
}

function armLeaderExpiry(
  memoryDir: string,
  generation: number,
  leaseExpiresAtMs: number,
): void {
  if (
    !leaderHeld ||
    leaderGeneration !== generation ||
    leaderMemoryDir !== memoryDir
  ) {
    return
  }
  leaderLeaseExpiresAtMs = leaseExpiresAtMs
  stopLeaderExpiryTimer()
  const checkDeadline = (): void => {
    leaderExpiryTimer = null
    if (
      !leaderHeld ||
      leaderGeneration !== generation ||
      leaderMemoryDir !== memoryDir ||
      leaderLeaseExpiresAtMs !== leaseExpiresAtMs
    ) {
      return
    }
    const remainingMs = leaseExpiresAtMs - Date.now()
    if (remainingMs > 0) {
      leaderExpiryTimer = setTimeout(
        checkDeadline,
        Math.min(remainingMs, MAX_TIMER_DELAY_MS),
      )
      leaderExpiryTimer.unref?.()
      return
    }
    stepDownFromLeader(
      memoryDir,
      generation,
      'lease deadline elapsed without an authoritative renewal; stepping down',
    )
  }
  leaderExpiryTimer = setTimeout(
    checkDeadline,
    Math.min(Math.max(0, leaseExpiresAtMs - Date.now()), MAX_TIMER_DELAY_MS),
  )
  leaderExpiryTimer.unref?.()
}

function activateLeader(memoryDir: string, lease: LeaderLease): void {
  stopLeaderWatchdog()
  leaderHeld = true
  leaderMemoryDir = memoryDir
  leaderToken = lease.leaderToken
  leaderEpoch = lease.leaderEpoch
  leaderGeneration += 1
  const generation = leaderGeneration
  // Arm the proven lease boundary before admitting any recovered work.
  armLeaderExpiry(memoryDir, generation, lease.leaseExpiresAtMs)
  startLeaderRenew(memoryDir, generation)
  initRunnersOnce()
  void memoryRunners
    .startDurableMemoryRunnerRecovery({
      leaderToken: lease.leaderToken,
      leaderEpoch: lease.leaderEpoch,
    })
    .catch(() => {
      logStderr(
        'leader-acquire durable runner drain paused; the next wake or acquisition will retry',
      )
    })
}

// W-MEMORY-KB-UPLIFT P5 (2026-07-17) — leader-gated periodic remote knowledge
// sync. Rides the renew tick (30s) because the lease holder is exactly the
// process that must own outbound side effects (same "one executor" argument
// as Tier frames — two processes diffing + pushing the same collection race
// each other's tombstones). `maybeRunPeriodicKnowledgeSync` self-gates on the
// user's knowledge-sync.json (enabled switch is user-file-only) + interval
// state, so the common unconfigured case is a single cheap file probe. The KB
// member floor (决策 D8) applies to the capability, not just the tool action —
// checked here from the persisted membership signals (pure sync read).
let kbSyncTickInFlight = false
let kbSyncBlockedLogged = false
function fireLeaderPeriodicKnowledgeSync(): void {
  if (kbSyncTickInFlight) return
  try {
    if (!canUseKbPremium(getMembershipGateInput())) {
      if (!kbSyncBlockedLogged) {
        kbSyncBlockedLogged = true
        logStderr(
          'periodic knowledge sync skipped: KB premium membership gate (will re-check each tick)',
        )
      }
      return
    }
    kbSyncBlockedLogged = false
    kbSyncTickInFlight = true
    void maybeRunPeriodicKnowledgeSync()
      .catch(err => {
        logStderr(
          `periodic knowledge sync error (fail-soft): ${err instanceof Error ? err.message : String(err)}`,
        )
      })
      .finally(() => {
        kbSyncTickInFlight = false
      })
  } catch (err) {
    // Membership read itself must never break lease renewal.
    kbSyncTickInFlight = false
    logStderr(
      `periodic knowledge sync gate error (fail-soft): ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

async function renewLeaderOnce(
  memoryDir: string,
  generation: number,
): Promise<void> {
  if (
    leaderRenewInFlight ||
    !leaderHeld ||
    leaderGeneration !== generation ||
    leaderMemoryDir !== memoryDir ||
    leaderToken === null ||
    leaderEpoch === null
  ) {
    return
  }
  const expectedLeaderToken = leaderToken
  const expectedLeaderEpoch = leaderEpoch
  leaderRenewInFlight = true
  try {
    const result = (await memoryBridgeIpc.send('memory.leader.renew', {
      memory_dir: memoryDir,
      owner_pid: process.pid,
      leader_token: expectedLeaderToken,
      leader_epoch: expectedLeaderEpoch,
      ttl_ms: LEADER_TTL_MS,
    })) as
      | {
          still_leader: boolean
          leader_epoch: number | null
          lease_expires_at_ms: number | null
        }
      | undefined
    if (
      !leaderHeld ||
      leaderGeneration !== generation ||
      leaderMemoryDir !== memoryDir ||
      leaderToken !== expectedLeaderToken ||
      leaderEpoch !== expectedLeaderEpoch
    ) {
      return
    }
    if (result?.still_leader !== true) {
      stepDownFromLeader(
        memoryDir,
        generation,
        'renew reported lost lease; restarting watchdog',
      )
      return
    }
    if (
      !Number.isSafeInteger(result.lease_expires_at_ms) ||
      (result.lease_expires_at_ms ?? -1) < 0
    ) {
      throw new Error('renew omitted a valid lease_expires_at_ms')
    }
    if (result.leader_epoch !== expectedLeaderEpoch) {
      stepDownFromLeader(
        memoryDir,
        generation,
        'renew returned a different leader epoch; stepping down',
      )
      return
    }
    armLeaderExpiry(
      memoryDir,
      generation,
      result.lease_expires_at_ms as number,
    )
    // A follower process can enqueue durable Memory work without being able
    // to deliver an in-process wake hint to this leader. The existing renewal
    // clock is therefore also the cross-process liveness clock for journal
    // enumeration.
    void memoryRunners.wakeDurableMemoryRunnerRecovery()
    fireLeaderPeriodicKnowledgeSync()
  } catch (err) {
    if (
      leaderHeld &&
      leaderGeneration === generation &&
      leaderMemoryDir === memoryDir
    ) {
      const deadline = leaderLeaseExpiresAtMs
      if (deadline === null || Date.now() >= deadline) {
        stepDownFromLeader(
          memoryDir,
          generation,
          'renew failed at or beyond the proven lease deadline; stepping down',
        )
      } else {
        logStderr(
          `renew error; retaining leadership only until ${deadline}: ${err instanceof Error ? err.message : String(err)}`,
        )
      }
    }
  } finally {
    leaderRenewInFlight = false
  }
}

function startLeaderRenew(memoryDir: string, generation: number): void {
  stopLeaderRenew()
  const timer = setInterval(() => {
    void renewLeaderOnce(memoryDir, generation)
  }, LEADER_RENEW_INTERVAL_MS)
  timer.unref?.()
  leaderRenewTimer = timer
}

async function watchdogClaimOnce(memoryDir: string): Promise<void> {
  if (leaderWatchdogInFlight || leaderHeld || leaderMemoryDir !== memoryDir) {
    return
  }
  leaderWatchdogInFlight = true
  try {
    const lease = await tryClaimLeader(memoryDir)
    if (lease && !leaderHeld && leaderMemoryDir === memoryDir) {
      logStderr(`watchdog → claimed leader (prior leader stale/dead)`)
      activateLeader(memoryDir, lease)
    }
  } catch (err) {
    logStderr(
      `watchdog error: ${err instanceof Error ? err.message : String(err)}`,
    )
  } finally {
    leaderWatchdogInFlight = false
  }
}

function startLeaderWatchdog(memoryDir: string): void {
  stopLeaderWatchdog()
  const timer = setInterval(() => {
    void watchdogClaimOnce(memoryDir)
  }, LEADER_RENEW_INTERVAL_MS)
  timer.unref?.()
  leaderWatchdogTimer = timer
}

export async function bootstrapMemoryBridgeAndMaybeRunners(): Promise<{
  bridgeInit: boolean
  leaderClaimed: boolean
}> {
  const bridgeInit = memoryBridgeIpc.initFromEnv()
  if (!bridgeInit) {
    return { bridgeInit: false, leaderClaimed: false }
  }

  // The orchestrator broadcasts Tier frames without project routing, so the
  // lease is global rather than per project.
  const memoryDir = getGlobalExecutorLeaseDir()
  leaderMemoryDir = memoryDir

  const lease = await tryClaimLeader(memoryDir)
  const leaderClaimed = lease !== null
  if (lease) {
    logStderr(`leader claimed (pid ${process.pid}, memory_dir ${memoryDir})`)
    activateLeader(memoryDir, lease)
  } else {
    logStderr(
      `leader busy — running as follower; watchdog @ ${LEADER_RENEW_INTERVAL_MS}ms`,
    )
    startLeaderWatchdog(memoryDir)
  }
  return { bridgeInit, leaderClaimed }
}

/**
 * Best-effort release of the leader lease + cleanup of renew/watchdog timers.
 * Called from the worker's `shutdown(reason)` path on stdin-close / SIGTERM /
 * SIGINT (and from the TUI cleanup chain) so a graceful exit lets the next
 * process claim immediately (no need to wait the full TTL).
 *
 * Safe to call even when this process never held the lease (no-op).
 */
export async function releaseLeaderIfHeld(): Promise<void> {
  stopLeaderRenew()
  stopLeaderWatchdog()
  stopLeaderExpiryTimer()
  // W-MEMORY-EVOLUTION PR-4 (2026-05-29): tear down the Tier proxy
  // subscription on graceful release (idempotent — no-op when not installed).
  try {
    stopMemoryTierProxy()
  } catch {
    // Fail-soft — teardown errors must not block lease release.
  }
  // W-MEMORY-EVOLUTION PR-7b (2026-05-29): tear down the Tier tool proxy too.
  try {
    stopMemoryTierToolProxy()
  } catch {
    // Fail-soft — teardown errors must not block lease release.
  }
  const memoryDir = leaderMemoryDir
  const wasLeader = leaderHeld
  const token = leaderToken
  const epoch = leaderEpoch
  // Claimed runner deliveries must settle or release while this exact leader
  // fence is still valid. Releasing the leader first would strand in-flight
  // work until its delivery TTL.
  await memoryRunners.stopDurableMemoryRunnerRecovery('leader_release')
  leaderHeld = false
  leaderToken = null
  leaderEpoch = null
  leaderLeaseExpiresAtMs = null
  leaderGeneration += 1
  leaderMemoryDir = null
  if (
    !wasLeader ||
    memoryDir === null ||
    token === null ||
    epoch === null
  ) {
    return
  }
  try {
    await memoryBridgeIpc.send('memory.leader.release', {
      memory_dir: memoryDir,
      owner_pid: process.pid,
      leader_token: token,
      leader_epoch: epoch,
    })
  } catch (err) {
    logStderr(
      `release failed (fail-soft, TTL takeover still works): ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

/**
 * Legacy narrow API — preserved so any caller that still expects the v6
 * one-shot "just init the bridge" form keeps compiling. Equivalent to
 * `bootstrapMemoryBridgeAndMaybeRunners()` but discards the leader-claim
 * outcome and runs synchronously (no awaitable promise on the bootstrap
 * path). Test fixtures may prefer this when they don't want the runner
 * init side-effects.
 */
export function bootstrapMemoryBridgeFromEnv(): boolean {
  return memoryBridgeIpc.initFromEnv()
}

// Test-only accessors. Not exported from any barrel.
export function _testOnly_isLeader(): boolean {
  return leaderHeld
}
export function _testOnly_hasRenewTimer(): boolean {
  return leaderRenewTimer !== null
}
export function _testOnly_hasWatchdogTimer(): boolean {
  return leaderWatchdogTimer !== null
}
export function _testOnly_leaseExpiresAtMs(): number | null {
  return leaderLeaseExpiresAtMs
}
export function _testOnly_forceRenew(): Promise<void> {
  const memoryDir = leaderMemoryDir
  if (memoryDir === null) return Promise.resolve()
  return renewLeaderOnce(memoryDir, leaderGeneration)
}
export function _testOnly_forceWatchdog(): Promise<void> {
  const memoryDir = leaderMemoryDir
  if (memoryDir === null) return Promise.resolve()
  return watchdogClaimOnce(memoryDir)
}
export function _testOnly_reset(): void {
  stopLeaderRenew()
  stopLeaderWatchdog()
  stopLeaderExpiryTimer()
  // Admission closes synchronously inside this call; asynchronous delivery
  // release is intentionally not awaited by the synchronous test reset API.
  void memoryRunners.stopDurableMemoryRunnerRecovery('test_reset')
  // W-MEMORY-EVOLUTION PR-4 (2026-05-29): tear down the proxy too so the
  // module-level singleton does not leak across leader-election tests.
  try {
    stopMemoryTierProxy()
  } catch {
    // ignore
  }
  try {
    stopMemoryTierToolProxy()
  } catch {
    // ignore
  }
  leaderHeld = false
  leaderMemoryDir = null
  leaderToken = null
  leaderEpoch = null
  leaderLeaseExpiresAtMs = null
  leaderGeneration += 1
  leaderRenewInFlight = false
  leaderWatchdogInFlight = false
  runnersInitOnce = false
}
