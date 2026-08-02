import { lstatSync } from 'node:fs'
import { dirname, join } from 'node:path'
import {
  installCronEnsureCommandProvider,
  type CronEnsureCommandProvider,
} from './cronEnsure.js'

/**
 * Resolve the dedicated package's process-owned lifecycle route.
 *
 * The sibling launcher is the already-packaged `crabcode-tui` executable.
 * That launcher delegates to the shared Rust daemon-launcher contract; the TS
 * runtime never starts `crabcode-cron` directly.
 */
export const resolveDirectTuiCronEnsureCommand: CronEnsureCommandProvider = (
  execPath = process.execPath,
) => {
  const exeName =
    process.platform === 'win32' ? 'crabcode-tui.exe' : 'crabcode-tui'
  const candidate = join(dirname(execPath), exeName)
  try {
    if (!lstatSync(candidate).isFile()) return null
  } catch {
    return null
  }
  return {
    executable: candidate,
    args: ['--ensure-cron-daemon'],
    label: 'native TUI cron ensure',
  }
}

export function installDirectTuiCronEnsureProvider(): void {
  installCronEnsureCommandProvider(resolveDirectTuiCronEnsureCommand)
}
