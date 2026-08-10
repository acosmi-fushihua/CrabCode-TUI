/**
 * 严格沙箱档的确定性拒绝（W-SANDBOX-ENFORCED-DEADCODE PR-0）
 *
 * 严格档 = `sandbox.enabled===true && sandbox.allowUnsandboxedCommands===false`：
 * 用户/策略明确要求「命令必须在沙箱里跑」。此时若执行后端根本没接线
 * （`enforcedBackendProbe`），只剩两条路：
 *   - 静默裸跑 —— 就是本立项要消灭的假沙箱；
 *   - **确定性拒绝** —— 本模块。
 *
 * 拒绝走 `validateInput` 输入校验通道（不是 runtime throw），因此
 * 同输入必然同拒绝；这是配置事实而非工具解析链的终态错误，模型看到的是
 * 「改配置」而不是「换参数重试」。
 *
 * 文案是契约：`terminalToolError.ts` 那条铁律的同款要求 —— producer 抽导出
 * builder，测试 `import` 真源断言，**禁止手抄近似文案**。
 */

import { getPlatform } from '../platform.js'
import { SandboxManager } from './sandbox-adapter.js'

/**
 * 拒绝文案的稳定前缀。文案主体随 reason 变化，前缀不变，供 TUI / 日志做前缀
 * 归类（与 `WEB_FETCH_SSRF_BLOCKED_PREFIX` 同款）。
 */
export const STRICT_SANDBOX_REFUSAL_PREFIX =
  'Refused: strict sandbox mode is on, but the sandbox execution backend is unavailable'

/**
 * 构造严格档拒绝文案。三要素缺一不可（`strictSandboxRefusalMentionsAllThree`
 * 有断言钉着）：
 *   1. 真因 —— 后端不可用 + 具体 reason
 *   2. 归因 —— `sandbox.allowUnsandboxedCommands=false` 要求命令必须在沙箱内执行
 *   3. 出路 —— TUI `/sandbox`
 */
export function buildStrictSandboxRefusal(reason: string): string {
  return (
    `${STRICT_SANDBOX_REFUSAL_PREFIX} (${reason}). ` +
    `This project sets sandbox.allowUnsandboxedCommands=false (strict sandbox mode), ` +
    `which requires every command to execute inside the sandbox — so running it unsandboxed is not permitted, ` +
    `and dangerouslyDisableSandbox will not lift this. ` +
    `To proceed, run /sandbox in the TUI and switch to full access, ` +
    `which turns the sandbox off.`
  )
}

/**
 * 当前是否处于「严格档 + 后端不可用」。
 *
 * 刻意读 `isSandboxEnabledInSettings()`（用户的原始意图）而不是
 * `isSandboxingEnabled()` —— 后者已被第 4 道门按后端可用性拉成 false，
 * 用它判会永远返回 false，把这道闸门变成死代码。
 */
export function isStrictSandboxBackendViolation(): boolean {
  if (!SandboxManager.isSandboxEnabledInSettings()) return false
  if (SandboxManager.areUnsandboxedCommandsAllowed()) return false
  return !SandboxManager.isEnforcedBackendWired()
}

/**
 * 严格档违例时给出完整拒绝文案；否则 null。
 * 两个 shell 工具的 `validateInput` 各调一次。
 */
export function strictSandboxRefusalOrNull(): string | null {
  if (!isStrictSandboxBackendViolation()) return null
  return buildStrictSandboxRefusal(
    SandboxManager.getEnforcedBackendUnavailableReason() ?? 'unspecified',
  )
}

/**
 * Sandboxed PowerShell is POSIX-only: the sandboxed form spawns `/bin/sh` as
 * the outer shell (see `Shell.ts::isSandboxedPowerShell`), which Windows does
 * not have. So on Windows this tool always runs unsandboxed — and if enterprise
 * policy has sandbox.enabled AND forbids unsandboxed commands, it cannot
 * comply: refuse execution rather than silently bypass the policy.
 *
 * Note the reason is about **this tool**, not about the platform. Since
 * W-SANDBOX-ENFORCED-DEADCODE PR-3, Windows does have a working sandbox backend
 * — BashTool uses it. Restoring the old wording ("sandbox is unavailable on
 * native Windows") would re-assert something that is no longer true.
 *
 * Checked in BOTH validateInput (clean tool-runner error) and call()
 * (covers direct callers like promptShellExecution.ts that skip
 * validateInput). The call() guard is the load-bearing one.
 *
 * 住在本文件而不是 `PowerShellTool.tsx`，是因为 `!cmd` 那条路
 * （`processBashCommand.tsx`）一个工具都不经过，却必须用**同一份**文案与判据：
 * 抄一份过去就等于给「Windows + pwsh + 严格档」这条不变式开第二个真源。本文件
 * 是 leaf 模块（只依赖 platform + sandbox-adapter），两个消费者都已 import 它。
 */
export const WINDOWS_SANDBOX_POLICY_REFUSAL =
  'Enterprise policy requires sandboxing, but PowerShell cannot currently be routed through the native Windows sandbox backend. PowerShell command execution is blocked by policy; use the sandbox-capable Bash tool or change the policy explicitly.'

/**
 * Windows + 「策略要求沙箱且禁止裸跑」⇒ pwsh 这条路无法合规。
 *
 * 与 `isStrictSandboxBackendViolation()` 的分工：那条问「后端接没接线」，这条问
 * 「这个 shell 在这个平台上有没有沙箱形态可用」——后端接线之后本条仍然成立，
 * 因为 pwsh 的沙箱形态需要一个 POSIX 外层 shell。
 */
export function isWindowsSandboxPolicyViolation(): boolean {
  return (
    getPlatform() === 'windows' &&
    SandboxManager.isSandboxEnabledInSettings() &&
    !SandboxManager.areUnsandboxedCommandsAllowed()
  )
}
