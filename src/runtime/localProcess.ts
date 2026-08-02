import { copyFile, link, stat as fsStat, truncate as fsTruncate } from 'fs/promises'
import { getCwd } from '../utils/cwd.js'
import { execFileNoThrowWithCwd } from '../utils/execFileNoThrow.js'
import { gitExe } from '../utils/git.js'
import { exec } from '../utils/Shell.js'
import {
  ensureToolResultsDir,
  getToolResultPath,
} from '../utils/toolResultStorage.js'

export interface GitContextQuery {
  cwd: string
}

export interface GitContextResult {
  status: string
  log: string
  userName: string
}

export interface GitCommandRequest {
  args: string[]
  cwd?: string
  preserveOutputOnError?: boolean
  timeout?: number
  env?: Record<string, string | undefined>
  stdin?: 'ignore' | 'inherit' | 'pipe'
  input?: string
}

export interface GitCommandResult {
  stdout: string
  stderr: string
  code: number
  error?: string
}

export interface CommandRequest {
  command: string
  args: string[]
  cwd?: string
  timeout?: number
  env?: Record<string, string | undefined>
  preserveOutputOnError?: boolean
  maxBuffer?: number
  shell?: boolean | string
  stdin?: 'ignore' | 'inherit' | 'pipe'
  input?: string
}

export interface CommandResult {
  stdout: string
  stderr: string
  code: number
  error?: string
}

export interface SpawnManagedRequest {
  command: string
  shell: 'bash' | 'zsh' | 'sh' | 'powershell' | 'pwsh' | 'cmd'
  dangerouslyDisableSandbox?: boolean
  cwd?: string
  timeout?: number
  env?: Record<string, string | undefined>
}

export interface SpawnManagedResult {
  stdout: string
  stderr: string
  exitCode: number
  interrupted?: boolean
  persistedOutputPath?: string
  persistedOutputSize?: number
  sandboxBackend?: string
  finalCwd?: string
}

export type SpawnProgressCallback = (
  lastLines: string,
  allLines: string,
  totalLines: number,
  totalBytes: number,
  isIncomplete: boolean,
) => void

export interface SpawnManagedOptions {
  onProgress?: SpawnProgressCallback
  abortSignal?: AbortSignal
}

export interface LocalProcessRuntime {
  getGitContext(query: GitContextQuery): Promise<GitContextResult>
  execGitCommand(request: GitCommandRequest): Promise<GitCommandResult>
  execGitCommandBatch(
    requests: GitCommandRequest[],
  ): Promise<GitCommandResult[]>
  execCommand(request: CommandRequest): Promise<CommandResult>
  spawnManaged(
    request: SpawnManagedRequest,
    options?: SpawnManagedOptions,
  ): Promise<SpawnManagedResult>
}

/** Process execution owned by the standalone TUI backend. */
export const localProcess: LocalProcessRuntime = {
  async getGitContext(query) {
    const options = {
      preserveOutputOnError: false,
      cwd: query.cwd || getCwd(),
    }
    const [statusResult, logResult, userNameResult] = await Promise.all([
      execFileNoThrowWithCwd(
        gitExe(),
        ['--no-optional-locks', 'status', '--short'],
        options,
      ),
      execFileNoThrowWithCwd(
        gitExe(),
        ['--no-optional-locks', 'log', '--oneline', '-n', '5'],
        options,
      ),
      execFileNoThrowWithCwd(gitExe(), ['config', 'user.name'], options),
    ])
    return {
      status: statusResult.stdout.trim(),
      log: logResult.stdout.trim(),
      userName: userNameResult.stdout.trim(),
    }
  },

  async execGitCommand(request) {
    return execFileNoThrowWithCwd(gitExe(), request.args, {
      preserveOutputOnError: request.preserveOutputOnError,
      timeout: request.timeout,
      cwd: request.cwd || getCwd(),
      env: request.env,
      stdin: request.stdin,
      input: request.input,
    })
  },

  async execGitCommandBatch(requests) {
    return Promise.all(requests.map(request => localProcess.execGitCommand(request)))
  },

  async execCommand(request) {
    return execFileNoThrowWithCwd(request.command, request.args, {
      timeout: request.timeout,
      preserveOutputOnError: request.preserveOutputOnError,
      cwd: request.cwd || getCwd(),
      env: request.env,
      maxBuffer: request.maxBuffer,
      shell: request.shell,
      stdin: request.stdin,
      input: request.input,
    })
  },

  async spawnManaged(request, options) {
    const signal = options?.abortSignal ?? new AbortController().signal
    const shellCommand = await exec(request.command, signal, request.shell, {
      timeout: request.timeout,
      onProgress: options?.onProgress,
      shouldUseSandbox: !request.dangerouslyDisableSandbox,
    })
    const result = await shellCommand.result
    shellCommand.cleanup()

    const maxPersistedSize = 64 * 1024 * 1024
    let persistedOutputPath: string | undefined
    let persistedOutputSize: number | undefined
    if (result.outputFilePath && result.outputTaskId) {
      try {
        const fileStat = await fsStat(result.outputFilePath)
        persistedOutputSize = fileStat.size
        await ensureToolResultsDir()
        const destination = getToolResultPath(result.outputTaskId, false)
        if (fileStat.size > maxPersistedSize) {
          await fsTruncate(result.outputFilePath, maxPersistedSize)
        }
        try {
          await link(result.outputFilePath, destination)
        } catch {
          await copyFile(result.outputFilePath, destination)
        }
        persistedOutputPath = destination
      } catch {
        // The temporary output may already have been cleaned up.
      }
    }

    return {
      stdout: result.stdout,
      stderr: result.stderr,
      exitCode: result.code,
      interrupted: result.interrupted || undefined,
      persistedOutputPath,
      persistedOutputSize,
      sandboxBackend: 'none',
    }
  },
}

// Compatibility name for call sites while the execution abstraction remains
// local to this process.
export const localExecBridge = localProcess
