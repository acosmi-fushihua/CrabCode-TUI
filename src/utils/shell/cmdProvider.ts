// W-BRIDGE-FU-2 (2026-04-25 攻坚 2)：Windows cmd.exe ShellProvider 实现。
//
// 与 bashProvider / powershellProvider 的关键差异：
//   - 命令行语法：`cmd.exe /c <command>` 直接执行；不支持 -c / -Command 风格
//   - 不支持 snapshot / source / 函数预加载（cmd 没有 .bashrc 等价物）
//   - cwd 跟踪：cmd 内置 `cd` 输出当前目录到 stdout 的方式不同；用 `echo %CD%`
//   - quoting：cmd 用 `"..."` 包裹 token；内部双引号需 `""` 转义；含 `& | < > ^`
//     特殊字符的 token 不可用 backslash 转义
//
// 当前作用：让 SpawnManagedRequest.shell='cmd' 在 TS 本地 fallback 路径有真实
// 实现，不再静默回退为 'powershell'。

import type { ShellProvider, ShellType } from './shellProvider.js'

/**
 * cmd.exe 命令转义 — 把单个 token 包装成 cmd 安全形式：
 *   - 含 `& | < > ^ "` 任一字符 → 用双引号包，内部 " 改 ""
 *   - 否则原样输出
 */
function cmdQuote(token: string): string {
  if (!/[&|<>^" \t]/.test(token)) return token
  return '"' + token.replace(/"/g, '""') + '"'
}

export function createCmdProvider(cmdPath: string): ShellProvider {
  return {
    type: 'cmd' as ShellType,
    shellPath: cmdPath,
    detached: false, // cmd.exe 不需要 detached（无作业控制）

    async buildExecCommand(
      command: string,
      opts: {
        id: number | string
        sandboxTmpDir?: string
        useSandbox: boolean
      },
    ): Promise<{ commandString: string; cwdFilePath: string }> {
      // cwd file：cmd `& echo %CD%>file` 写当前目录后追加，便于父进程跟踪
      const cwdFilePath = opts.sandboxTmpDir
        ? `${opts.sandboxTmpDir}\\cwd-${opts.id}.txt`
        : `${process.env.TEMP || process.env.TMP || 'C:\\Windows\\Temp'}\\crabcode-cwd-${opts.id}.txt`

      // 用户命令 + 追加 cwd 写入；任何子命令异常都不阻断 cwd 写入
      const wrapped = `${command} & echo %CD%> ${cmdQuote(cwdFilePath)}`
      return { commandString: wrapped, cwdFilePath }
    },

    getSpawnArgs(commandString: string): string[] {
      // /c → 执行后退出；/v:on → 启用延迟变量扩展 (!VAR! 形式)
      // 不加 /q，保留命令回显便于 stderr 调试
      return ['/v:on', '/c', commandString]
    },

    async getEnvironmentOverrides(_command: string): Promise<Record<string, string>> {
      // cmd 不需要特殊环境变量（PATH/PROMPT 等由 Windows 默认提供）
      return {}
    },
  }
}
