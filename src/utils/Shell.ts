import { execFileSync, spawn } from 'child_process'
import { constants as fsConstants, readFileSync, unlinkSync } from 'fs'
import { type FileHandle, open, realpath } from 'fs/promises'
import memoize from 'lodash-es/memoize.js'
import { isAbsolute, resolve } from 'path'
import { join as nativeJoin } from 'path'
import { join as posixJoin } from 'path/posix'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'
import {
  getOriginalCwd,
  getSessionId,
  setCwdState,
} from '../bootstrap/state.js'
import { generateTaskId } from '../Task.js'
import { pwd } from './cwd.js'
import { logForDebugging } from './debug.js'
import { errorMessage, isENOENT } from './errors.js'
import { getFsImplementation } from './fsOperations.js'
import { logError } from './log.js'
import {
  createAbortedCommand,
  createFailedCommand,
  type ShellCommand,
  wrapSpawn,
} from './ShellCommand.js'
import { NO_SUITABLE_SHELL_MESSAGE } from './shellMessages.js'
import {
  bindTaskOutputFileIdentity,
  ensureTaskOutputDir,
} from './task/diskOutput.js'
import { TaskOutput } from './task/TaskOutput.js'
import { which } from './which.js'

export type { ExecResult } from './ShellCommand.js'

import { accessSync } from 'fs'
import { onCwdChangedForHooks } from './hooks/fileChangedWatcher.js'
import {
  getCrabCodeTempDir,
  getCrabCodeTempDirName,
} from './permissions/filesystem.js'
import { getPlatform } from './platform.js'
import {
  markEnforcedBackendDegraded,
  resolveSandboxHelperBin,
} from './sandbox/enforcedBackendProbe.js'
import { SandboxManager } from './sandbox/sandbox-adapter.js'
import {
  buildSandboxExecArgv,
  buildSandboxExecConfig,
  cleanupSandboxCommandTempDir,
  generateSandboxCommandId,
} from './sandbox/sandboxExecConfig.js'
import { sandboxProxyChildEnv } from './sandbox/sandboxNetworkProxy.js'
import { invalidateSessionEnvCache } from './sessionEnvironment.js'
import { createBashShellProvider } from './shell/bashProvider.js'
import {
  getWindowsGitBashCandidates,
  isWslBashShim,
} from './shell/wslShimGuard.js'
import { createCmdProvider } from './shell/cmdProvider.js'
import { getCachedPowerShellPath } from './shell/powershellDetection.js'
import { createPowerShellProvider } from './shell/powershellProvider.js'
import type { ShellProvider, ShellType } from './shell/shellProvider.js'
import {
  appendVendoredEngineDirsToEnvPath,
  existingVendoredEngineDirs,
} from './officeParse/vendoredEnginePath.js'
import { subprocessEnv } from './subprocessEnv.js'
import { posixPathToWindowsPath } from './windowsPaths.js'

const DEFAULT_TIMEOUT = 30 * 60 * 1000 // 30 minutes

export type ShellConfig = {
  provider: ShellProvider
}

function isExecutable(shellPath: string): boolean {
  try {
    accessSync(shellPath, fsConstants.X_OK)
    return true
  } catch (_err) {
    // Fallback for Nix and other environments where X_OK check might fail
    try {
      // Try to execute the shell with --version, which should exit quickly
      // Use execFileSync to avoid shell injection vulnerabilities
      execFileSync(shellPath, ['--version'], {
        timeout: 1000,
        stdio: 'ignore',
      })
      return true
    } catch {
      return false
    }
  }
}

/**
 * Determines the best available shell to use.
 */
export async function findSuitableShell(): Promise<string> {
  // Check for explicit shell override first
  const shellOverride = process.env.CRABCODE_SHELL
  if (shellOverride) {
    // Validate it's a supported shell type
    const isSupported =
      shellOverride.includes('bash') || shellOverride.includes('zsh')
    // W-WIN-SHELL-WSL-HIJACK: 显式 override 也拒 Windows System32 WSL 垫片
    // （无发行版即 execvpe 炸、且破坏 cwd/路径不变量）——落到下方检测优先真 Git bash。
    if (isWslBashShim(shellOverride)) {
      logForDebugging(
        `CRABCODE_SHELL="${shellOverride}" is a WSL launcher stub (System32\\bash.exe); ignoring and falling back to detection`,
      )
    } else if (isSupported && isExecutable(shellOverride)) {
      logForDebugging(`Using shell override: ${shellOverride}`)
      return shellOverride
    } else {
      // Note, if we ever want to add support for new shells here we'll need to update or Bash tool parsing to account for this
      logForDebugging(
        `CRABCODE_SHELL="${shellOverride}" is not a valid bash/zsh path, falling back to detection`,
      )
    }
  }

  // Check user's preferred shell from environment
  const env_shell = process.env.SHELL
  // Only consider SHELL if it's bash or zsh
  const isEnvShellSupported =
    env_shell && (env_shell.includes('bash') || env_shell.includes('zsh'))
  const preferBash = env_shell?.includes('bash')

  // Try to locate shells using which (uses Bun.which when available)
  const [zshPath, bashPathRaw] = await Promise.all([which('zsh'), which('bash')])
  // W-WIN-SHELL-WSL-HIJACK: which('bash') 在 Windows 常命中 System32 WSL 垫片；
  // 剔除它，改由下方 Git bash 候选提供真 bash。
  const bashPath = bashPathRaw && !isWslBashShim(bashPathRaw) ? bashPathRaw : null

  // Populate shell paths from which results and fallback locations
  const shellPaths = ['/bin', '/usr/bin', '/usr/local/bin', '/opt/homebrew/bin']

  // Order shells based on user preference
  const shellOrder = preferBash ? ['bash', 'zsh'] : ['zsh', 'bash']
  const supportedShells = shellOrder.flatMap(shell =>
    shellPaths.map(path => `${path}/${shell}`),
  )

  // W-WIN-SHELL-WSL-HIJACK: Windows 上显式加入真 Git bash（MSYS2）候选。POSIX
  // fallback 路径（/bin/bash 等）在 Windows 不存在，which('bash') 命中的 System32
  // 垫片已剔除——不加这些候选，有 Git bash 的机器也会误抛 no_suitable_shell。
  // 非 Windows 返回 []（无副作用）。
  supportedShells.unshift(...getWindowsGitBashCandidates())

  // Add discovered paths to the beginning of our search list
  // Put the user's preferred shell type first
  if (preferBash) {
    if (bashPath) supportedShells.unshift(bashPath)
    if (zshPath) supportedShells.push(zshPath)
  } else {
    if (zshPath) supportedShells.unshift(zshPath)
    if (bashPath) supportedShells.push(bashPath)
  }

  // Always prioritize SHELL env variable if it's a supported shell type
  // W-WIN-SHELL-WSL-HIJACK: 但 SHELL 指向 WSL 垫片时不采信（同 override 理由）。
  if (
    isEnvShellSupported &&
    !isWslBashShim(env_shell) &&
    isExecutable(env_shell)
  ) {
    supportedShells.unshift(env_shell)
  }

  // W-WIN-SHELL-WSL-HIJACK: 末位兜底——任何漏网的 WSL 垫片候选都剔除。
  const shellPath = supportedShells.find(
    shell => shell && !isWslBashShim(shell) && isExecutable(shell),
  )

  // If no valid shell found, throw a helpful error
  if (!shellPath) {
    // 文案即契约:terminalToolError.ts 签名 no_suitable_shell 按 'No suitable shell
    // found' 字符串判据;文案真源在 shellMessages.ts,改动必须同步判据与分类测试。
    logError(new Error(NO_SUITABLE_SHELL_MESSAGE))
    throw new Error(NO_SUITABLE_SHELL_MESSAGE)
  }

  return shellPath
}

async function getShellConfigImpl(): Promise<ShellConfig> {
  const binShell = await findSuitableShell()
  const provider = await createBashShellProvider(binShell)
  return { provider }
}

// Memoize the entire shell config so it only happens once per session
export const getShellConfig = memoize(getShellConfigImpl)

export const getPsProvider = memoize(async (): Promise<ShellProvider> => {
  const psPath = await getCachedPowerShellPath()
  if (!psPath) {
    throw new Error('PowerShell is not available')
  }
  return createPowerShellProvider(psPath)
})

// W-BRIDGE-FU-2 (2026-04-25 攻坚 2)：6-shell PATH 探测 + provider 解析。
//
// `findExecutableInPath(name)` 在 PATH 中线性查找可执行二进制；返回首个找到的
// 绝对路径或 null。Windows 上自动补 `.exe` 扩展。
function findExecutableInPath(name: string): string | null {
  const pathEnv = process.env.PATH || ''
  const sep = getPlatform() === 'windows' ? ';' : ':'
  const exts = getPlatform() === 'windows' ? ['.exe', '.cmd', ''] : ['']
  for (const dir of pathEnv.split(sep)) {
    if (!dir) continue
    for (const ext of exts) {
      const candidate = nativeJoin(dir, name + ext)
      if (isExecutable(candidate)) return candidate
    }
  }
  return null
}

// 单独 memoize 各 shell 的 provider，找不到时按 JSDoc 承诺 throw（不静默降级）。
const getZshProvider = memoize(async (): Promise<ShellProvider> => {
  const zshPath = findExecutableInPath('zsh')
  if (!zshPath) {
    throw new Error('zsh shell requested but `zsh` binary not found in PATH')
  }
  return createBashShellProvider(zshPath)
})

const getShProvider = memoize(async (): Promise<ShellProvider> => {
  const shPath = findExecutableInPath('sh')
  if (!shPath) {
    throw new Error('sh shell requested but `sh` binary not found in PATH')
  }
  // sh 太精简（不保证支持 source / shopt），跳过 snapshot 直接 login-shell init
  return createBashShellProvider(shPath, { skipSnapshot: true })
})

const getPwshProvider = memoize(async (): Promise<ShellProvider> => {
  const pwshPath = findExecutableInPath('pwsh')
  if (!pwshPath) {
    throw new Error('pwsh shell requested but `pwsh` (PowerShell 7+) not found in PATH')
  }
  return createPowerShellProvider(pwshPath)
})

const getCmdProvider = memoize(async (): Promise<ShellProvider> => {
  if (getPlatform() !== 'windows') {
    throw new Error('cmd shell requested on non-Windows platform; use bash/zsh/sh instead')
  }
  // cmd.exe 在 Windows 上几乎一定在 SystemRoot 下，但仍走 PATH 检查避免硬编码
  const cmdPath = findExecutableInPath('cmd') ?? `${process.env.SystemRoot ?? 'C:\\Windows'}\\System32\\cmd.exe`
  if (!isExecutable(cmdPath)) {
    throw new Error(`cmd shell requested but cmd.exe not found at ${cmdPath}`)
  }
  return createCmdProvider(cmdPath)
})

const resolveProvider: Record<ShellType, () => Promise<ShellProvider>> = {
  bash: async () => (await getShellConfig()).provider,
  zsh: getZshProvider,
  sh: getShProvider,
  powershell: getPsProvider,
  pwsh: getPwshProvider,
  cmd: getCmdProvider,
}

export type ExecOptions = {
  timeout?: number
  onProgress?: (
    lastLines: string,
    allLines: string,
    totalLines: number,
    totalBytes: number,
    isIncomplete: boolean,
  ) => void
  preventCwdChanges?: boolean
  shouldUseSandbox?: boolean
  shouldAutoBackground?: boolean
  /** When provided, stdout is piped (not sent to file) and this callback fires on each data chunk. */
  onStdout?: (data: string) => void
}

/**
 * 沙箱化执行的墙钟分桶（PR-4 遥测）。
 *
 * **量的是整条命令，不是沙箱开销** —— TS 侧拿不到「helper 建沙箱花了多久」，
 * exec 之后进程就是命令本身。所以字段名叫 `command_duration_bucket` 而不是
 * `sandbox_overhead`：它能回答的是「开了沙箱之后命令时长分布有没有整体右移」
 * （群体级对照），不能回答单条命令的开销归因。init 失败那一档除外 —— 那时
 * 命令根本没跑，时长就是沙箱建立失败前的耗时。
 *
 * 计时终点是 `shellCommand.result` 落定的那一刻，也就是**命令结束或被转后台**
 * 二者较早的那个。被转后台的命令因此报的是「跑到转后台为止」，不是它的全部
 * 寿命 —— 写在这里免得有人拿这个桶去算后台任务时长。
 *
 * 分桶而不是发原始毫秒：原始时长在这里没有额外信息量，却是一个无界基数维度。
 */
const SANDBOX_EXEC_DURATION_BUCKETS: readonly {
  readonly maxMs: number
  readonly label: string
}[] = [
  { maxMs: 100, label: 'lt_100ms' },
  { maxMs: 500, label: 'lt_500ms' },
  { maxMs: 2_000, label: 'lt_2s' },
  { maxMs: 10_000, label: 'lt_10s' },
  { maxMs: 60_000, label: 'lt_60s' },
]

function sandboxExecDurationBucket(durationMs: number): string {
  for (const bucket of SANDBOX_EXEC_DURATION_BUCKETS) {
    if (durationMs < bucket.maxMs) return bucket.label
  }
  return 'gte_60s'
}

/**
 * 一次沙箱化执行的遥测（SoT §3 关联方 #23）。
 *
 * 载荷里**没有命令内容、没有路径、没有 helper 的 stderr 明细** —— 只有后端
 * 标识（封闭枚举）、init 失败与否、Rust 侧固定 slug 的失败原因、时长桶。
 */
function logSandboxExecEvent(args: {
  backend: string | null
  initFailureReason: string | null
  startedAtMs: number
}): void {
  logEvent('tengu_sandbox_exec', {
    backend: (args.backend ??
      'unknown') as unknown as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    init_failed: args.initFailureReason !== null,
    // slug 是 Rust 侧 `sandbox_exec.rs` 的固定枚举（`sandbox-apply-failed` /
    // `sandbox-verify-failed` / `exec-failed` / `config-*` / `invalid-argv`），
    // 外加 TS 侧的 `helper-not-found`。既不是代码也不是路径 —— 这正是这个断言
    // 类型要求调用方亲自确认的那件事。
    reason: (args.initFailureReason ??
      'none') as unknown as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    command_duration_bucket: sandboxExecDurationBucket(
      Date.now() - args.startedAtMs,
    ) as unknown as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })
}

/**
 * Execute a shell command using the environment snapshot
 * Creates a new shell process for each command execution
 */
export async function exec(
  command: string,
  abortSignal: AbortSignal,
  shellType: ShellType,
  options?: ExecOptions,
): Promise<ShellCommand> {
  const {
    timeout,
    onProgress,
    preventCwdChanges,
    shouldUseSandbox,
    shouldAutoBackground,
    onStdout,
  } = options ?? {}
  const commandTimeout = timeout || DEFAULT_TIMEOUT

  const provider = await resolveProvider[shellType]()

  const id = generateSandboxCommandId()

  // Sandbox temp directory - use per-user directory name to prevent multi-user permission conflicts
  const sandboxTmpDir = posixJoin(
    process.env.CRABCODE_TMPDIR || '/tmp',
    getCrabCodeTempDirName(),
    'sandbox-commands',
    id,
  )
  const resolvedSandboxTmpDir = nativeJoin(
    getCrabCodeTempDir(),
    'sandbox-commands',
    id,
  )

  // W-SANDBOX-ENFORCED-DEADCODE PR-2 + PR-6：enforced routing（把命令桥到
  // mediated runtime 去执行）整条链已退役并**删除**。它自 2026-05-06 起 100% 恒不
  // 命中——启用它需要一个从未被任何代码、脚本或测试设置过的环境变量，于是每条
  // 沙箱命令都落进 `wrapWithSandbox` 的 throw，而系统提示词就在旁边教模型用
  // `dangerouslyDisableSandbox:true` 绕过（SoT §1.1 死代码机制链）。PR-2 摘掉
  // 这里的调用，PR-6 删掉模块本体（routing 决策函数 / ShellCommand 包装器 /
  // IPC 适配器三件）。
  //
  // 现在沙箱化 = 给同一次本地 spawn 的 argv 加前缀（下方 `spawnBinary`
  // 决策处）。执行仍留在本文件，所以 TaskOutput / auto-background / tree-kill /
  // cwd 文件 / #29316 清扫 / hooks 全部按构造继承（SoT §3）。
  // 反漂移闸门在 `tests/unit/sandbox-exec-wiring.test.ts` 的「Shell.ts 源码合同」。
  const { commandString: builtCommand, cwdFilePath } =
    await provider.buildExecCommand(command, {
      id,
      sandboxTmpDir: shouldUseSandbox ? sandboxTmpDir : undefined,
      useSandbox: shouldUseSandbox ?? false,
    })

  // On Windows, cwdFilePath is a POSIX path (for bash's `pwd -P >| $path`),
  // but Node.js needs a native path for host-side open/read/cleanup.
  const nativeCwdFilePath =
    getPlatform() === 'windows'
      ? posixPathToWindowsPath(cwdFilePath)
      : cwdFilePath

  let sandboxCommandTempDir: string | undefined
  let cwdProbeHandle: FileHandle | undefined
  const cleanupSandboxCommandResources = async (): Promise<void> => {
    const handle = cwdProbeHandle
    cwdProbeHandle = undefined
    if (handle !== undefined) {
      try {
        await handle.close()
      } catch (error) {
        logError(error)
      }
    }

    const tempDir = sandboxCommandTempDir
    sandboxCommandTempDir = undefined
    if (tempDir !== undefined) {
      try {
        await cleanupSandboxCommandTempDir(tempDir)
      } catch (error) {
        logError(error)
      }
    }
  }

  // 命令串本身不再被改写 —— 沙箱化改的是 argv 前缀，不是命令内容
  // （旧 `wrapWithSandbox` 把它重写成另一个命令串，那条路已经退役）。
  const commandString = builtCommand

  let cwd = pwd()

  // Recover if the current working directory no longer exists on disk.
  // This can happen when a command deletes its own CWD (e.g., temp dir cleanup).
  try {
    await realpath(cwd)
  } catch {
    const fallback = getOriginalCwd()
    logForDebugging(
      `Shell CWD "${cwd}" no longer exists, recovering to "${fallback}"`,
    )
    try {
      await realpath(fallback)
      setCwdState(fallback)
      cwd = fallback
    } catch {
      return createFailedCommand(
        `Working directory "${cwd}" no longer exists. Please restart CrabCode from an existing directory.`,
      )
    }
  }

  // If already aborted, don't spawn the process at all
  if (abortSignal.aborted) {
    return createAbortedCommand()
  }

  const binShell = provider.shellPath

  // Sandboxed PowerShell: the sandbox's inner shell must stay POSIX —
  // using pwsh there would lose -NoProfile -NonInteractive (profile load
  // inside sandbox → delays, stray output, may hang on prompts). Instead:
  //   • powershellProvider.buildExecCommand (useSandbox) pre-wraps as
  //     `pwsh -NoProfile -NonInteractive -EncodedCommand <base64>` — base64
  //     survives quoting layers intact
  //   • pass /bin/sh as the sandbox's inner shell to exec that invocation
  //
  // This branch is POSIX-only. Since W-SANDBOX-ENFORCED-DEADCODE PR-3 that is
  // no longer guaranteed by "the sandbox does not run on Windows" — PR-3 wired
  // that backend. What guarantees it today is that `shellType === 'powershell'`
  // and Windows are mutually exclusive with sandboxing, via two independent
  // constructions:
  //   • `isPowerShellToolEnabled()` (utils/shell/shellToolUtils.ts) returns
  //     false off Windows, so every path that routes to the pwsh shell type —
  //     PowerShellTool's tool-list gate, processBashCommand's `!` routing,
  //     promptShellExecution's frontmatter routing — is Windows-only; and
  //   • on Windows, `shouldSandboxPowerShell()` hard-codes false.
  // So `shellType === 'powershell' && shouldUseSandbox` is constructively
  // unreachable and this branch is a tripwire, not a live path. If anyone ever
  // lets Windows reach here, `/bin/sh` will not exist and the spawn will fail;
  // the fix then is a Windows outer shell, not deleting this comment.
  const isSandboxedPowerShell = shouldUseSandbox && shellType === 'powershell'

  // 未沙箱化时的最终 spawn 席位。沙箱化只是在它前面加一段 argv 前缀 —— 被执行的
  // 二进制与参数一个字节都不变，这正是关联方全部按构造继承的原因。
  let spawnBinary = isSandboxedPowerShell ? '/bin/sh' : binShell
  let shellArgs = isSandboxedPowerShell
    ? ['-c', commandString]
    : provider.getSpawnArgs(commandString)

  // PR-8：本条命令要不要注入 proxy env。0 = 不注入（没配域名规则，或代理没起来）。
  // 在这里声明是因为它的产地（沙箱分支）与用处（spawnEnv）隔着一段。
  let sandboxNetworkProxyPort = 0
  let sandboxExecConfigJson: string | null = null

  // 遥测（PR-4，SoT §3 关联方 #23）在 spawn 前固定 backend 与起始时刻。
  // 子进程的退出码和 stderr 都不可信，后续不得用它们改写会话安全状态。
  const sandboxBackendAtSpawn = shouldUseSandbox
    ? (SandboxManager.getSandboxBackend() ?? null)
    : null
  const sandboxExecStartedAtMs = Date.now()

  if (shouldUseSandbox) {
    // helper 解析复用探测层的那一份（`resolveSandboxHelperBin`），不另写一套：
    // 第 4 道门放行的依据就是它解析出来的那个二进制，这里再解析一次不同的路径
    // 等于「探测的是 A、执行的是 B」。
    const helperBin = resolveSandboxHelperBin()
    if (helperBin === null) {
      // 第 4 道门刚说过后端可用，helper 却解析不出来 —— 只可能是会话中途二进制
      // 布局变了。fail-closed：这条命令诚实失败，**绝不**悄悄裸跑一遍。
      logSandboxExecEvent({
        backend: sandboxBackendAtSpawn,
        initFailureReason: 'helper-not-found',
        startedAtMs: sandboxExecStartedAtMs,
      })
      markEnforcedBackendDegraded('helper-not-found')
      return createFailedCommand(
        `Sandbox is enabled but the sandbox helper binary could not be resolved. ` +
          `The command was NOT run (running it unsandboxed would silently break the ` +
          `isolation this session promised). Sandboxing is now degraded to off for ` +
          `this session — run /sandbox for details.`,
      )
    }

    const { configJson, networkProxyPort } = await buildSandboxExecConfig({
        cwd,
        tmpDir: resolvedSandboxTmpDir,
        tmpDirAliases: [sandboxTmpDir],
        // cwd 探针文件由 buildExecCommand 烘进命令串，必须放行，否则 cd 传播
        // 静默失效（命令写不了它 ⇒ 宿主读回空 ⇒ 用户的 `cd` 悄悄不生效）。
        cwdFile: cwdFilePath,
      })
    sandboxCommandTempDir = resolvedSandboxTmpDir
    try {
      // Bind the host read to the inode created before spawn. The command can
      // unlink/replace the pathname, but that can only make cwd propagation
      // fail closed; it cannot redirect the host to an attacker-selected file.
      cwdProbeHandle = await open(nativeCwdFilePath, 'wx+', 0o600)
    } catch (error) {
      await cleanupSandboxCommandResources()
      throw error
    }
    sandboxNetworkProxyPort = networkProxyPort
    sandboxExecConfigJson = configJson

    const prefixed = buildSandboxExecArgv({
      helperBin,
      program: spawnBinary,
      args: shellArgs,
    })
    spawnBinary = prefixed.binary
    shellArgs = prefixed.args
  }

  let envOverrides: Record<string, string>
  try {
    envOverrides = await provider.getEnvironmentOverrides(command)
  } catch (error) {
    await cleanupSandboxCommandResources()
    throw error
  }

  // When onStdout is provided, use pipe mode: stdout flows through
  // StreamWrapper → TaskOutput in-memory buffer instead of a file fd.
  // This lets callers receive real-time stdout callbacks.
  const usePipeMode = !!onStdout
  const taskId = generateTaskId('local_bash')
  const taskOutput = new TaskOutput(taskId, onProgress ?? null, !usePipeMode)
  let outputHandle: FileHandle | undefined
  try {
    // W-TASK-OUTPUT-ISOLATION 问题 E —— 按**这个任务自己**的产物路径建目录，不按「当前会话
    // 目录」。TaskOutput 上一行已经把路径 pin 死了；用当前会话目录会在跨 /clear 或跨
    // 线程时 mkdir 一个目录、却 open 另一个路径。
    await ensureTaskOutputDir(taskId)

    // F1a (2026-07-24 文档解析根因重审): append the vendored document-parsing
    // engine dirs that actually exist right now to the child PATH tail, so
    // `which pandoc` 等自检与产品 resolver 看到同一真源（win32 三件套；
    // mac/linux 仅 pandoc+soffice，poppler 因需 lib/ 动态库刻意不入 PATH）。
    // Append-not-prepend：不遮蔽用户系统安装。现探不缓存（负缓存 = R1 根因）。
    const spawnEnv: Record<string, string | undefined> = {
      ...subprocessEnv(),
      SHELL: shellType === 'bash' ? binShell : undefined,
      GIT_EDITOR: 'true',
      CRABCODE: '1',
      ...envOverrides,
      ...(process.env.USER_TYPE === 'ant'
        ? {
            CRABCODE_SESSION_ID: getSessionId(),
          }
        : {}),
      // 必须是**最后**一个展开：`subprocessEnv()` 会把宿主自己的 HTTPS_PROXY 继承
      // 下来，排在它前面就等于让用户的上游代理接管沙箱命令的流量，过滤面被整个绕开。
      // 端口 0 时 helper 返回 {}，本行是纯 no-op（非沙箱命令永远走这条）。
      ...sandboxProxyChildEnv(sandboxNetworkProxyPort),
    }
    appendVendoredEngineDirsToEnvPath(
      spawnEnv,
      await existingVendoredEngineDirs(),
    )

    // In file mode, both stdout and stderr go to the same host-opened fd.
    // Keep the host descriptor alive until process exit: the sandboxed child
    // may unlink or replace the pathname, but cannot redirect this descriptor
    // or evade the output-size watchdog. O_APPEND keeps interleaved writes
    // atomic; O_NONBLOCK prevents a replaced FIFO from blocking host cleanup.
    if (!usePipeMode) {
      const O_NOFOLLOW = fsConstants.O_NOFOLLOW ?? 0
      outputHandle = await open(
        taskOutput.path,
        process.platform === 'win32'
          ? 'wx+'
          : fsConstants.O_RDWR |
              fsConstants.O_CREAT |
              fsConstants.O_EXCL |
              fsConstants.O_APPEND |
              fsConstants.O_NONBLOCK |
              O_NOFOLLOW,
      )
      const outputStats = await outputHandle.stat({ bigint: true })
      bindTaskOutputFileIdentity(taskId, {
        dev: outputStats.dev,
        ino: outputStats.ino,
      })
    }

    const childProcess = spawn(spawnBinary, shellArgs, {
      env: spawnEnv,
      cwd,
      // File mode is also piped: ShellCommand drains both streams into the
      // host-owned descriptor and closes inherited writers after leader exit.
      // The child never receives the output-file descriptor itself.
      stdio: ['pipe', 'pipe', 'pipe'],
      // Don't pass the signal - we'll handle termination ourselves with tree-kill
      detached: provider.detached,
      // Prevent visible console window on Windows (no-op on other platforms)
      windowsHide: true,
    })

    if (sandboxExecConfigJson !== null) {
      // A private one-shot pipe has no pathname another sandboxed command can
      // replace. The helper consumes it before applying policy and execing the
      // user command; that command receives an already-closed stdin.
      const controlStdin = childProcess.stdin
      if (controlStdin === null) {
        childProcess.kill()
        throw new Error('sandbox helper control stdin was not created')
      }
      controlStdin.on('error', () => {
        // wrapSpawn owns spawn/exit reporting. Swallow the duplicate EPIPE
        // that an early helper failure can emit on this control pipe.
      })
      controlStdin.end(sandboxExecConfigJson)
    }

    const shellCommand = wrapSpawn(
      childProcess,
      abortSignal,
      commandTimeout,
      taskOutput,
      shouldAutoBackground,
      outputHandle,
    )
    // Ownership moved to ShellCommandImpl. The catch below must not close the
    // descriptor while a successfully spawned child is still using its dup.
    outputHandle = undefined

    // In pipe mode, attach the caller's callbacks alongside StreamWrapper.
    // Both listeners receive the same data chunks (Node.js ReadableStream supports
    // multiple 'data' listeners). StreamWrapper feeds TaskOutput for persistence;
    // these callbacks give the caller real-time access.
    if (childProcess.stdout && onStdout) {
      childProcess.stdout.on('data', (chunk: string | Buffer) => {
        onStdout(typeof chunk === 'string' ? chunk : chunk.toString())
      })
    }

    // Attach cleanup to the command result
    // NOTE: readFileSync/unlinkSync are intentional here — these must complete
    // synchronously within the .then() microtask so that callers who
    // `await shellCommand.result` see the updated cwd immediately after.
    // Using async readFile would introduce a microtask boundary, causing
    // a race where cwd hasn't been updated yet when the caller continues.

    void shellCommand.result.then(async result => {
      try {
        // On Linux, bwrap creates 0-byte mount-point files on the host to deny
        // writes to non-existent paths (.bashrc, HEAD, etc.). These persist after
        // bwrap exits as ghost dotfiles in cwd. Cleanup is synchronous and a no-op
        // on macOS. Keep before any await so callers awaiting .result see a clean
        // working tree in the same microtask.
        //
        // W-SANDBOX-ENFORCED-DEADCODE PR-2：enforced 早返退役后，沙箱路径**也**
        // 流到这里 —— 于是 #29316 的 bare-repo scrub 第一次真的覆盖沙箱化命令
        // （关联方矩阵 #16）。旧注释说的「enforced 路径不进入此块」已经不再成立。
        if (shouldUseSandbox) {
          SandboxManager.cleanupAfterCommand()

        // Child output is untrusted, including exit 125 and helper-looking
        // marker lines: a cooperating/background sandboxed command can observe
        // shared temp files and forge either. Never let it mutate the cached
        // session policy. A real helper initialization failure remains a
        // fail-closed failure for this command, and the next command probes/
        // applies the helper again. Only host-side facts (such as resolving no
        // helper above) may call markEnforcedBackendDegraded().
          const initFailure = null
        // PR-4：**每一次**沙箱化执行发一条，init 成没成功是它的一个维度。
        // PR-2 的 `tengu_sandbox_exec_init_failed` 由本事件取代（并入而非并列）：
        // 只发失败事件的话，分母不存在 —— 「沙箱化命令里有多大比例起不来」
        // 这个唯一值得问的问题就得跨两个事件流去 join。
          logSandboxExecEvent({
            backend: sandboxBackendAtSpawn,
            initFailureReason: initFailure,
            startedAtMs: sandboxExecStartedAtMs,
          })
        }
        // Only foreground tasks update the cwd
        if (result && !preventCwdChanges && !result.backgroundTaskId) {
          try {
            let newCwd = readFileSync(
              cwdProbeHandle?.fd ?? nativeCwdFilePath,
              { encoding: 'utf8' },
            ).trim()
            if (getPlatform() === 'windows') {
              newCwd = posixPathToWindowsPath(newCwd)
            }
            // cwd is NFC-normalized (setCwdState); newCwd from `pwd -P` may be
            // NFD on macOS APFS. Normalize before comparing so Unicode paths
            // don't false-positive as "changed" on every command.
            if (newCwd.normalize('NFC') !== cwd) {
              setCwd(newCwd, cwd)
              invalidateSessionEnvCache()
              void onCwdChangedForHooks(cwd, newCwd)
            }
          } catch {
            logEvent('tengu_shell_set_cwd', { success: false })
          }
        }
      } finally {
        if (shouldUseSandbox) {
          await cleanupSandboxCommandResources()
        } else {
          // Clean up the non-sandbox temp file used for cwd tracking.
          try {
            unlinkSync(nativeCwdFilePath)
          } catch {
            // File may not exist if command failed before pwd -P ran
          }
        }
      }
    })

    return shellCommand
  } catch (error) {
    await cleanupSandboxCommandResources()
    // Close the fd if spawn failed (child never got its dup)
    if (outputHandle !== undefined) {
      try {
        await outputHandle.close()
      } catch {
        // May already be closed
      }
    }
    taskOutput.clear()
    // The output file is already on disk by now: in file mode it IS the child's
    // stdio target, so `open()` above created it before `spawn` was even called.
    // If `spawn` threw, nothing ever wrote to it and nothing ever will — no
    // child, no `ShellCommandImpl`, therefore no `#handleExit`, which is the
    // only other place a foreground command's output file is unlinked. Without
    // this the run leaves a 0-byte file in the session's tasks directory that
    // no code path in the repo ever collects (there is no sweeper: the one
    // `cleanupTaskOutput` helper has zero callers).
    //
    // Safe against the two cases that must keep their file: a backgrounded task
    // (`background()` can only be called on a `ShellCommand` that was never
    // constructed here) and a large-output foreground run (there is no output).
    await taskOutput.deleteOutputFile()

    logForDebugging(`Shell exec error: ${errorMessage(error)}`)

    return createAbortedCommand(undefined, {
      code: 126, // Standard Unix code for execution errors
      stderr: errorMessage(error),
    })
  }
}

/**
 * Set the current working directory
 */
export function setCwd(path: string, relativeTo?: string): void {
  const resolved = isAbsolute(path)
    ? path
    : resolve(relativeTo || getFsImplementation().cwd(), path)
  // Resolve symlinks to match the behavior of pwd -P.
  // realpathSync throws ENOENT if the path doesn't exist - convert to a
  // friendlier error message instead of a separate existsSync pre-check (TOCTOU).
  let physicalPath: string
  try {
    physicalPath = getFsImplementation().realpathSync(resolved)
  } catch (e) {
    if (isENOENT(e)) {
      throw new Error(`Path "${resolved}" does not exist`)
    }
    throw e
  }

  setCwdState(physicalPath)
  if (process.env.NODE_ENV !== 'test') {
    try {
      logEvent('tengu_shell_set_cwd', {
        success: true,
      })
    } catch (_error) {
      // Ignore logging errors to prevent test failures
    }
  }
}
