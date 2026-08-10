import { chmodSync, writeFileSync as fsWriteFileSync } from 'fs'
import { realpath, stat } from 'fs/promises'
import { homedir } from 'os'
import {
  basename,
  dirname,
  extname,
  isAbsolute,
  join,
  normalize,
  relative,
  resolve,
  sep,
} from 'path'
import { logEvent } from 'src/services/analytics/index.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../services/analytics/growthbook.js'
import { getCwd } from '../utils/cwd.js'
import { logForDebugging } from './debug.js'
import { isENOENT, isFsInaccessible } from './errors.js'
import {
  detectEncodingForResolvedPath,
  detectLineEndingsForString,
  type LineEndingType,
} from './fileRead.js'
import { fileReadCache } from './fileReadCache.js'
import { getFsImplementation, safeResolvePath } from './fsOperations.js'
import { logError } from './log.js'
import { expandPath } from './path.js'
import { getPlatform } from './platform.js'

export type File = {
  filename: string
  content: string
}

/**
 * Check if a path exists asynchronously.
 */
export async function pathExists(path: string): Promise<boolean> {
  try {
    await stat(path)
    return true
  } catch {
    return false
  }
}

export const MAX_OUTPUT_SIZE = 0.25 * 1024 * 1024 // 0.25MB in bytes

export function readFileSafe(filepath: string): string | null {
  try {
    const fs = getFsImplementation()
    return fs.readFileSync(filepath, { encoding: 'utf8' })
  } catch (error) {
    logError(error)
    return null
  }
}

/**
 * Get the normalized modification time of a file in milliseconds.
 * Uses Math.floor to ensure consistent timestamp comparisons across file operations,
 * reducing false positives from sub-millisecond precision changes (e.g., from IDE
 * file watchers that touch files without changing content).
 */
export function getFileModificationTime(filePath: string): number {
  const fs = getFsImplementation()
  return Math.floor(fs.statSync(filePath).mtimeMs)
}

/**
 * Async variant of getFileModificationTime. Same floor semantics.
 * Use this in async paths (getChangedFiles runs every turn on every readFileState
 * entry — sync statSync there triggers the slow-operation indicator on network/
 * slow disks).
 */
export async function getFileModificationTimeAsync(
  filePath: string,
): Promise<number> {
  const s = await getFsImplementation().stat(filePath)
  return Math.floor(s.mtimeMs)
}

export function writeTextContent(
  filePath: string,
  content: string,
  encoding: BufferEncoding,
  endings: LineEndingType,
): void {
  let toWrite = content
  if (endings === 'CRLF') {
    // Normalize any existing CRLF to LF first so a new_string that already
    // contains \r\n (raw model output) doesn't become \r\r\n after the join.
    toWrite = content.replaceAll('\r\n', '\n').split('\n').join('\r\n')
  }

  writeFileSyncAndFlush_DEPRECATED(filePath, toWrite, { encoding })

}

export function detectFileEncoding(filePath: string): BufferEncoding {
  try {
    const fs = getFsImplementation()
    const { resolvedPath } = safeResolvePath(fs, filePath)
    return detectEncodingForResolvedPath(resolvedPath)
  } catch (error) {
    if (isFsInaccessible(error)) {
      logForDebugging(
        `detectFileEncoding failed for expected reason: ${error.code}`,
        {
          level: 'debug',
        },
      )
    } else {
      logError(error)
    }
    return 'utf8'
  }
}

export function detectLineEndings(
  filePath: string,
  encoding: BufferEncoding = 'utf8',
): LineEndingType {
  try {
    const fs = getFsImplementation()
    const { resolvedPath } = safeResolvePath(fs, filePath)
    const { buffer, bytesRead } = fs.readSync(resolvedPath, { length: 4096 })

    const content = buffer.toString(encoding, 0, bytesRead)
    return detectLineEndingsForString(content)
  } catch (error) {
    logError(error)
    return 'LF'
  }
}

export function convertLeadingTabsToSpaces(content: string): string {
  // The /gm regex scans every line even on no-match; skip it entirely
  // for the common tab-free case.
  if (!content.includes('\t')) return content
  return content.replace(/^\t+/gm, _ => '  '.repeat(_.length))
}

export function getAbsoluteAndRelativePaths(path: string | undefined): {
  absolutePath: string | undefined
  relativePath: string | undefined
} {
  const absolutePath = path ? expandPath(path) : undefined
  const relativePath = absolutePath
    ? relative(getCwd(), absolutePath)
    : undefined
  return { absolutePath, relativePath }
}

export function getDisplayPath(filePath: string): string {
  // Use relative path if file is in the current working directory
  const { relativePath } = getAbsoluteAndRelativePaths(filePath)
  if (relativePath && !relativePath.startsWith('..')) {
    return relativePath
  }

  // Use tilde notation for files in home directory
  const homeDir = homedir()
  if (filePath.startsWith(homeDir + sep)) {
    return '~' + filePath.slice(homeDir.length)
  }

  // Otherwise return the absolute path
  return filePath
}

/**
 * Find files with the same name but different extensions in the same directory
 * @param filePath The path to the file that doesn't exist
 * @returns The found file with a different extension, or undefined if none found
 */

export function findSimilarFile(filePath: string): string | undefined {
  const fs = getFsImplementation()
  try {
    const dir = dirname(filePath)
    const fileBaseName = basename(filePath, extname(filePath))

    // Get all files in the directory
    const files = fs.readdirSync(dir)

    // Find files with the same base name but different extension
    const similarFiles = files.filter(
      file =>
        basename(file.name, extname(file.name)) === fileBaseName &&
        join(dir, file.name) !== filePath,
    )

    // Return just the filename of the first match if found
    const firstMatch = similarFiles[0]
    if (firstMatch) {
      return firstMatch.name
    }
    return undefined
  } catch (error) {
    // Missing dir (ENOENT) is expected; for other errors log and return undefined
    if (!isENOENT(error)) {
      logError(error)
    }
    return undefined
  }
}

/**
 * Marker included in file-not-found error messages that contain a cwd note.
 * UI renderers check for this to show a short "File not found" message.
 */
export const FILE_NOT_FOUND_CWD_NOTE = 'Note: your current working directory is'

/**
 * Suggests a corrected path under the current working directory when a file/directory
 * is not found. Detects the "dropped repo folder" pattern where the model constructs
 * an absolute path missing the repo directory component.
 *
 * Example:
 *   cwd = /Users/zeeg/src/currentRepo
 *   requestedPath = /Users/zeeg/src/foobar           (doesn't exist)
 *   returns        /Users/zeeg/src/currentRepo/foobar (if it exists)
 *
 * @param requestedPath - The absolute path that was not found
 * @returns The corrected path if found under cwd, undefined otherwise
 */
export async function suggestPathUnderCwd(
  requestedPath: string,
): Promise<string | undefined> {
  const cwd = getCwd()
  const cwdParent = dirname(cwd)

  // Resolve symlinks in the requested path's parent directory (e.g., /tmp -> /private/tmp on macOS)
  // so the prefix comparison works correctly against the cwd (which is already realpath-resolved).
  let resolvedPath = requestedPath
  try {
    const resolvedDir = await realpath(dirname(requestedPath))
    resolvedPath = join(resolvedDir, basename(requestedPath))
  } catch {
    // Parent directory doesn't exist, use the original path
  }

  // Only check if the requested path is under cwd's parent but not under cwd itself.
  // When cwdParent is the root directory (e.g., '/'), use it directly as the prefix
  // to avoid a double-separator '//' that would never match.
  const cwdParentPrefix = cwdParent === sep ? sep : cwdParent + sep
  if (
    !resolvedPath.startsWith(cwdParentPrefix) ||
    resolvedPath.startsWith(cwd + sep) ||
    resolvedPath === cwd
  ) {
    return undefined
  }

  // Get the relative path from the parent directory
  const relFromParent = relative(cwdParent, resolvedPath)

  // Check if the same relative path exists under cwd
  const correctedPath = join(cwd, relFromParent)
  try {
    await stat(correctedPath)
    return correctedPath
  } catch {
    return undefined
  }
}

/**
 * Whether to use the compact line-number prefix format (`N\t` instead of
 * `     N→`). The padded-arrow format costs 9 bytes/line overhead; at
 * 1.35B Read calls × 132 lines avg this is 2.18% of fleet uncached input
 * (bq-queries/read_line_prefix_overhead_verify.sql).
 *
 * Ant soak validated no Edit error regression (6.29% vs 6.86% baseline).
 * Killswitch pattern: GB can disable if issues surface externally.
 */
export function isCompactLinePrefixEnabled(): boolean {
  // 3P default: killswitch off = compact format enabled. Client-side only —
  // no server support needed, safe for Bedrock/Vertex/Foundry.
  return !getFeatureValue_CACHED_MAY_BE_STALE(
    'tengu_compact_line_prefix_killswitch',
    false,
  )
}

/**
 * Adds cat -n style line numbers to the content.
 */
export function addLineNumbers({
  content,
  // 1-indexed
  startLine,
}: {
  content: string
  startLine: number
}): string {
  if (!content) {
    return ''
  }

  const lines = content.split(/\r?\n/)

  if (isCompactLinePrefixEnabled()) {
    return lines
      .map((line, index) => `${index + startLine}\t${line}`)
      .join('\n')
  }

  return lines
    .map((line, index) => {
      const numStr = String(index + startLine)
      if (numStr.length >= 6) {
        return `${numStr}→${line}`
      }
      return `${numStr.padStart(6, ' ')}→${line}`
    })
    .join('\n')
}

/**
 * Inverse of addLineNumbers — strips the `N→` or `N\t` prefix from a single
 * line. Co-located so format changes here and in addLineNumbers stay in sync.
 */
export function stripLineNumberPrefix(line: string): string {
  const match = line.match(/^\s*\d+[\u2192\t](.*)$/)
  return match?.[1] ?? line
}

/**
 * Checks if a directory is empty.
 * @param dirPath The path to the directory to check
 * @returns true if the directory is empty or does not exist, false otherwise
 */
export function isDirEmpty(dirPath: string): boolean {
  try {
    return getFsImplementation().isDirEmptySync(dirPath)
  } catch (e) {
    // ENOENT: directory doesn't exist, consider it empty
    // Other errors (EPERM on macOS protected folders, etc.): assume not empty
    return isENOENT(e)
  }
}

/**
 * Reads a file with caching to avoid redundant I/O operations.
 * This is the preferred method for FileEditTool operations.
 */
export function readFileSyncCached(filePath: string): string {
  const { content } = fileReadCache.readFile(filePath)
  return content
}

/**
 * Errno codes for which a rename is worth attempting again.
 *
 * Windows does not block a rename against a path another process holds a
 * handle on — it fails it, with EPERM (sometimes EBUSY or EACCES). Both
 * primitives below commit by renaming a temp file we finished writing
 * microseconds earlier onto a live target, so anything that opportunistically
 * opens that target (an editor, a real-time scanner, an indexer, a backup
 * agent) turns a perfectly good write into a hard failure — on a race that
 * clears itself unaided.
 *
 * Deliberately absent: ENOENT (the staged temp is gone; retrying cannot bring
 * it back), EXDEV (temp and target are siblings by construction, so this means
 * the target moved and retrying will not fix it), and the ENOTEMPTY / EEXIST
 * family (a directory occupies the target). None of those change on a retry —
 * retrying them would only delay an honest failure.
 *
 * This set is intentionally narrow: only short-lived lock/contention errors
 * are retried. Contract tests pin both the retryable and terminal errno sets.
 */
export const TRANSIENT_RENAME_CODES: ReadonlySet<string> = new Set([
  'EPERM',
  'EBUSY',
  'EACCES',
])

/**
 * Backoff before each successive retry, in ms. Six retries, ~630ms worst case:
 * sized for a scanner that holds a handle for tens of milliseconds, not for a
 * target something long-lived has genuinely locked.
 *
 * This blocks the event loop, which is affordable only because it is reached
 * exclusively from an already-failed rename — the success path never sleeps.
 * `settings.ts::saveSettings` already accepts a longer (~2.75s) synchronous
 * wait while acquiring its `ELOCKED` lock, so this is the more conservative of
 * the two synchronous waits on the settings write path.
 *
 * Accepted cost: on POSIX these three codes usually mean a genuine permission
 * problem rather than a passing lock, so such a failure is now reported ~630ms
 * later than it used to be. EBUSY on a network or mounted filesystem can still
 * be transient, so a delayed honest failure is the safer tradeoff.
 *
 * Before lengthening this: some callers run inside a cross-process lock
 * (`installedPluginsManager` holds one via `withCrossProcessResourceLockSync`),
 * so the ladder extends a critical section whose holder is expected to keep it
 * bounded. Two budgets constrain it. Stale takeover is not one of them —
 * `LOCK_STALE_MS` is 120s, and even a fully blocked event loop delays the 10s
 * lock refresh by under a second. The binding one is the *waiter*: eight
 * attempts of `min(25·2ⁿ, 200)`ms ≈ 975ms, so a ladder much longer than this
 * would start converting "the holder retried and succeeded" into "a concurrent
 * writer failed to acquire". That trade is currently favourable — the holder
 * would otherwise lose a credential or settings write outright, while the
 * waiter gets a loud, retryable acquisition error — but it does not survive an
 * arbitrarily long ladder.
 */
export const RENAME_RETRY_BACKOFF_MS: readonly number[] = [
  10, 20, 40, 80, 160, 320,
]

/**
 * `renameSync` that rides out the transient locks described above.
 *
 * `renameImpl` is required rather than defaulted: both callers below reach the
 * filesystem through their own indirection (`getFsImplementation()` /
 * `ops.renameSync`), and a default here would quietly invite a third caller to
 * bypass that layer. `sleep` is injectable because the failure this exists for
 * cannot be provoked on demand — the OS produces it or it does not — so tests
 * drive this exact retry loop rather than a re-implementation of it.
 */
export function renameSyncWithTransientRetry(
  from: string,
  to: string,
  renameImpl: (from: string, to: string) => void,
  sleep: (ms: number) => void = ms => Bun.sleepSync(ms),
): void {
  const backoffs = [...RENAME_RETRY_BACKOFF_MS]
  for (;;) {
    try {
      renameImpl(from, to)
      return
    } catch (err) {
      const code = (err as NodeJS.ErrnoException | null)?.code
      // `shift()` running dry is how "retries exhausted" is spelled.
      const backoffMs = backoffs.shift()
      if (
        backoffMs === undefined ||
        code === undefined ||
        !TRANSIENT_RENAME_CODES.has(code)
      ) {
        throw err
      }
      sleep(backoffMs)
    }
  }
}

/**
 * Writes to a file and flushes the file to disk
 * @param filePath The path to the file to write to
 * @param content The content to write to the file
 * @param options Options for writing the file, including encoding and mode
 * @deprecated Use `fs.promises.writeFile` with flush option instead for non-blocking writes.
 * Sync file writes block the event loop and cause performance issues.
 */
export function writeFileSyncAndFlush_DEPRECATED(
  filePath: string,
  content: string,
  options: { encoding: BufferEncoding; mode?: number } = { encoding: 'utf-8' },
): void {
  const fs = getFsImplementation()

  // Check if the target file is a symlink to preserve it for all users
  // Note: We don't use safeResolvePath here because we need to manually handle
  // symlinks to ensure we write to the target while preserving the symlink itself
  let targetPath = filePath
  try {
    // Try to read the symlink - if successful, it's a symlink
    const linkTarget = fs.readlinkSync(filePath)
    // Resolve to absolute path
    targetPath = isAbsolute(linkTarget)
      ? linkTarget
      : resolve(dirname(filePath), linkTarget)
    logForDebugging(`Writing through symlink: ${filePath} -> ${targetPath}`)
  } catch {
    // ENOENT (doesn't exist) or EINVAL (not a symlink) — keep targetPath = filePath
  }

  // Try atomic write first
  const tempPath = `${targetPath}.tmp.${process.pid}.${Date.now()}`

  // Check if target file exists and get its permissions (single stat, reused in both atomic and fallback paths)
  let targetMode: number | undefined
  let targetExists = false
  try {
    targetMode = fs.statSync(targetPath).mode
    targetExists = true
    logForDebugging(`Preserving file permissions: ${targetMode.toString(8)}`)
  } catch (e) {
    if (!isENOENT(e)) throw e
    if (options.mode !== undefined) {
      // Use provided mode for new files
      targetMode = options.mode
      logForDebugging(
        `Setting permissions for new file: ${targetMode.toString(8)}`,
      )
    }
  }

  try {
    logForDebugging(`Writing to temp file: ${tempPath}`)

    // Write to temp file with flush and mode (if specified for new file)
    const writeOptions: {
      encoding: BufferEncoding
      flush: boolean
      mode?: number
    } = {
      encoding: options.encoding,
      flush: true,
    }
    // Only set mode in writeFileSync for new files to ensure atomic permission setting
    if (!targetExists && options.mode !== undefined) {
      writeOptions.mode = options.mode
    }

    fsWriteFileSync(tempPath, content, writeOptions)
    logForDebugging(
      `Temp file written successfully, size: ${content.length} bytes`,
    )

    // For existing files or if mode was not set atomically, apply permissions
    if (targetExists && targetMode !== undefined) {
      chmodSync(tempPath, targetMode)
      logForDebugging(`Applied original permissions to temp file`)
    }

    // Atomic rename (on POSIX systems, this is atomic)
    // On Windows, this will overwrite the destination if it exists — or fail
    // outright while another process holds the target open, which is the race
    // `renameSyncWithTransientRetry` rides out. Only once the retry ladder is
    // exhausted does the non-atomic fallback below come into play, so a
    // transient lock no longer costs this write its atomicity.
    logForDebugging(`Renaming ${tempPath} to ${targetPath}`)
    renameSyncWithTransientRetry(tempPath, targetPath, (from, to) =>
      fs.renameSync(from, to),
    )
    logForDebugging(`File ${targetPath} written atomically`)
  } catch (atomicError) {
    logForDebugging(`Failed to write file atomically: ${atomicError}`, {
      level: 'error',
    })
    logEvent('tengu_atomic_write_error', {})

    // Clean up temp file on error
    try {
      logForDebugging(`Cleaning up temp file: ${tempPath}`)
      fs.unlinkSync(tempPath)
    } catch (cleanupError) {
      logForDebugging(`Failed to clean up temp file: ${cleanupError}`)
    }

    // Fallback to non-atomic write
    logForDebugging(`Falling back to non-atomic write for ${targetPath}`)
    try {
      const fallbackOptions: {
        encoding: BufferEncoding
        flush: boolean
        mode?: number
      } = {
        encoding: options.encoding,
        flush: true,
      }
      // Only set mode for new files
      if (!targetExists && options.mode !== undefined) {
        fallbackOptions.mode = options.mode
      }

      fsWriteFileSync(targetPath, content, fallbackOptions)
      logForDebugging(
        `File ${targetPath} written successfully with non-atomic fallback`,
      )
    } catch (fallbackError) {
      logForDebugging(`Non-atomic write also failed: ${fallbackError}`)
      throw fallbackError
    }
  }
}

export interface AtomicReplaceFileSyncOperations {
  readlinkSync(path: string): string
  statSync(path: string): { mode: number }
  writeFileSync(
    path: string,
    content: string,
    options: { encoding: BufferEncoding; flush: boolean; mode?: number },
  ): void
  chmodSync(path: string, mode: number): void
  renameSync(oldPath: string, newPath: string): void
  unlinkSync(path: string): void
  /**
   * Synchronous backoff seam for the rename retry ladder. Production callers
   * omit it and get `Bun.sleepSync`; tests supply a recorder so the ladder is
   * asserted without actually waiting out ~630ms.
   */
  sleepSync?(ms: number): void
}

let atomicReplaceSequence = 0

/**
 * Atomically replace a small state file without ever degrading to an in-place
 * overwrite. Credential/settings publishers use this stricter primitive: if
 * staging or rename fails, the previous target remains the only committed
 * generation and the original error is surfaced to the caller.
 *
 * The optional operations argument is a fault-injection seam. Production
 * callers omit it and use the normal CrabCode filesystem implementation.
 */
export function writeFileSyncAtomicNoFallback(
  filePath: string,
  content: string,
  options: { encoding: BufferEncoding; mode?: number } = { encoding: 'utf-8' },
  operations?: AtomicReplaceFileSyncOperations,
): void {
  const fs = getFsImplementation()
  const ops: AtomicReplaceFileSyncOperations = operations ?? {
    readlinkSync: path => fs.readlinkSync(path),
    statSync: path => fs.statSync(path),
    writeFileSync: (path, value, writeOptions) =>
      fsWriteFileSync(path, value, writeOptions),
    chmodSync,
    renameSync: (oldPath, newPath) => fs.renameSync(oldPath, newPath),
    unlinkSync: path => fs.unlinkSync(path),
  }

  let targetPath = filePath
  try {
    const linkTarget = ops.readlinkSync(filePath)
    targetPath = isAbsolute(linkTarget)
      ? linkTarget
      : resolve(dirname(filePath), linkTarget)
  } catch {
    // ENOENT (new target) and EINVAL (ordinary file) both use filePath.
  }

  let targetMode: number | undefined
  let targetExists = false
  try {
    targetMode = ops.statSync(targetPath).mode
    targetExists = true
  } catch (error) {
    if (!isENOENT(error)) throw error
    targetMode = options.mode
  }

  atomicReplaceSequence += 1
  const tempPath = `${targetPath}.tmp.${process.pid}.${Date.now()}.${atomicReplaceSequence}`
  try {
    ops.writeFileSync(tempPath, content, {
      encoding: options.encoding,
      flush: true,
      ...(!targetExists && options.mode !== undefined
        ? { mode: options.mode }
        : {}),
    })
    if (targetExists && (options.mode ?? targetMode) !== undefined) {
      ops.chmodSync(tempPath, (options.mode ?? targetMode)!)
    }
    // rename is the commit point. Do not perform any throwing durability step
    // afterwards: callers must never receive an ambiguous post-commit error.
    //
    // Retrying here does not weaken that guarantee: every attempt either
    // commits or leaves the previous generation untouched, so the ladder only
    // changes how long we wait before declaring the commit impossible. Without
    // it a transient Windows handle lock costs a credential or settings write
    // outright — this primitive has no fallback by design.
    renameSyncWithTransientRetry(
      tempPath,
      targetPath,
      (from, to) => ops.renameSync(from, to),
      ops.sleepSync?.bind(ops),
    )
  } catch (error) {
    try {
      ops.unlinkSync(tempPath)
    } catch {
      // Best-effort removal of an unpublished generation.
    }
    throw error
  }
}

export function getDesktopPath(): string {
  const platform = getPlatform()
  const homeDir = homedir()

  if (platform === 'macos') {
    return join(homeDir, 'Desktop')
  }

  if (platform === 'windows') {
    // For WSL, try to access Windows desktop
    const windowsHome = process.env.USERPROFILE
      ? process.env.USERPROFILE.replace(/\\/g, '/')
      : null

    if (windowsHome) {
      const wslPath = windowsHome.replace(/^[A-Z]:/, '')
      const desktopPath = `/mnt/c${wslPath}/Desktop`

      if (getFsImplementation().existsSync(desktopPath)) {
        return desktopPath
      }
    }

    // Fallback: try to find desktop in typical Windows user location
    try {
      const usersDir = '/mnt/c/Users'
      const userDirs = getFsImplementation().readdirSync(usersDir)

      for (const user of userDirs) {
        if (
          user.name === 'Public' ||
          user.name === 'Default' ||
          user.name === 'Default User' ||
          user.name === 'All Users'
        ) {
          continue
        }

        const potentialDesktopPath = join(usersDir, user.name, 'Desktop')

        if (getFsImplementation().existsSync(potentialDesktopPath)) {
          return potentialDesktopPath
        }
      }
    } catch (error) {
      logError(error)
    }
  }

  // Linux/unknown platform fallback
  const desktopPath = join(homeDir, 'Desktop')
  if (getFsImplementation().existsSync(desktopPath)) {
    return desktopPath
  }

  // If Desktop folder doesn't exist, fallback to home directory
  return homeDir
}

/**
 * Validates that a file size is within the specified limit.
 * Returns true if the file is within the limit, false otherwise.
 *
 * @param filePath The path to the file to validate
 * @param maxSizeBytes The maximum allowed file size in bytes
 * @returns true if file size is within limit, false otherwise
 */
export function isFileWithinReadSizeLimit(
  filePath: string,
  maxSizeBytes: number = MAX_OUTPUT_SIZE,
): boolean {
  try {
    const stats = getFsImplementation().statSync(filePath)
    return stats.size <= maxSizeBytes
  } catch {
    // If we can't stat the file, return false to indicate validation failure
    return false
  }
}

/**
 * Normalize a file path for comparison, handling platform differences.
 * On Windows, normalizes path separators and converts to lowercase for
 * case-insensitive comparison.
 */
export function normalizePathForComparison(filePath: string): string {
  // Use path.normalize() to clean up redundant separators and resolve . and ..
  let normalized = normalize(filePath)

  // On Windows, normalize for case-insensitive comparison:
  // - Convert forward slashes to backslashes (path.normalize only does this on actual Windows)
  // - Convert to lowercase (Windows paths are case-insensitive)
  if (getPlatform() === 'windows') {
    normalized = normalized.replace(/\//g, '\\').toLowerCase()
  }

  return normalized
}

/**
 * Compare two file paths for equality, handling Windows case-insensitivity.
 */
export function pathsEqual(path1: string, path2: string): boolean {
  return normalizePathForComparison(path1) === normalizePathForComparison(path2)
}
