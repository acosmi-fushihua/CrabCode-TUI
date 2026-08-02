import { getInitialSettings } from '../settings/settings.js'
import { getPlatform } from '../platform.js'
import { decideDefaultShell, hasRealWindowsBash } from './wslShimGuard.js'

/**
 * Resolve the default shell for input-box `!` commands.
 *
 * Resolution order:
 *   explicit settings.defaultShell（恒尊重）
 *   → Windows 无真 bash: 'powershell'
 *   → 'bash'
 *
 * W-WIN-SHELL-WSL-HIJACK：原先"Windows 也默认 bash、不 auto-flip"会让**无真 bash**
 * 的机器把默认 bash 解析到 System32 WSL 垫片（无发行版即炸）。改为"有真 Git bash
 * 才默认 bash，否则 powershell"——Git bash 用户零回归（仍默认 bash），只有无真
 * bash 的 Windows 机被切到 powershell。与 Rust detection.rs::resolve_default_shell
 * 与 Rust detection.rs::resolve_default_shell 对齐。
 *
 * 注：唯一消费者 processBashCommand.tsx 把本结果与 isPowerShellToolEnabled() 相与，
 * 故非 PowerShellTool 用户（默认）该 flip 不改行为；仅 PS 工具启用者在无 bash 机上
 * 受益（`!` 走 PS 而非坏 bash）。真正保护 agent BashTool 路径的是 findSuitableShell
 * 的拒垫片 + fail-loud（见 wslShimGuard.ts）。
 */
export function resolveDefaultShell(): 'bash' | 'powershell' {
  const explicit = getInitialSettings().defaultShell
  const platform = getPlatform()
  // 仅在 Windows + 无显式配置 时才做 fs 探测（hasRealWindowsBash 触盘）；
  // 其余分支 decideDefaultShell 不消费 hasRealBash，传 true 占位即可。
  const needsProbe = !explicit && platform === 'windows'
  return decideDefaultShell(
    explicit,
    platform,
    needsProbe ? hasRealWindowsBash() : true,
  )
}
