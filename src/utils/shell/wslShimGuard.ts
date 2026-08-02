/**
 * W-WIN-SHELL-WSL-HIJACK — Windows "bash" 身份消歧关隘（拒 WSL 垫片 + 优先 Git bash）。
 *
 * Windows 上裸 `bash` 常解析到 `C:\Windows\System32\bash.exe` —— 这是
 * Microsoft-Windows-Subsystem-Linux 的 wsl.exe 启动垫片。本机无发行版时
 * `bash -c -l …` → `execvpe(/bin/bash) failed` 炸（黑框闪 + 命令失败）；即便有
 * 发行版，命令也落 Linux FS(`/mnt/c`)、破坏本产品全套 POSIX 路径/cwd 不变量。
 * 故 WSL 垫片**从不是**受支持的 bash 目标，所有 shell 二进制解析路径一律拒绝，
 * 优先真 Git bash（MSYS2，跑在 Windows FS、与本产品 POSIX 适配自洽）。
 *
 * 对齐 Rust `acosmi-supervisor::resolve_shell` 既有护栏（Windows `shell:"bash"`
 * → `cmd.exe /C`，测试明写"避开 System32 WSL stub"）——本模块把该认知收敛为 TS
 * 侧单一真源，供 `findSuitableShell`（auto 发现）与 `resolveDefaultShell`（默认
 * 决策）共用，避免散落各入口各自重推。
 *
 * 单独成模块：让单测直接 import 纯判据，无需拉起 Shell.ts 的重依赖图
 * （analytics / bootstrap state / sandbox / bridge adapters）。
 */
import { accessSync, constants as fsConstants } from 'fs'
import { join as nativeJoin } from 'path'
import { getPlatform, type Platform } from '../platform.js'

/**
 * 判定 bash 候选路径是否为 Windows 的 WSL 启动垫片（而非真 Git/MSYS2 bash）。
 *
 * 判据 = 路径以 `\System32\bash.exe` 或 `\Sysnative\bash.exe` 结尾（大小写不敏感、
 * 正反斜杠均归一）。这是 `%SystemRoot%\System32\bash.exe`（WSL 转发垫片）；真
 * bash 装在 Git 目录（`…\Git\bin\bash.exe` / `…\Git\usr\bin\bash.exe`），不在此。
 *
 * 纯字符串判据（不触盘）：POSIX 路径用 `/` 分隔、绝不以 `\system32\bash.exe`
 * 结尾，故在非 Windows 上自然恒 false，无需平台门即可安全用于任何平台的字符串。
 */
export function isWslBashShim(shellPath: string): boolean {
  if (!shellPath) return false
  const normalized = shellPath.replace(/\//g, '\\').toLowerCase()
  return (
    normalized.endsWith('\\system32\\bash.exe') ||
    normalized.endsWith('\\sysnative\\bash.exe')
  )
}

/**
 * Windows 上真 Git bash（MSYS2）的常见安装位置，按优先级返回绝对路径候选。
 * `…\Git\bin\bash.exe`（登录包装器）优先于 `…\Git\usr\bin\bash.exe`（MSYS2 本体）。
 * 非 Windows 返回空数组。不触盘（仅拼路径），可执行性由调用方校验。
 */
export function getWindowsGitBashCandidates(
  platform: Platform = getPlatform(),
): string[] {
  if (platform !== 'windows') return []
  const bases: string[] = []
  const programFiles = process.env.ProgramW6432 || process.env.ProgramFiles
  const programFilesX86 = process.env['ProgramFiles(x86)']
  const localAppData = process.env.LOCALAPPDATA
  if (programFiles) bases.push(programFiles)
  if (programFilesX86) bases.push(programFilesX86)
  // 常量兜底（env 缺失时仍能命中标准安装位置）
  bases.push('C:\\Program Files', 'C:\\Program Files (x86)')
  if (localAppData) bases.push(nativeJoin(localAppData, 'Programs'))

  const candidates: string[] = []
  const seen = new Set<string>()
  for (const base of bases) {
    for (const rel of [
      ['Git', 'bin', 'bash.exe'],
      ['Git', 'usr', 'bin', 'bash.exe'],
    ]) {
      const p = nativeJoin(base, ...rel)
      const key = p.toLowerCase()
      if (!seen.has(key)) {
        seen.add(key)
        candidates.push(p)
      }
    }
  }
  return candidates
}

/** accessSync(X_OK) 的静默布尔封装（供本模块内同步探测用）。 */
function isExecutableFileSync(p: string): boolean {
  try {
    accessSync(p, fsConstants.X_OK)
    return true
  } catch {
    return false
  }
}

/**
 * 同步判定 Windows 上是否存在**真** bash（非 WSL 垫片），供 `resolveDefaultShell`
 * 的默认决策使用（"有真 bash 才默认 bash，否则 powershell"）。
 * 非 Windows 恒 true（POSIX 必有 shell，默认决策与本模块无关）。
 *
 * 探测顺序：① Git bash 候选位置；② PATH 里非垫片的 bash.exe。任一命中即 true。
 *
 * 不 memoize：唯一（间接）消费者是 `!` 输入框路径（processBashCommand），人触发、
 * 低频，且仅在 `settings.defaultShell` 未显式配置 + Windows 时才走到——非热路径，
 * memoize 属过早优化（且会妨碍测试跨 env 复算）。
 */
export function hasRealWindowsBash(platform: Platform = getPlatform()): boolean {
  if (platform !== 'windows') return true
  for (const candidate of getWindowsGitBashCandidates(platform)) {
    if (isExecutableFileSync(candidate)) return true
  }
  // PATH 扫描：跳过 System32/Sysnative WSL 垫片，找任一可执行 bash.exe。
  const pathEnv = process.env.PATH || ''
  for (const dir of pathEnv.split(';')) {
    if (!dir) continue
    const candidate = nativeJoin(dir, 'bash.exe')
    if (isWslBashShim(candidate)) continue
    if (isExecutableFileSync(candidate)) return true
  }
  return false
}

/**
 * 纯默认-shell 决策（无 IO，供 resolveDefaultShell 委托 + 单测直接覆盖全矩阵）。
 *
 * - 显式 `settings.defaultShell` 恒尊重（bash/powershell 原样返回）；
 * - 否则 Windows 且无真 bash → 'powershell'（避免默认落 WSL 垫片炸）；
 * - 其余 → 'bash'。
 *
 * `hasRealBash` 由调用方按需惰性求值（见 resolveDefaultShell）：显式或非 Windows
 * 分支不消费它，故那些情况传任意值均安全。
 */
export function decideDefaultShell(
  explicit: 'bash' | 'powershell' | undefined,
  platform: Platform,
  hasRealBash: boolean,
): 'bash' | 'powershell' {
  if (explicit) return explicit
  if (platform === 'windows' && !hasRealBash) return 'powershell'
  return 'bash'
}
