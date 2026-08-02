import { spawn } from 'node:child_process'

export type ProcessLifecycleCommand = Readonly<{
  executable: string
  args: readonly string[]
  label: string
}>

/**
 * Execute one bounded, stdio-free lifecycle command.
 *
 * Resolution and product policy stay with the caller; this helper only owns
 * process completion, timeout, and the never-throw result contract.
 */
export async function runProcessLifecycleCommand(
  command: ProcessLifecycleCommand,
  timeoutMs: number,
): Promise<boolean> {
  return new Promise<boolean>(resolve => {
    let settled = false
    const done = (ok: boolean) => {
      if (settled) return
      settled = true
      resolve(ok)
    }
    let child: ReturnType<typeof spawn>
    try {
      child = spawn(command.executable, [...command.args], {
        stdio: 'ignore',
      })
    } catch {
      done(false)
      return
    }
    const timer = setTimeout(() => {
      try {
        child.kill('SIGKILL')
      } catch {
        // best-effort
      }
      done(false)
    }, timeoutMs)
    child.once('error', () => {
      clearTimeout(timer)
      done(false)
    })
    child.once('exit', code => {
      clearTimeout(timer)
      done(code === 0)
    })
  })
}
