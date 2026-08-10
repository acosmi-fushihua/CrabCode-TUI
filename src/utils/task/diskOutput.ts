import { constants as fsConstants } from 'fs'
import {
  type FileHandle,
  mkdir,
  open,
  symlink,
  unlink,
} from 'fs/promises'
import { dirname, join, resolve } from 'path'
import { getSessionId } from '../../bootstrap/state.js'
import { getErrnoCode } from '../errors.js'
import {
  type FileIdentity,
  readFileRange,
  tailFile,
} from '../fsOperations.js'
import { logError } from '../log.js'
import { getProjectTempDir } from '../permissions/filesystem.js'

// SECURITY: O_NOFOLLOW prevents following symlinks when opening task output files.
// Without this, an attacker in the sandbox could create symlinks in the tasks directory
// pointing to arbitrary files, causing CrabCode on the host to write to those files.
// Windows uses string flags plus host backing/identity checks instead.
const O_NOFOLLOW = fsConstants.O_NOFOLLOW ?? 0

const DEFAULT_MAX_READ_BYTES = 8 * 1024 * 1024 // 8MB

/**
 * Disk cap for task output files. In file mode (bash), a watchdog polls
 * file size and kills the process. In pipe mode (hooks), DiskTaskOutput
 * drops chunks past this limit. Shared so both caps stay in sync.
 */
export const MAX_TASK_OUTPUT_BYTES = 5 * 1024 * 1024 * 1024
export const MAX_TASK_OUTPUT_BYTES_DISPLAY = '5GB'

/**
 * Get the task output directory for the CURRENT session.
 * Uses project temp storage; FileRead auto-allows only exact host-registered
 * task paths and then enforces this module's backing identity.
 *
 * The session ID is included so concurrent sessions in the same project don't
 * clobber each other's output files. Startup cleanup in one session previously
 * unlinked in-flight output files from other sessions — the writing process's fd
 * keeps the inode alive but reads via path fail ENOENT, and getStdout() returned
 * empty string (inc-4586 / boris-20260309-060423).
 *
 * This value must not be process-memoized: `/clear` can rotate the session
 * while an existing background task is still alive. Per-task pinning below
 * preserves that task's original location without freezing all future tasks
 * to the first session seen by the process.
 */
export function getTaskOutputDir(): string {
  return join(getProjectTempDir(), getSessionId(), 'tasks')
}

/**
 * Per-task pinned output paths.
 *
 * A task keeps the path selected on first resolution, even when `/clear`
 * rotates the current session. An unseen task starts in the current session.
 */
const _pinnedTaskOutputPaths = new Map<string, string>()
const _taskOutputFileIdentities = new Map<string, FileIdentity>()
const _taskOutputBackingPaths = new Map<string, string>()

function staleTaskOutputError(taskId: string, detail: string): Error {
  const error = new Error(
    `task output identity is unavailable for ${taskId}: ${detail}`,
  ) as NodeJS.ErrnoException
  error.code = 'ESTALE'
  return error
}

/** Bind host reads to the inode opened before the untrusted command starts. */
export function bindTaskOutputFileIdentity(
  taskId: string,
  identity: FileIdentity,
): void {
  if (identity.dev === 0n && identity.ino === 0n) {
    throw staleTaskOutputError(
      taskId,
      'the filesystem did not provide a stable file identity',
    )
  }
  const existing = _taskOutputFileIdentities.get(taskId)
  if (
    existing !== undefined &&
    (existing.dev !== identity.dev || existing.ino !== identity.ino)
  ) {
    throw staleTaskOutputError(taskId, 'the bound inode changed')
  }
  _taskOutputFileIdentities.set(taskId, identity)
}

/** Return the original inode or fail closed before opening an attacker path. */
export function requireTaskOutputFileIdentity(taskId: string): FileIdentity {
  const identity = _taskOutputFileIdentities.get(taskId)
  if (identity === undefined) {
    throw staleTaskOutputError(taskId, 'no host-opened inode was recorded')
  }
  return identity
}

/** Trusted backing path selected by the host (agent transcript or output file). */
export function getTaskOutputBackingPath(taskId: string): string {
  return _taskOutputBackingPaths.get(taskId) ?? getTaskOutputPath(taskId)
}

/**
 * Resolve a lazily-created transcript backing file to a stable identity.
 * Registration may precede the first persisted message, so symlink tasks are
 * allowed to remain unbound until the transcript actually materializes.
 */
export async function resolveTaskOutputFileIdentity(
  taskId: string,
): Promise<FileIdentity> {
  const existing = _taskOutputFileIdentities.get(taskId)
  if (existing !== undefined) return existing

  const backingPath = getTaskOutputBackingPath(taskId)
  await using handle = await open(
    backingPath,
    process.platform === 'win32'
      ? 'r'
      : fsConstants.O_RDONLY | fsConstants.O_NONBLOCK | O_NOFOLLOW,
  )
  const stats = await handle.stat({ bigint: true })
  if (!stats.isFile()) {
    throw staleTaskOutputError(taskId, 'the backing path is not a regular file')
  }
  const identity = { dev: stats.dev, ino: stats.ino }
  bindTaskOutputFileIdentity(taskId, identity)
  return identity
}

function taskOutputPathKey(path: string): string {
  const key = resolve(path).normalize('NFC')
  return process.platform === 'win32' ? key.toLowerCase() : key
}

/** Recognize a published task path without resolving an attacker-controlled symlink. */
export function getBoundTaskOutputForPath(
  path: string,
): { taskId: string; readPath: string } | null {
  const candidate = taskOutputPathKey(path)
  for (const [taskId, pinnedPath] of _pinnedTaskOutputPaths) {
    if (taskOutputPathKey(pinnedPath) === candidate) {
      return {
        taskId,
        readPath: getTaskOutputBackingPath(taskId),
      }
    }
  }
  return null
}

/** Test helper — clears the per-task pinned paths. */
export function _resetTaskOutputDirForTest(): void {
  _pinnedTaskOutputPaths.clear()
  _taskOutputFileIdentities.clear()
  _taskOutputBackingPaths.clear()
}

/**
 * Ensure the output directory for THIS task exists.
 *
 * Resolve through the task pin so directory creation and file open cannot
 * split across a session rotation.
 */
async function ensureOutputDir(taskId: string): Promise<void> {
  await mkdir(dirname(getTaskOutputPath(taskId)), { recursive: true })
}

/**
 * Get the output file path for a task — pinned at first resolution.
 */
export function getTaskOutputPath(taskId: string): string {
  const pinned = _pinnedTaskOutputPaths.get(taskId)
  if (pinned !== undefined) {
    return pinned
  }
  const resolved = join(getTaskOutputDir(), `${taskId}.output`)
  _pinnedTaskOutputPaths.set(taskId, resolved)
  return resolved
}

/** Release a terminal task's session pin after its artifact is gone. */
export function releaseTaskOutputPath(taskId: string): void {
  _pinnedTaskOutputPaths.delete(taskId)
  _taskOutputFileIdentities.delete(taskId)
  _taskOutputBackingPaths.delete(taskId)
}

/**
 * Ensure the output directory for `taskId` exists. Exported for callers that
 * create the directory before handing the task a file descriptor (Shell.ts).
 */
export function ensureTaskOutputDir(taskId: string): Promise<void> {
  return ensureOutputDir(taskId)
}

// Tracks fire-and-forget promises (initTaskOutput, initTaskOutputAsSymlink,
// evictTaskOutput, #drain) so tests can drain before teardown. Prevents the
// async-ENOENT-after-teardown flake class (#24957, #25065): a voided async
// resumes after preload's afterEach nuked the temp dir → ENOENT → unhandled
// rejection → flaky test failure. allSettled so a rejection doesn't short-
// circuit the drain and leave other ops racing the rmSync.
const _pendingOps = new Set<Promise<unknown>>()
function track<T>(p: Promise<T>): Promise<T> {
  _pendingOps.add(p)
  void p.finally(() => _pendingOps.delete(p)).catch(() => {})
  return p
}

/**
 * Encapsulates async disk writes for a single task's output.
 *
 * Uses a flat array as a write queue processed by a single drain loop,
 * so each chunk can be GC'd immediately after its write completes.
 * This avoids the memory retention problem of chained .then() closures
 * where every reaction captures its data until the whole chain resolves.
 */
export class DiskTaskOutput {
  #taskId: string
  #path: string
  #fileHandle: FileHandle | null = null
  #queue: string[] = []
  #bytesWritten = 0
  #capped = false
  #flushPromise: Promise<void> | null = null
  #flushResolve: (() => void) | null = null

  constructor(taskId: string) {
    this.#taskId = taskId
    this.#path = getTaskOutputPath(taskId)
  }

  append(content: string): void {
    if (this.#capped) {
      return
    }
    // content.length (UTF-16 code units) undercounts UTF-8 bytes by at most ~3×.
    // Acceptable for a coarse disk-fill guard — avoids re-scanning every chunk.
    this.#bytesWritten += content.length
    if (this.#bytesWritten > MAX_TASK_OUTPUT_BYTES) {
      this.#capped = true
      this.#queue.push(
        `\n[output truncated: exceeded ${MAX_TASK_OUTPUT_BYTES_DISPLAY} disk cap]\n`,
      )
    } else {
      this.#queue.push(content)
    }
    if (!this.#flushPromise) {
      this.#flushPromise = new Promise<void>(resolve => {
        this.#flushResolve = resolve
      })
      void track(this.#drain())
    }
  }

  flush(): Promise<void> {
    return this.#flushPromise ?? Promise.resolve()
  }

  cancel(): void {
    this.#queue.length = 0
  }

  async #drainAllChunks(): Promise<void> {
    while (true) {
      try {
        if (!this.#fileHandle) {
          // 直接用本实例 pin 住的路径推目录 —— 这是「产物路径属于任务」最强的形式：
          // 连 taskId 都不必再解析一次，写入目录与写入文件恒同源。
          await mkdir(dirname(this.#path), { recursive: true })
          const alreadyBound = _taskOutputFileIdentities.has(this.#taskId)
          this.#fileHandle = await open(
            this.#path,
            process.platform === 'win32'
              ? alreadyBound
                ? 'a'
                : 'ax'
              : fsConstants.O_WRONLY |
                  fsConstants.O_APPEND |
                  fsConstants.O_CREAT |
                  (alreadyBound ? 0 : fsConstants.O_EXCL) |
                  fsConstants.O_NONBLOCK |
                  O_NOFOLLOW,
          )
          const stats = await this.#fileHandle.stat({ bigint: true })
          if (!stats.isFile()) {
            throw staleTaskOutputError(
              this.#taskId,
              'the output path is not a regular file',
            )
          }
          bindTaskOutputFileIdentity(this.#taskId, {
            dev: stats.dev,
            ino: stats.ino,
          })
        }
        while (true) {
          await this.#writeAllChunks()
          if (this.#queue.length === 0) {
            break
          }
        }
      } finally {
        if (this.#fileHandle) {
          const fileHandle = this.#fileHandle
          this.#fileHandle = null
          await fileHandle.close()
        }
      }
      // you could have another .append() while we're waiting for the file to close, so we check the queue again before fully exiting
      if (this.#queue.length) {
        continue
      }

      break
    }
  }

  #writeAllChunks(): Promise<void> {
    // This code is extremely precise.
    // You **must not** add an await here!! That will cause memory to balloon as the queue grows.
    // It's okay to add an `await` to the caller of this method (e.g. #drainAllChunks) because that won't cause Buffer[] to be kept alive in memory.
    return this.#fileHandle!.appendFile(
      // This variable needs to get GC'd ASAP.
      this.#queueToBuffers(),
    )
  }

  /** Keep this in a separate method so that GC doesn't keep it alive for any longer than it should. */
  #queueToBuffers(): Buffer {
    // Use .splice to in-place mutate the array, informing the GC it can free it.
    const queue = this.#queue.splice(0, this.#queue.length)

    let totalLength = 0
    for (const str of queue) {
      totalLength += Buffer.byteLength(str, 'utf8')
    }

    const buffer = Buffer.allocUnsafe(totalLength)
    let offset = 0
    for (const str of queue) {
      offset += buffer.write(str, offset, 'utf8')
    }

    return buffer
  }

  async #drain(): Promise<void> {
    try {
      await this.#drainAllChunks()
    } catch (e) {
      // Transient fs errors (EMFILE on busy CI, EPERM on Windows pending-
      // delete) previously rode up through `void this.#drain()` as an
      // unhandled rejection while the flush promise resolved anyway — callers
      // saw an empty file with no error. Retry once for the transient case
      // (queue is intact if open() failed), then log and give up.
      logError(e)
      if (this.#queue.length > 0) {
        try {
          await this.#drainAllChunks()
        } catch (e2) {
          logError(e2)
        }
      }
    } finally {
      const resolve = this.#flushResolve!
      this.#flushPromise = null
      this.#flushResolve = null
      resolve()
    }
  }
}

const outputs = new Map<string, DiskTaskOutput>()

/**
 * Test helper — cancel pending writes, await in-flight ops, clear the map.
 * backgroundShells.test.ts and other task tests spawn real shells that
 * write through this module without afterEach cleanup; their entries
 * leak into diskOutput.test.ts on the same shard.
 *
 * Awaits all tracked promises until the set stabilizes — a settling promise
 * may spawn another (initTaskOutputAsSymlink's catch → initTaskOutput).
 * Call this in afterEach BEFORE rmSync to avoid async-ENOENT-after-teardown.
 */
export async function _clearOutputsForTest(): Promise<void> {
  for (const output of outputs.values()) {
    output.cancel()
  }
  while (_pendingOps.size > 0) {
    await Promise.allSettled([..._pendingOps])
  }
  outputs.clear()
}

function getOrCreateOutput(taskId: string): DiskTaskOutput {
  let output = outputs.get(taskId)
  if (!output) {
    output = new DiskTaskOutput(taskId)
    outputs.set(taskId, output)
  }
  return output
}

/**
 * Append output to a task's disk file asynchronously.
 * Creates the file if it doesn't exist.
 */
export function appendTaskOutput(taskId: string, content: string): void {
  getOrCreateOutput(taskId).append(content)
}

/**
 * Wait for all pending writes for a task to complete.
 * Useful before reading output to ensure all data is flushed.
 */
export async function flushTaskOutput(taskId: string): Promise<void> {
  const output = outputs.get(taskId)
  if (output) {
    await output.flush()
  }
}

/**
 * Evict a task's DiskTaskOutput from the in-memory map after flushing.
 * Unlike cleanupTaskOutput, this does not delete the output file on disk.
 * Call this when a task completes and its output has been consumed.
 *
 * Keep the pin while the file remains readable; cleanup removes both.
 */
export function evictTaskOutput(taskId: string): Promise<void> {
  return track(
    (async () => {
      const output = outputs.get(taskId)
      if (output) {
        await output.flush()
        outputs.delete(taskId)
      }
    })(),
  )
}

/**
 * Get delta (new content) since last read.
 * Reads only from the byte offset, up to maxBytes — never loads the full file.
 */
export async function getTaskOutputDelta(
  taskId: string,
  fromOffset: number,
  maxBytes: number = DEFAULT_MAX_READ_BYTES,
): Promise<{ content: string; newOffset: number }> {
  try {
    const identity = await resolveTaskOutputFileIdentity(taskId)
    const result = await readFileRange(
      getTaskOutputBackingPath(taskId),
      fromOffset,
      maxBytes,
      identity,
    )
    if (!result) {
      return { content: '', newOffset: fromOffset }
    }
    return {
      content: result.content,
      newOffset: fromOffset + result.bytesRead,
    }
  } catch (e) {
    const code = getErrnoCode(e)
    if (code === 'ENOENT') {
      return { content: '', newOffset: fromOffset }
    }
    logError(e)
    return { content: '', newOffset: fromOffset }
  }
}

/**
 * Get output for a task, reading the tail of the file.
 * Caps at maxBytes to avoid loading multi-GB files into memory.
 */
export async function getTaskOutput(
  taskId: string,
  maxBytes: number = DEFAULT_MAX_READ_BYTES,
): Promise<string> {
  try {
    const identity = await resolveTaskOutputFileIdentity(taskId)
    const { content, bytesTotal, bytesRead } = await tailFile(
      getTaskOutputBackingPath(taskId),
      maxBytes,
      identity,
    )
    if (bytesTotal > bytesRead) {
      return `[${Math.round((bytesTotal - bytesRead) / 1024)}KB of earlier output omitted]\n${content}`
    }
    return content
  } catch (e) {
    const code = getErrnoCode(e)
    if (code === 'ENOENT') {
      return ''
    }
    logError(e)
    return ''
  }
}

/**
 * Get the current size (offset) of a task's output file.
 */
export async function getTaskOutputSize(taskId: string): Promise<number> {
  try {
    const path = getTaskOutputBackingPath(taskId)
    const identity = await resolveTaskOutputFileIdentity(taskId)
    const result = await readFileRange(path, 0, 0, identity)
    return result?.bytesTotal ?? 0
  } catch (e) {
    const code = getErrnoCode(e)
    if (code === 'ENOENT') {
      return 0
    }
    logError(e)
    return 0
  }
}

/**
 * Copy a task artifact through its original inode, never through an untrusted
 * replacement pathname. The source may be truncated only through that same
 * verified handle.
 */
export async function persistTaskOutputFile(
  taskId: string,
  destination: string,
  maxBytes: number,
): Promise<number> {
  const identity = await resolveTaskOutputFileIdentity(taskId)
  const sourcePath = getTaskOutputBackingPath(taskId)
  const source = await open(
    sourcePath,
    process.platform === 'win32'
      ? 'r+'
      : fsConstants.O_RDWR | fsConstants.O_NONBLOCK | O_NOFOLLOW,
  )
  let destinationHandle: FileHandle | null = null
  let removeIncompleteDestination = false
  try {
    const stats = await source.stat({ bigint: true })
    if (stats.dev !== identity.dev || stats.ino !== identity.ino) {
      throw staleTaskOutputError(taskId, 'the path was replaced before persistence')
    }

    const originalSize = Number(stats.size)
    const bytesToCopy = Math.min(originalSize, maxBytes)
    if (originalSize > maxBytes) {
      await source.truncate(maxBytes)
    }

    // The task id makes this destination unique. Refuse replacement instead of
    // following or overwriting an entry created by an untrusted process.
    destinationHandle = await open(destination, 'wx', 0o600)
    const buffer = Buffer.allocUnsafe(Math.min(1024 * 1024, bytesToCopy || 1))
    let sourceOffset = 0
    while (sourceOffset < bytesToCopy) {
      const requested = Math.min(buffer.length, bytesToCopy - sourceOffset)
      const { bytesRead } = await source.read(
        buffer,
        0,
        requested,
        sourceOffset,
      )
      if (bytesRead === 0) {
        break
      }
      let written = 0
      while (written < bytesRead) {
        const result = await destinationHandle.write(
          buffer,
          written,
          bytesRead - written,
          null,
        )
        written += result.bytesWritten
      }
      sourceOffset += bytesRead
    }
    if (sourceOffset !== bytesToCopy) {
      throw new Error(
        `task output ended while persisting ${taskId}: expected ${bytesToCopy}, copied ${sourceOffset}`,
      )
    }
    return originalSize
  } catch (error) {
    removeIncompleteDestination = destinationHandle !== null
    throw error
  } finally {
    await Promise.allSettled([
      destinationHandle?.close() ?? Promise.resolve(),
      source.close(),
    ])
    if (removeIncompleteDestination) {
      try {
        await unlink(destination)
      } catch {
        // Best-effort cleanup of the file this call created.
      }
    }
  }
}

/**
 * Clean up a task's output file and write queue.
 */
export async function cleanupTaskOutput(taskId: string): Promise<void> {
  const output = outputs.get(taskId)
  if (output) {
    output.cancel()
    outputs.delete(taskId)
  }

  try {
    await unlink(getTaskOutputPath(taskId))
  } catch (e) {
    const code = getErrnoCode(e)
    if (code !== 'ENOENT') {
      logError(e)
    }
  } finally {
    // ENOENT is also terminal for the artifact, so always release the pin.
    releaseTaskOutputPath(taskId)
  }
}

/**
 * Initialize output file for a new task.
 * Creates an empty file to ensure the path exists.
 */
export function initTaskOutput(taskId: string): Promise<string> {
  return track(
    (async () => {
      await ensureOutputDir(taskId)
      const outputPath = getTaskOutputPath(taskId)
      _taskOutputBackingPaths.delete(taskId)
      _taskOutputFileIdentities.delete(taskId)
      // SECURITY: O_NOFOLLOW prevents symlink-following attacks from the sandbox.
      // O_EXCL ensures we create a new file and fail if something already exists at this path.
      // On Windows, use string flags — numeric O_EXCL can produce EINVAL through libuv.
      await using fh = await open(
        outputPath,
        process.platform === 'win32'
          ? 'wx'
          : fsConstants.O_WRONLY |
              fsConstants.O_CREAT |
              fsConstants.O_EXCL |
              O_NOFOLLOW,
      )
      const stats = await fh.stat({ bigint: true })
      bindTaskOutputFileIdentity(taskId, {
        dev: stats.dev,
        ino: stats.ino,
      })
      return outputPath
    })(),
  )
}

/**
 * Publish a task alias for an agent transcript without materializing the
 * transcript early. Internal readers use the host-selected backing path and a
 * stable identity; the alias is only a user-facing convenience.
 */
export function initTaskOutputAsSymlink(
  taskId: string,
  targetPath: string,
): Promise<string> {
  return track(
    (async () => {
      await ensureOutputDir(taskId)
      const outputPath = getTaskOutputPath(taskId)

      // Do not O_CREAT the transcript here. Session persistence may be disabled,
      // and normal persistence intentionally materializes lazily on the first
      // user/assistant message.
      _taskOutputBackingPaths.set(taskId, targetPath)
      _taskOutputFileIdentities.delete(taskId)
      try {
        await using target = await open(
          targetPath,
          process.platform === 'win32'
            ? 'r'
            : fsConstants.O_RDONLY | fsConstants.O_NONBLOCK | O_NOFOLLOW,
        )
        const targetStats = await target.stat({ bigint: true })
        if (!targetStats.isFile()) {
          throw staleTaskOutputError(
            taskId,
            'the host-owned transcript target is not a regular file',
          )
        }
        bindTaskOutputFileIdentity(taskId, {
          dev: targetStats.dev,
          ino: targetStats.ino,
        })
      } catch (error) {
        if (getErrnoCode(error) !== 'ENOENT') {
          logError(error)
        }
      }

      try {
        await unlink(outputPath)
      } catch (error) {
        if (getErrnoCode(error) !== 'ENOENT') logError(error)
      }

      try {
        if (process.platform !== 'win32') {
          // Unix symlinks may intentionally be dangling until first write.
          await symlink(targetPath, outputPath)
        }
      } catch (error) {
        logError(error)
      }

      return outputPath
    })(),
  )
}
