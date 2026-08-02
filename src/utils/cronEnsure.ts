// Transport-neutral cron daemon cold-start ensure.
//
// The daemon lifecycle remains owned by the existing Rust daemon-launcher
// double-fork + lock + PID contract. Each process entry installs its exact
// lifecycle command; this module never discovers another server/shared-CLI
// transport and never starts `crabcode-cron` directly.

import { DURABLE_CRON_LIVE_CONSUMER_READY } from '../tools/ScheduleCronTool/constants.js'
import { logForDebugging } from './debug.js'
import { isEnvTruthy } from './envUtils.js'
import {
  runProcessLifecycleCommand,
  type ProcessLifecycleCommand,
} from './processLifecycleCommand.js'

/** ~8s — covers the launcher's 5s PID-file wait + the status RPC + margin. */
const ENSURE_TIMEOUT_MS = 8000

export type CronEnsureCommand = ProcessLifecycleCommand
export type CronEnsureCommandProvider = (
  execPath?: string,
) => CronEnsureCommand | null

let commandProvider: CronEnsureCommandProvider | null = null

export function installCronEnsureCommandProvider(
  provider: CronEnsureCommandProvider,
): void {
  commandProvider = provider
}

export function resetCronEnsureCommandProviderForTests(): void {
  commandProvider = null
}

/**
 * Ensure the crabcode-cron daemon is running through the entry-owned Rust
 * lifecycle command.
 *
 * Returns `true` if the command exited 0 (daemon reachable/ensured), `false`
 * otherwise. Never throws. No-op returning `false` when `CRABCODE_DISABLE_CRON`
 * is set or when the active process entry did not install a command.
 */
export async function ensureCronDaemon(): Promise<boolean> {
  // Release authority is compile-time. Do not even resolve/spawn a CLI while
  // PR-5's exact-target live-consumer cutover is incomplete.
  if (!DURABLE_CRON_LIVE_CONSUMER_READY) {
    return false
  }
  if (isEnvTruthy(process.env.CRABCODE_DISABLE_CRON)) {
    return false
  }
  const command = commandProvider?.()
  if (!command) {
    logForDebugging(
      '[cronEnsure] lifecycle command unavailable; skipping daemon ensure',
    )
    return false
  }
  const ok = await runProcessLifecycleCommand(command, ENSURE_TIMEOUT_MS)
  if (!ok) {
    logForDebugging(`[cronEnsure] ${command.label} failed`)
  }
  return ok
}
