// W-BRIDGE-FU-2 (2026-04-25 攻坚 2)：扩到 6 值，与
// Keep this union aligned with runtime/localProcess SpawnManagedRequest.shell.
//   - bash / zsh / sh : POSIX 家族（zsh/sh 复用 bash provider，二进制路径不同；
//     sh 跳过 snapshot 因 sh 太精简）
//   - powershell / pwsh : PowerShell 家族（pwsh 是 PowerShell 7+ 的跨平台 binary）
//   - cmd : Windows cmd.exe（独立 cmdProvider，与 POSIX/PowerShell 都不同）
// `findShellBinary` 在 PATH 中查找；找不到时按 JSDoc 承诺 throw（不静默降级）。
export const SHELL_TYPES = ['bash', 'zsh', 'sh', 'powershell', 'pwsh', 'cmd'] as const
export type ShellType = (typeof SHELL_TYPES)[number]
export const DEFAULT_HOOK_SHELL: ShellType = 'bash'

export type ShellProvider = {
  type: ShellType
  shellPath: string
  detached: boolean

  /**
   * Build the full command string including all shell-specific setup.
   * For bash: source snapshot, session env, disable extglob, eval-wrap, pwd tracking.
   */
  buildExecCommand(
    command: string,
    opts: {
      id: number | string
      sandboxTmpDir?: string
      useSandbox: boolean
    },
  ): Promise<{ commandString: string; cwdFilePath: string }>

  /**
   * Shell args for spawn (e.g., ['-c', '-l', cmd] for bash).
   */
  getSpawnArgs(commandString: string): string[]

  /**
   * Extra env vars for this shell type.
   * May perform async initialization (e.g., tmux socket setup for bash).
   */
  getEnvironmentOverrides(command: string): Promise<Record<string, string>>
}
