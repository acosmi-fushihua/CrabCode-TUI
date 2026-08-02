import { execFileSync, spawn } from 'child_process'
import { constants as fsConstants, readFileSync, unlinkSync } from 'fs'
import { type FileHandle, mkdir, open, realpath } from 'fs/promises'
import memoize from 'lodash-es/memoize.js'
import { isAbsolute, resolve } from 'path'
import { join as nativeJoin } from 'path'
import { join as posixJoin } from 'path/posix'
import { logEvent } from 'src/services/analytics/index.js'
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
import { getTaskOutputDir } from './task/diskOutput.js'
import { TaskOutput } from './task/TaskOutput.js'
import { which } from './which.js'

export type { ExecResult } from './ShellCommand.js'

import { accessSync } from 'fs'
import { onCwdChangedForHooks } from './hooks/fileChangedWatcher.js'
import { getCrabCodeTempDirName } from './permissions/filesystem.js'
import { getPlatform } from './platform.js'
import { SandboxManager } from './sandbox/sandbox-adapter.js'
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
  /**
   * Phase 2 R1（2026-05-06）：ExecBridge fallback 防循环 sentinel。
   *
   * 当 execBridgeLocal.spawnManaged / execBridgeIpc.spawnManaged catch 块兜底
   * 调 Shell.exec 时设 true。Shell.exec routing pre-hook 看到此标志即跳过
   * ExecBridge 分支（直走 stub no-op 路径），避免 Shell.exec → ExecBridge.spawnManaged
   * → execBridgeLocal → Shell.exec → 又分支 的死循环。
   *
   * **不要在外部 caller 设此字段** — 仅 ExecBridge fallback 路径内部使用。
   * 详主真源 doc 附录 D.10.1。
   */
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

  const id = Math.floor(Math.random() * 0x10000)
    .toString(16)
    .padStart(4, '0')

  // Sandbox temp directory - use per-user directory name to prevent multi-user permission conflicts
  const sandboxTmpDir = posixJoin(
    process.env.CRABCODE_TMPDIR || '/tmp',
    getCrabCodeTempDirName(),
  )

  const { commandString: builtCommand, cwdFilePath } =
    await provider.buildExecCommand(command, {
      id,
      sandboxTmpDir: shouldUseSandbox ? sandboxTmpDir : undefined,
      useSandbox: shouldUseSandbox ?? false,
    })

  let commandString = builtCommand

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

  // Sandboxed PowerShell: wrapWithSandbox hardcodes `<binShell> -c '<cmd>'` —
  // using pwsh there would lose -NoProfile -NonInteractive (profile load
  // inside sandbox → delays, stray output, may hang on prompts). Instead:
  //   • powershellProvider.buildExecCommand (useSandbox) pre-wraps as
  //     `pwsh -NoProfile -NonInteractive -EncodedCommand <base64>` — base64
  //     survives the runtime's shellquote.quote() layer
  //   • pass /bin/sh as the sandbox's inner shell to exec that invocation
  //   • outer spawn is also /bin/sh -c to parse the runtime's POSIX output
  // /bin/sh exists on every platform where sandbox is supported.
  const isSandboxedPowerShell = shouldUseSandbox && shellType === 'powershell'
  const sandboxBinShell = isSandboxedPowerShell ? '/bin/sh' : binShell

  if (shouldUseSandbox) {
    commandString = await SandboxManager.wrapWithSandbox(
      commandString,
      sandboxBinShell,
      undefined,
      abortSignal,
    )
    // Create sandbox temp directory for sandboxed processes with secure permissions
    try {
      const fs = getFsImplementation()
      await fs.mkdir(sandboxTmpDir, { mode: 0o700 })
    } catch (error) {
      logForDebugging(`Failed to create ${sandboxTmpDir} directory: ${error}`)
    }
  }

  const spawnBinary = isSandboxedPowerShell ? '/bin/sh' : binShell
  const shellArgs = isSandboxedPowerShell
    ? ['-c', commandString]
    : provider.getSpawnArgs(commandString)
  const envOverrides = await provider.getEnvironmentOverrides(command)

  // When onStdout is provided, use pipe mode: stdout flows through
  // StreamWrapper → TaskOutput in-memory buffer instead of a file fd.
  // This lets callers receive real-time stdout callbacks.
  const usePipeMode = !!onStdout
  const taskId = generateTaskId('local_bash')
  const taskOutput = new TaskOutput(taskId, onProgress ?? null, !usePipeMode)
  await mkdir(getTaskOutputDir(), { recursive: true })

  // In file mode, both stdout and stderr go to the same file fd.
  // On POSIX, O_APPEND makes each write atomic (seek-to-end + write), so
  // stdout and stderr are interleaved chronologically without tearing.
  // On Windows, 'a' mode strips FILE_WRITE_DATA (only grants FILE_APPEND_DATA)
  // via libuv's fs__open. MSYS2/Cygwin probes inherited handles with
  // NtQueryInformationFile(FileAccessInformation) and treats handles without
  // FILE_WRITE_DATA as read-only, silently discarding all output. Using 'w'
  // grants FILE_GENERIC_WRITE. Atomicity is preserved because duplicated
  // handles share the same FILE_OBJECT with FILE_SYNCHRONOUS_IO_NONALERT,
  // which serializes all I/O through a single kernel lock.
  // SECURITY: O_NOFOLLOW prevents symlink-following attacks from the sandbox.
  // On Windows, use string flags — numeric flags can produce EINVAL through libuv.
  let outputHandle: FileHandle | undefined
  if (!usePipeMode) {
    const O_NOFOLLOW = fsConstants.O_NOFOLLOW ?? 0
    outputHandle = await open(
      taskOutput.path,
      process.platform === 'win32'
        ? 'w'
        : fsConstants.O_WRONLY |
            fsConstants.O_CREAT |
            fsConstants.O_APPEND |
            O_NOFOLLOW,
    )
  }

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
  }
  appendVendoredEngineDirsToEnvPath(
    spawnEnv,
    await existingVendoredEngineDirs(),
  )

  try {
    const childProcess = spawn(spawnBinary, shellArgs, {
      env: spawnEnv,
      cwd,
      stdio: usePipeMode
        ? ['pipe', 'pipe', 'pipe']
        : ['pipe', outputHandle?.fd, outputHandle?.fd],
      // Don't pass the signal - we'll handle termination ourselves with tree-kill
      detached: provider.detached,
      // Prevent visible console window on Windows (no-op on other platforms)
      windowsHide: true,
    })

    const shellCommand = wrapSpawn(
      childProcess,
      abortSignal,
      commandTimeout,
      taskOutput,
      shouldAutoBackground,
    )

    // Close our copy of the fd — the child has its own dup.
    // Must happen after wrapSpawn attaches 'error' listener, since the await
    // yields and the child's ENOENT 'error' event can fire in that window.
    // Wrapped in its own try/catch so a close failure (e.g. EIO) doesn't fall
    // through to the spawn-failure catch block, which would orphan the child.
    if (outputHandle !== undefined) {
      try {
        await outputHandle.close()
      } catch {
        // fd may already be closed by the child; safe to ignore
      }
    }

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

    // On Windows, cwdFilePath is a POSIX path (for bash's `pwd -P >| $path`),
    // but Node.js needs a native Windows path for readFileSync/unlinkSync.
    // Similarly, `pwd -P` outputs a POSIX path that must be converted before setCwd.
    const nativeCwdFilePath =
      getPlatform() === 'windows'
        ? posixPathToWindowsPath(cwdFilePath)
        : cwdFilePath

    void shellCommand.result.then(async result => {
      // On Linux, bwrap creates 0-byte mount-point files on the host to deny
      // writes to non-existent paths (.bashrc, HEAD, etc.). These persist after
      // bwrap exits as ghost dotfiles in cwd. Cleanup is synchronous and a no-op
      // on macOS. Keep before any await so callers awaiting .result see a clean
      // working tree in the same microtask.
      if (shouldUseSandbox) {
        SandboxManager.cleanupAfterCommand()
      }
      // Only foreground tasks update the cwd
      if (result && !preventCwdChanges && !result.backgroundTaskId) {
        try {
          let newCwd = readFileSync(nativeCwdFilePath, {
            encoding: 'utf8',
          }).trim()
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
      // Clean up the temp file used for cwd tracking
      try {
        unlinkSync(nativeCwdFilePath)
      } catch {
        // File may not exist if command failed before pwd -P ran
      }
    })

    return shellCommand
  } catch (error) {
    // Close the fd if spawn failed (child never got its dup)
    if (outputHandle !== undefined) {
      try {
        await outputHandle.close()
      } catch {
        // May already be closed
      }
    }
    taskOutput.clear()

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
