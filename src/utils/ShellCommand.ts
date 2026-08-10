import type { ChildProcess } from 'child_process'
import { createWriteStream, type WriteStream } from 'fs'
import type { FileHandle } from 'fs/promises'
import type { Readable } from 'stream'
import treeKill from 'tree-kill'
import { generateTaskId } from '../Task.js'
import { formatDuration } from './format.js'
import {
  MAX_TASK_OUTPUT_BYTES,
  MAX_TASK_OUTPUT_BYTES_DISPLAY,
} from './task/diskOutput.js'
import { TaskOutput } from './task/TaskOutput.js'

export type ExecResult = {
  stdout: string
  stderr: string
  code: number
  interrupted: boolean
  backgroundTaskId?: string
  backgroundedByUser?: boolean
  /** Set when assistant-mode auto-backgrounded a long-running blocking command. */
  assistantAutoBackgrounded?: boolean
  /** Set when stdout was too large to fit inline — points to the output file on disk. */
  outputFilePath?: string
  /** Total size of the output file in bytes (set when outputFilePath is set). */
  outputFileSize?: number
  /** The task ID for the output file (set when outputFilePath is set). */
  outputTaskId?: string
  /** Error message when the command failed before spawning (e.g., deleted cwd). */
  preSpawnError?: string
}

export type ShellCommand = {
  background: (backgroundTaskId: string) => boolean
  result: Promise<ExecResult>
  kill: () => void
  status: 'running' | 'backgrounded' | 'completed' | 'killed'
  /**
   * Cleans up stream resources (event listeners).
   * Should be called after the command completes or is killed to prevent memory leaks.
   */
  cleanup: () => void
  onTimeout?: (
    callback: (backgroundFn: (taskId: string) => boolean) => void,
  ) => void
  /** The TaskOutput instance that owns all stdout/stderr data and progress. */
  taskOutput: TaskOutput
}

const SIGKILL = 137
const SIGTERM = 143

// File-mode output is drained through a host-owned sink. Poll the authoritative
// descriptor as a second line of defense against writes made through another fd.
const SIZE_WATCHDOG_INTERVAL_MS = 5_000
const POST_EXIT_PIPE_DRAIN_GRACE_MS = 100

function prependStderr(prefix: string, stderr: string): string {
  return stderr ? `${prefix} ${stderr}` : prefix
}

/**
 * Thin pipe from a child process stream into TaskOutput.
 * Used in pipe mode (hooks) for stdout and stderr.
 * In file mode (bash commands), both fds go to the output file —
 * the child process streams are null and no wrappers are created.
 */
class StreamWrapper {
  #stream: Readable | null
  #isCleanedUp = false
  #taskOutput: TaskOutput | null
  #isStderr: boolean
  #onData = this.#dataHandler.bind(this)

  constructor(stream: Readable, taskOutput: TaskOutput, isStderr: boolean) {
    this.#stream = stream
    this.#taskOutput = taskOutput
    this.#isStderr = isStderr
    // Emit strings instead of Buffers - avoids repeated .toString() calls
    stream.setEncoding('utf-8')
    stream.on('data', this.#onData)
  }

  #dataHandler(data: Buffer | string): void {
    const str = typeof data === 'string' ? data : data.toString()

    if (this.#isStderr) {
      this.#taskOutput!.writeStderr(str)
    } else {
      this.#taskOutput!.writeStdout(str)
    }
  }

  cleanup(): void {
    if (this.#isCleanedUp) {
      return
    }
    this.#isCleanedUp = true
    this.#stream!.removeListener('data', this.#onData)
    // Release references so the stream, its StringDecoder, and
    // the TaskOutput can be GC'd independently of this wrapper.
    this.#stream = null
    this.#taskOutput = null
    this.#onData = () => {}
  }
}

/**
 * Drains stdout and stderr into the host-opened output descriptor.
 *
 * The child receives only pipe write ends, never the output-file descriptor.
 * When the shell leader exits, inherited pipe writers get a short drain grace
 * and are then disconnected. This prevents an escaped grandchild from filling
 * an anonymous inode after unlinking the published output path.
 */
class FileOutputCollector {
  #streams: Readable[]
  #sink: WriteStream
  #endedStreams = new Set<Readable>()
  #allStreamsEnded: Promise<void>
  #resolveAllStreamsEnded: (() => void) | null = null
  #sinkSettled: Promise<void>
  #finishPromise: Promise<void> | null = null
  #bytesSeen = 0
  #limitTriggered = false
  #failureTriggered = false

  constructor(
    streams: Readable[],
    outputFileHandle: FileHandle,
    maxOutputBytes: number,
    onLimit: () => void,
    onFailure: () => void,
  ) {
    this.#streams = streams
    this.#allStreamsEnded = new Promise(resolve => {
      this.#resolveAllStreamsEnded = resolve
    })
    this.#sink = createWriteStream('', {
      fd: outputFileHandle.fd,
      autoClose: false,
    })
    this.#sinkSettled = new Promise(resolve => {
      this.#sink.once('finish', resolve)
      this.#sink.once('close', resolve)
      this.#sink.once('error', () => resolve())
    })
    this.#sink.on('error', () => {
      if (!this.#failureTriggered) {
        this.#failureTriggered = true
        onFailure()
      }
    })

    for (const stream of streams) {
      stream.on('data', (chunk: Buffer | string) => {
        this.#bytesSeen +=
          typeof chunk === 'string' ? Buffer.byteLength(chunk) : chunk.length
        if (!this.#limitTriggered && this.#bytesSeen > maxOutputBytes) {
          this.#limitTriggered = true
          onLimit()
        }
      })
      const markEnded = (): void => {
        this.#endedStreams.add(stream)
        if (this.#endedStreams.size === this.#streams.length) {
          this.#resolveAllStreamsEnded?.()
          this.#resolveAllStreamsEnded = null
        }
      }
      stream.once('end', markEnded)
      stream.once('close', markEnded)
      stream.once('error', () => {
        if (!this.#failureTriggered) {
          this.#failureTriggered = true
          onFailure()
        }
        markEnded()
      })
      stream.pipe(this.#sink, { end: false })
    }

    if (streams.length === 0) {
      this.#resolveAllStreamsEnded?.()
      this.#resolveAllStreamsEnded = null
    }
  }

  finishAfterLeaderExit(): Promise<void> {
    if (this.#finishPromise !== null) return this.#finishPromise
    this.#finishPromise = this.#finish()
    return this.#finishPromise
  }

  async #finish(): Promise<void> {
    let graceTimer: ReturnType<typeof setTimeout> | undefined
    try {
      await Promise.race([
        this.#allStreamsEnded,
        new Promise<void>(resolve => {
          graceTimer = setTimeout(resolve, POST_EXIT_PIPE_DRAIN_GRACE_MS)
          graceTimer.unref()
        }),
      ])
    } finally {
      if (graceTimer !== undefined) clearTimeout(graceTimer)
    }

    for (const stream of this.#streams) {
      stream.unpipe(this.#sink)
      if (!stream.destroyed) stream.destroy()
    }
    this.#sink.end()
    await this.#sinkSettled
  }
}

/**
 * Implementation of ShellCommand that wraps a child process.
 *
 * For bash commands: both stdout and stderr go to a file fd via
 * stdio[1] and stdio[2] — no JS involvement. Progress is extracted
 * by polling the file tail.
 * For hooks: pipe mode with StreamWrappers for real-time detection.
 */
class ShellCommandImpl implements ShellCommand {
  #status: 'running' | 'backgrounded' | 'completed' | 'killed' = 'running'
  #backgroundTaskId: string | undefined
  #stdoutWrapper: StreamWrapper | null
  #stderrWrapper: StreamWrapper | null
  #childProcess: ChildProcess
  #timeoutId: NodeJS.Timeout | null = null
  #sizeWatchdog: NodeJS.Timeout | null = null
  #killedForSize = false
  #killedForOutputIntegrity = false
  #leaderExited = false
  #outputFileHandle: FileHandle | null
  #fileOutputCollector: FileOutputCollector | null = null
  #maxOutputBytes: number
  #abortSignal: AbortSignal
  #onTimeoutCallback:
    | ((backgroundFn: (taskId: string) => boolean) => void)
    | undefined
  #timeout: number
  #shouldAutoBackground: boolean
  #resultResolver: ((result: ExecResult) => void) | null = null
  #exitCodeResolver: ((code: number) => void) | null = null
  #boundAbortHandler: (() => void) | null = null
  readonly taskOutput: TaskOutput

  static #handleTimeout(self: ShellCommandImpl): void {
    if (self.#shouldAutoBackground && self.#onTimeoutCallback) {
      self.#onTimeoutCallback(self.background.bind(self))
    } else {
      self.#doKill(SIGTERM)
    }
  }

  readonly result: Promise<ExecResult>
  readonly onTimeout?: (
    callback: (backgroundFn: (taskId: string) => boolean) => void,
  ) => void

  constructor(
    childProcess: ChildProcess,
    abortSignal: AbortSignal,
    timeout: number,
    taskOutput: TaskOutput,
    shouldAutoBackground = false,
    maxOutputBytes = MAX_TASK_OUTPUT_BYTES,
    outputFileHandle?: FileHandle,
  ) {
    this.#childProcess = childProcess
    this.#abortSignal = abortSignal
    this.#timeout = timeout
    this.#shouldAutoBackground = shouldAutoBackground
    this.#maxOutputBytes = maxOutputBytes
    this.#outputFileHandle = outputFileHandle ?? null
    this.taskOutput = taskOutput
    this.result = this.#createResultPromise()

    if (this.#outputFileHandle !== null) {
      const streams = [childProcess.stdout, childProcess.stderr].filter(
        (stream): stream is Readable => stream !== null,
      )
      this.#fileOutputCollector = new FileOutputCollector(
        streams,
        this.#outputFileHandle,
        this.#maxOutputBytes,
        () => {
          if (!this.#killedForSize) {
            this.#killedForSize = true
            this.#doKill(SIGKILL)
          }
        },
        () => {
          if (!this.#killedForOutputIntegrity) {
            this.#killedForOutputIntegrity = true
            this.#doKill(SIGKILL)
          }
        },
      )
      this.#stdoutWrapper = null
      this.#stderrWrapper = null
    } else {
      // Hook/explicit callback mode stays in memory.
      this.#stderrWrapper = childProcess.stderr
        ? new StreamWrapper(childProcess.stderr, taskOutput, true)
        : null
      this.#stdoutWrapper = childProcess.stdout
        ? new StreamWrapper(childProcess.stdout, taskOutput, false)
        : null
    }

    if (shouldAutoBackground) {
      this.onTimeout = (callback): void => {
        this.#onTimeoutCallback = callback
      }
    }

    if (this.#outputFileHandle !== null) {
      // Keep the host's original descriptor as the authority. A sandboxed
      // child may unlink or replace the pathname, but cannot redirect fstat on
      // this handle or evade the disk cap by writing the anonymous inode.
      this.#startSizeWatchdog()
    }
  }

  get status(): 'running' | 'backgrounded' | 'completed' | 'killed' {
    return this.#status
  }

  #abortHandler(): void {
    // On 'interrupt' (user submitted a new message), don't kill — let the
    // caller background the process so the model can see partial output.
    if (this.#abortSignal.reason === 'interrupt') {
      return
    }
    this.kill()
  }

  #exitHandler(code: number | null, signal: NodeJS.Signals | null): void {
    this.#leaderExited = true
    const exitCode =
      code !== null && code !== undefined
        ? code
        : signal === 'SIGTERM'
          ? 144
          : 1
    this.#resolveExitCode(exitCode)
  }

  #errorHandler(): void {
    this.#resolveExitCode(1)
  }

  #resolveExitCode(code: number): void {
    if (this.#exitCodeResolver) {
      this.#exitCodeResolver(code)
      this.#exitCodeResolver = null
    }
  }

  // Note: exit/error listeners are NOT removed here — they're needed for
  // the result promise to resolve. They clean up when the child process exits.
  #cleanupListeners(): void {
    this.#clearSizeWatchdog()
    const timeoutId = this.#timeoutId
    if (timeoutId) {
      clearTimeout(timeoutId)
      this.#timeoutId = null
    }
    const boundAbortHandler = this.#boundAbortHandler
    if (boundAbortHandler) {
      this.#abortSignal.removeEventListener('abort', boundAbortHandler)
      this.#boundAbortHandler = null
    }
  }

  #clearSizeWatchdog(): void {
    if (this.#sizeWatchdog) {
      clearInterval(this.#sizeWatchdog)
      this.#sizeWatchdog = null
    }
  }

  #startSizeWatchdog(): void {
    if (this.#sizeWatchdog !== null || this.#outputFileHandle === null) {
      return
    }
    this.#sizeWatchdog = setInterval(() => {
      const outputFileHandle = this.#outputFileHandle
      if (outputFileHandle === null) {
        return
      }
      void outputFileHandle.stat().then(
        stats => {
          // Bail if the watchdog was cleared while this stat was in flight
          // (process exited on its own) — otherwise we'd mislabel stderr.
          if (
            stats.size > this.#maxOutputBytes &&
            (this.#status === 'running' || this.#status === 'backgrounded') &&
            this.#sizeWatchdog !== null
          ) {
            this.#killedForSize = true
            this.#clearSizeWatchdog()
            this.#doKill(SIGKILL)
          }
        },
        () => {
          if (
            (this.#status === 'running' || this.#status === 'backgrounded') &&
            this.#sizeWatchdog !== null
          ) {
            this.#killedForOutputIntegrity = true
            this.#clearSizeWatchdog()
            this.#doKill(SIGKILL)
          }
        },
      )
    }, SIZE_WATCHDOG_INTERVAL_MS)
    this.#sizeWatchdog.unref()
  }

  #createResultPromise(): Promise<ExecResult> {
    this.#boundAbortHandler = this.#abortHandler.bind(this)
    this.#abortSignal.addEventListener('abort', this.#boundAbortHandler, {
      once: true,
    })

    // Use 'exit' not 'close': 'close' waits for stdio to close, which includes
    // grandchild processes that inherit file descriptors (e.g. `sleep 30 &`).
    // 'exit' fires when the shell itself exits, returning control immediately.
    this.#childProcess.once('exit', this.#exitHandler.bind(this))
    this.#childProcess.once('error', this.#errorHandler.bind(this))

    this.#timeoutId = setTimeout(
      ShellCommandImpl.#handleTimeout,
      this.#timeout,
      this,
    ) as NodeJS.Timeout

    const exitPromise = new Promise<number>(resolve => {
      this.#exitCodeResolver = resolve
    })

    return new Promise<ExecResult>(resolve => {
      this.#resultResolver = resolve
      void exitPromise.then(this.#handleExit.bind(this))
    })
  }

  async #handleExit(code: number): Promise<void> {
    // The leader PID is already terminal (or an explicit kill has begun).
    // Remove timeout/abort/watchdog callbacks before draining pipe buffers so
    // a reused PID cannot be targeted during the final flush.
    this.#cleanupListeners()
    await this.#fileOutputCollector?.finishAfterLeaderExit()
    if (this.#status === 'running' || this.#status === 'backgrounded') {
      this.#status = 'completed'
    }

    let stdout: string
    try {
      stdout = await this.taskOutput.getStdout(this.#outputFileHandle ?? undefined)
    } finally {
      const outputFileHandle = this.#outputFileHandle
      this.#outputFileHandle = null
      try {
        await outputFileHandle?.close()
      } catch {
        // The child already has its own descriptor; close failures must not
        // hide the command result.
      }
    }
    const effectiveCode =
      this.#killedForSize || this.#killedForOutputIntegrity ? SIGKILL : code
    const result: ExecResult = {
      code: effectiveCode,
      stdout,
      stderr: this.taskOutput.getStderr(),
      interrupted: effectiveCode === SIGKILL,
      backgroundTaskId: this.#backgroundTaskId,
    }

    if (this.taskOutput.stdoutToFile && !this.#backgroundTaskId) {
      if (this.taskOutput.outputFileRedundant) {
        // Small file — full content is in result.stdout, delete the file
        void this.taskOutput.deleteOutputFile()
      } else {
        // Large file — tell the caller where the full output lives
        result.outputFilePath = this.taskOutput.path
        result.outputFileSize = this.taskOutput.outputFileSize
        result.outputTaskId = this.taskOutput.taskId
      }
    }

    if (this.#killedForOutputIntegrity) {
      result.stderr = prependStderr(
        'Command killed: host output descriptor integrity check failed',
        result.stderr,
      )
    } else if (this.#killedForSize) {
      result.stderr = prependStderr(
        `Command killed: output file exceeded ${MAX_TASK_OUTPUT_BYTES_DISPLAY}`,
        result.stderr,
      )
    } else if (code === SIGTERM) {
      result.stderr = prependStderr(
        `Command timed out after ${formatDuration(this.#timeout)}`,
        result.stderr,
      )
    }

    const resultResolver = this.#resultResolver
    if (resultResolver) {
      this.#resultResolver = null
      resultResolver(result)
    }
  }

  #doKill(code?: number): void {
    this.#status = 'killed'
    if (!this.#leaderExited && this.#childProcess.pid) {
      treeKill(this.#childProcess.pid, 'SIGKILL')
    }
    this.#resolveExitCode(code ?? SIGKILL)
  }

  kill(): void {
    this.#doKill()
  }

  background(taskId: string): boolean {
    if (this.#status === 'running') {
      this.#backgroundTaskId = taskId
      this.#status = 'backgrounded'
      this.#cleanupListeners()
      if (this.taskOutput.stdoutToFile) {
        // File mode: child writes directly to the fd with no JS involvement.
        // The foreground timeout is gone, so watch file size to prevent
        // a stuck append loop from filling the disk (768GB incident).
        this.#startSizeWatchdog()
      } else {
        // Pipe mode: spill the in-memory buffer so readers can find it on disk.
        this.taskOutput.spillToDisk()
      }
      return true
    }
    return false
  }

  cleanup(): void {
    this.#stdoutWrapper?.cleanup()
    this.#stderrWrapper?.cleanup()
    this.taskOutput.clear()
    // Must run before nulling #abortSignal — #cleanupListeners() calls
    // removeEventListener on it. Without this, a kill()+cleanup() sequence
    // crashes: kill() queues #handleExit as a microtask, cleanup() nulls
    // #abortSignal, then #handleExit runs #cleanupListeners() on the null ref.
    this.#cleanupListeners()
    // Release references to allow GC of ChildProcess internals and AbortController chain
    this.#childProcess = null!
    this.#abortSignal = null!
    this.#onTimeoutCallback = undefined
  }
}

/**
 * Wraps a child process to enable flexible handling of shell command execution.
 */
export function wrapSpawn(
  childProcess: ChildProcess,
  abortSignal: AbortSignal,
  timeout: number,
  taskOutput: TaskOutput,
  shouldAutoBackground = false,
  outputFileHandle?: FileHandle,
  maxOutputBytes = MAX_TASK_OUTPUT_BYTES,
): ShellCommand {
  return new ShellCommandImpl(
    childProcess,
    abortSignal,
    timeout,
    taskOutput,
    shouldAutoBackground,
    maxOutputBytes,
    outputFileHandle,
  )
}

/**
 * Static ShellCommand implementation for commands that were aborted before execution.
 */
class AbortedShellCommand implements ShellCommand {
  readonly status = 'killed' as const
  readonly result: Promise<ExecResult>
  readonly taskOutput: TaskOutput

  constructor(opts?: {
    backgroundTaskId?: string
    stderr?: string
    code?: number
  }) {
    this.taskOutput = new TaskOutput(generateTaskId('local_bash'), null)
    this.result = Promise.resolve({
      code: opts?.code ?? 145,
      stdout: '',
      stderr: opts?.stderr ?? 'Command aborted before execution',
      interrupted: true,
      backgroundTaskId: opts?.backgroundTaskId,
    })
  }

  background(): boolean {
    return false
  }

  kill(): void {}

  cleanup(): void {}
}

export function createAbortedCommand(
  backgroundTaskId?: string,
  opts?: { stderr?: string; code?: number },
): ShellCommand {
  return new AbortedShellCommand({
    backgroundTaskId,
    ...opts,
  })
}

export function createFailedCommand(preSpawnError: string): ShellCommand {
  const taskOutput = new TaskOutput(generateTaskId('local_bash'), null)
  return {
    status: 'completed' as const,
    result: Promise.resolve({
      code: 1,
      stdout: '',
      stderr: preSpawnError,
      interrupted: false,
      preSpawnError,
    }),
    taskOutput,
    background(): boolean {
      return false
    },
    kill(): void {},
    cleanup(): void {},
  }
}
