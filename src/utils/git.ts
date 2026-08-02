import { createHash } from 'crypto'
import { realpathSync, statSync } from 'fs'
import { open, readFile, realpath, stat } from 'fs/promises'
import memoize from 'lodash-es/memoize.js'
import { delimiter, isAbsolute, join } from 'path'
import { hasBinaryExtension, isBinaryContent } from '../constants/files.js'
import { getCwd } from './cwd.js'
import { logForDebugging } from './debug.js'
import { logForDiagnosticsNoPII } from './diagLogs.js'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { getFsImplementation } from './fsOperations.js'
import {
  getCachedBranch,
  getCachedDefaultBranch,
  getCachedHead,
  getCachedRemoteUrl,
  getWorktreeCountFromFs,
  isShallowClone as isShallowCloneFs,
  resolveGitDir,
} from './git/gitFilesystem.js'
import { logError } from './log.js'
import {
  findCanonicalWorkspaceGitRoot,
  findWorkspaceGitRoot,
  installWorkspaceProjectIdentityDiagnostics,
} from './workspaceProjectIdentity.js'

installWorkspaceProjectIdentityDiagnostics(logForDiagnosticsNoPII)

export const findGitRoot = findWorkspaceGitRoot
export const findCanonicalGitRoot = findCanonicalWorkspaceGitRoot

type GitExecutablePlatform =
  | 'darwin'
  | 'linux'
  | 'win32'
  | 'other-unix'

export type GitExecutableResolutionInput = {
  overridePath?: string
  pathEnv?: string
  commonCandidates: readonly string[]
  windows: boolean
  pathDelimiter?: string
}

export type CommonGitExecutableCandidateInput = {
  platform: GitExecutablePlatform
  home?: string
  programFiles?: string
  localAppData?: string
}

function canonicalRegularGitExecutable(
  candidate: string,
  requireUnixExecuteBits: boolean,
): string {
  if (!isAbsolute(candidate)) {
    throw new Error('path is not absolute')
  }
  const canonical = realpathSync(candidate)
  const metadata = statSync(canonical)
  if (!isAbsolute(canonical) || !metadata.isFile()) {
    throw new Error('canonical path is not an absolute regular file')
  }
  if (requireUnixExecuteBits && (metadata.mode & 0o111) === 0) {
    throw new Error('canonical regular file has no Unix execute bit')
  }
  return canonical
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

/**
 * Resolve Git from sealed inputs using the same precedence and fail-closed
 * rules as the platform resolver: GIT_PATH, absolute PATH directories, then
 * common platform install locations. Exported so the cross-platform contract
 * can be tested without mutating process-global environment or platform state.
 */
export function resolveGitExecutableFrom({
  overridePath,
  pathEnv,
  commonCandidates,
  windows,
  pathDelimiter = delimiter,
}: GitExecutableResolutionInput): string {
  const requireUnixExecuteBits = !windows && process.platform !== 'win32'
  if (overridePath !== undefined && overridePath.length > 0) {
    if (!isAbsolute(overridePath)) {
      throw new Error(
        `GIT_PATH must be an absolute path to a regular git executable: ${overridePath}`,
      )
    }
    try {
      return canonicalRegularGitExecutable(
        overridePath,
        requireUnixExecuteBits,
      )
    } catch (error) {
      throw new Error(
        `GIT_PATH points to a missing or invalid git executable ${overridePath}: ${errorMessage(error)}`,
      )
    }
  }

  const binaryName = windows ? 'git.exe' : 'git'
  if (pathEnv !== undefined) {
    for (const directory of pathEnv.split(pathDelimiter)) {
      if (!isAbsolute(directory)) continue
      try {
        if (!statSync(directory).isDirectory()) continue
        return canonicalRegularGitExecutable(
          join(directory, binaryName),
          requireUnixExecuteBits,
        )
      } catch {
        // Keep scanning PATH just like the platform resolver.
      }
    }
  }

  for (const candidate of commonCandidates) {
    if (!isAbsolute(candidate)) continue
    try {
      return canonicalRegularGitExecutable(candidate, requireUnixExecuteBits)
    } catch {
      // Keep scanning common platform candidates.
    }
  }

  throw new Error(
    'git CLI not found via GIT_PATH, PATH, or common platform install paths',
  )
}

/** Return Git's common desktop install locations in platform priority order. */
export function commonGitExecutableCandidatesFor({
  platform,
  home,
  programFiles,
  localAppData,
}: CommonGitExecutableCandidateInput): string[] {
  if (platform === 'darwin') {
    return [
      '/opt/homebrew/bin/git',
      '/usr/local/bin/git',
      '/usr/bin/git',
      ...(home ? [join(home, '.local/bin/git')] : []),
    ]
  }

  if (platform === 'win32') {
    return [
      ...(programFiles
        ? [
            join(programFiles, 'Git/cmd/git.exe'),
            join(programFiles, 'Git/bin/git.exe'),
          ]
        : []),
      ...(localAppData
        ? [join(localAppData, 'Programs/Git/cmd/git.exe')]
        : []),
    ]
  }

  return [
    '/usr/local/bin/git',
    '/usr/bin/git',
    ...(home ? [join(home, '.local/bin/git')] : []),
  ]
}

function currentGitExecutablePlatform(): GitExecutablePlatform {
  if (process.platform === 'darwin') return 'darwin'
  if (process.platform === 'linux') return 'linux'
  if (process.platform === 'win32') return 'win32'
  return 'other-unix'
}

export function resolveGitExecutable(): string {
  const platform = currentGitExecutablePlatform()
  const home = process.env.HOME ?? process.env.USERPROFILE
  return resolveGitExecutableFrom({
    overridePath: process.env.GIT_PATH,
    pathEnv: process.env.PATH,
    commonCandidates: commonGitExecutableCandidatesFor({
      platform,
      home,
      programFiles: process.env.PROGRAMFILES,
      localAppData: process.env.LOCALAPPDATA,
    }),
    windows: platform === 'win32',
  })
}

export const gitExe = memoize((): string => {
  // Every time we spawn a process, we have to lookup and canonicalize the path.
  // Cache the sealed absolute executable identity for the process lifetime.
  return resolveGitExecutable()
})

// W-PROMPT-ENV-ISGIT-FINGERPRINT (2026-05-23, P1-12): cwd-keyed memoize.
//
// Previously `memoize(async () => ...)` with no resolver — lodash defaults
// the cache key to the first argument, so the zero-arg overload always
// resolves to the SAME `undefined` cache key. The first call for any cwd
// fixed `isGit` for the entire process lifetime; switching project (`cd`
// into a non-git dir, or out of one into a repo) kept reporting the old
// boolean in the env_info_simple system-prompt section.
//
// Resolver = current cwd. lodash memoize takes (value, ...args) and the
// resolver is invoked with the same args as the function; here zero args
// means the resolver receives nothing, so we read `getCwd()` ourselves to
// build the cache key. This keeps all existing zero-arg callers
// backward-compatible while ensuring per-cwd cache buckets.
//
// Known limitation: `findGitRoot` itself is LRU-memoized by path, so if a
// user runs `git init` (or `rm -rf .git`) on a cwd we have already
// observed, both `findGitRoot` and this cache return the stale value.
// Fixing that requires invalidating `findGitRoot`'s LRU on filesystem
// .git transitions (out of scope for this PR — see audit report §「关联
// 方影响审查」).
export const getIsGit = memoize(
  async (): Promise<boolean> => {
    const startTime = Date.now()
    logForDiagnosticsNoPII('info', 'is_git_check_started')

    const cwd = getCwd()
    const isGit = findGitRoot(cwd) !== null

    logForDiagnosticsNoPII('info', 'is_git_check_completed', {
      duration_ms: Date.now() - startTime,
      is_git: isGit,
    })
    return isGit
  },
  // Resolver: cache bucket per cwd. Reads `getCwd()` at call time so the
  // memoize key tracks `setCwdState` updates from Shell.ts /
  // spawnManagedShellCommand.ts and EnterWorktree/ExitWorktree tools.
  () => getCwd(),
)

export function getGitDir(cwd: string): Promise<string | null> {
  return resolveGitDir(cwd)
}

export async function isAtGitRoot(): Promise<boolean> {
  const cwd = getCwd()
  const gitRoot = findGitRoot(cwd)
  if (!gitRoot) {
    return false
  }
  // Resolve symlinks for accurate comparison
  try {
    const [resolvedCwd, resolvedGitRoot] = await Promise.all([
      realpath(cwd),
      realpath(gitRoot),
    ])
    return resolvedCwd === resolvedGitRoot
  } catch {
    return cwd === gitRoot
  }
}

export const dirIsInGitRepo = async (cwd: string): Promise<boolean> => {
  return findGitRoot(cwd) !== null
}

export const getHead = async (): Promise<string> => {
  return getCachedHead()
}

export const getBranch = async (): Promise<string> => {
  return getCachedBranch()
}

export const getDefaultBranch = async (): Promise<string> => {
  return getCachedDefaultBranch()
}

export const getRemoteUrl = async (): Promise<string | null> => {
  return getCachedRemoteUrl()
}

/**
 * Normalizes a git remote URL to a canonical form for hashing.
 * Converts SSH and HTTPS URLs to the same format: host/owner/repo (lowercase, no .git)
 *
 * Examples:
 * - git@github.com:owner/repo.git -> github.com/owner/repo
 * - https://github.com/owner/repo.git -> github.com/owner/repo
 * - ssh://git@github.com/owner/repo -> github.com/owner/repo
 * - http://local_proxy@127.0.0.1:16583/git/owner/repo -> github.com/owner/repo
 */
export function normalizeGitRemoteUrl(url: string): string | null {
  const trimmed = url.trim()
  if (!trimmed) return null

  // Handle SSH format: git@host:owner/repo.git
  const sshMatch = trimmed.match(/^git@([^:]+):(.+?)(?:\.git)?$/)
  if (sshMatch && sshMatch[1] && sshMatch[2]) {
    return `${sshMatch[1]}/${sshMatch[2]}`.toLowerCase()
  }

  // Handle HTTPS/SSH URL format: https://host/owner/repo.git or ssh://git@host/owner/repo
  const urlMatch = trimmed.match(
    /^(?:https?|ssh):\/\/(?:[^@]+@)?([^/]+)\/(.+?)(?:\.git)?$/,
  )
  if (urlMatch && urlMatch[1] && urlMatch[2]) {
    const host = urlMatch[1]
    const path = urlMatch[2]

    // CCR git proxy URLs use format:
    //   Legacy:  http://...@127.0.0.1:PORT/git/owner/repo       (github.com assumed)
    //   GHE:     http://...@127.0.0.1:PORT/git/ghe.host/owner/repo (host encoded in path)
    // Strip the /git/ prefix. If the first segment contains a dot, it's a
    // hostname (GitHub org names cannot contain dots). Otherwise assume github.com.
    if (isLocalHost(host) && path.startsWith('git/')) {
      const proxyPath = path.slice(4) // Remove "git/" prefix
      const segments = proxyPath.split('/')
      // 3+ segments where first contains a dot → host/owner/repo (GHE format)
      if (segments.length >= 3 && segments[0]!.includes('.')) {
        return proxyPath.toLowerCase()
      }
      // 2 segments → owner/repo (legacy format, assume github.com)
      return `github.com/${proxyPath}`.toLowerCase()
    }

    return `${host}/${path}`.toLowerCase()
  }

  return null
}

/**
 * Returns a SHA256 hash (first 16 chars) of the normalized git remote URL.
 * This provides a globally unique identifier for the repository that:
 * - Is the same regardless of SSH vs HTTPS clone
 * - Does not expose the actual repository name in logs
 */
export async function getRepoRemoteHash(): Promise<string | null> {
  const remoteUrl = await getRemoteUrl()
  if (!remoteUrl) return null

  const normalized = normalizeGitRemoteUrl(remoteUrl)
  if (!normalized) return null

  const hash = createHash('sha256').update(normalized).digest('hex')
  return hash.substring(0, 16)
}

export const getIsHeadOnRemote = async (): Promise<boolean> => {
  const { code } = await localExecBridge.execGitCommand({
    args: ['rev-parse', '@{u}'],
    preserveOutputOnError: false,
  })
  return code === 0
}

export const hasUnpushedCommits = async (): Promise<boolean> => {
  const { stdout, code } = await localExecBridge.execGitCommand({
    args: ['rev-list', '--count', '@{u}..HEAD'],
    preserveOutputOnError: false,
  })
  return code === 0 && parseInt(stdout.trim(), 10) > 0
}

export const getIsClean = async (options?: {
  ignoreUntracked?: boolean
}): Promise<boolean> => {
  const args = ['--no-optional-locks', 'status', '--porcelain']
  if (options?.ignoreUntracked) {
    args.push('-uno')
  }
  const { stdout } = await localExecBridge.execGitCommand({
    args,
    preserveOutputOnError: false,
  })
  return stdout.trim().length === 0
}

export const getChangedFiles = async (): Promise<string[]> => {
  const { stdout } = await localExecBridge.execGitCommand({
    args: ['--no-optional-locks', 'status', '--porcelain'],
    preserveOutputOnError: false,
  })
  return stdout
    .trim()
    .split('\n')
    .map(line => line.trim().split(' ', 2)[1]?.trim()) // Remove status prefix (e.g., "M ", "A ", "??")
    .filter(line => typeof line === 'string') // Remove empty entries
}

export type GitFileStatus = {
  tracked: string[]
  untracked: string[]
}

export const getFileStatus = async (): Promise<GitFileStatus> => {
  const { stdout } = await localExecBridge.execGitCommand({
    args: ['--no-optional-locks', 'status', '--porcelain'],
    preserveOutputOnError: false,
  })

  const tracked: string[] = []
  const untracked: string[] = []

  stdout
    .trim()
    .split('\n')
    .filter(line => line.length > 0)
    .forEach(line => {
      const status = line.substring(0, 2)
      const filename = line.substring(2).trim()

      if (status === '??') {
        untracked.push(filename)
      } else if (filename) {
        tracked.push(filename)
      }
    })

  return { tracked, untracked }
}

export const getWorktreeCount = async (): Promise<number> => {
  return getWorktreeCountFromFs()
}

/**
 * Stashes all changes (including untracked files) to return git to a clean porcelain state
 * Important: This function stages untracked files before stashing to prevent data loss
 * @param message - Optional custom message for the stash
 * @returns Promise<boolean> - true if stash was successful, false otherwise
 */
export const stashToCleanState = async (message?: string): Promise<boolean> => {
  try {
    const stashMessage =
      message || `CrabCode auto-stash - ${new Date().toISOString()}`

    // First, check if we have untracked files
    const { untracked } = await getFileStatus()

    // If we have untracked files, add them to the index first
    // This prevents them from being deleted
    if (untracked.length > 0) {
      const { code: addCode } = await localExecBridge.execGitCommand({
        args: ['add', ...untracked],
        preserveOutputOnError: false,
      })

      if (addCode !== 0) {
        return false
      }
    }

    // Now stash everything (staged and unstaged changes)
    const { code } = await localExecBridge.execGitCommand({
      args: ['stash', 'push', '--message', stashMessage],
      preserveOutputOnError: false,
    })
    return code === 0
  } catch (_) {
    return false
  }
}

export type GitRepoState = {
  commitHash: string
  branchName: string
  remoteUrl: string | null
  isHeadOnRemote: boolean
  isClean: boolean
  worktreeCount: number
}

export async function getGitState(): Promise<GitRepoState | null> {
  try {
    const [
      commitHash,
      branchName,
      remoteUrl,
      isHeadOnRemote,
      isClean,
      worktreeCount,
    ] = await Promise.all([
      getHead(),
      getBranch(),
      getRemoteUrl(),
      getIsHeadOnRemote(),
      getIsClean(),
      getWorktreeCount(),
    ])

    return {
      commitHash,
      branchName,
      remoteUrl,
      isHeadOnRemote,
      isClean,
      worktreeCount,
    }
  } catch (_) {
    // Fail silently - git state is best effort
    return null
  }
}

export async function getGithubRepo(): Promise<string | null> {
  const { parseGitRemote } = await import('./detectRepository.js')
  const remoteUrl = await getRemoteUrl()
  if (!remoteUrl) {
    logForDebugging('Local GitHub repo: unknown')
    return null
  }
  // Only return results for github.com — callers (e.g. issue submission)
  // assume the result is a github.com repository.
  const parsed = parseGitRemote(remoteUrl)
  if (parsed && parsed.host === 'github.com') {
    const result = `${parsed.owner}/${parsed.name}`
    logForDebugging(`Local GitHub repo: ${result}`)
    return result
  }
  logForDebugging('Local GitHub repo: unknown')
  return null
}

/**
 * Preserved git state for issue submission.
 * Uses remote base (e.g., origin/main) which is rarely force-pushed,
 * unlike local commits that can be GC'd after force push.
 */
export type PreservedGitState = {
  /** The SHA of the merge-base with the remote branch */
  remote_base_sha: string | null
  /** The remote branch used (e.g., "origin/main") */
  remote_base: string | null
  /** Patch from merge-base to current state (includes uncommitted changes) */
  patch: string
  /** Untracked files with their contents */
  untracked_files: Array<{ path: string; content: string }>
  /** git format-patch output for committed changes between merge-base and HEAD.
   *  Used to reconstruct the actual commit chain (author, date, message) in
   *  replay containers. null when there are no commits between merge-base and HEAD. */
  format_patch: string | null
  /** The current HEAD SHA (tip of the feature branch) */
  head_sha: string | null
  /** The current branch name (e.g., "feat/my-feature") */
  branch_name: string | null
}

// Size limits for untracked file capture
const MAX_FILE_SIZE_BYTES = 500 * 1024 * 1024 // 500MB per file
const MAX_TOTAL_SIZE_BYTES = 5 * 1024 * 1024 * 1024 // 5GB total
const MAX_FILE_COUNT = 20000

// Initial read buffer for binary detection + content reuse. 64KB covers
// most source files in a single read; isBinaryContent() internally scans
// only its first 8KB for the binary heuristic, so the extra bytes are
// purely for avoiding a second read when the file turns out to be text.
const SNIFF_BUFFER_SIZE = 64 * 1024

/**
 * Find the best remote branch to use as a base.
 * Priority: tracking branch > origin/main > origin/staging > origin/master
 */
export async function findRemoteBase(): Promise<string | null> {
  // First try: get the tracking branch for the current branch
  const { stdout: trackingBranch, code: trackingCode } =
    await localExecBridge.execGitCommand({
      args: ['rev-parse', '--abbrev-ref', '--symbolic-full-name', '@{u}'],
      preserveOutputOnError: false,
    })

  if (trackingCode === 0 && trackingBranch.trim()) {
    return trackingBranch.trim()
  }

  // Second try: check for common default branch names on origin
  const { stdout: remoteRefs, code: remoteCode } =
    await localExecBridge.execGitCommand({
      args: ['remote', 'show', 'origin', '--', 'HEAD'],
      preserveOutputOnError: false,
    })

  if (remoteCode === 0) {
    // Parse the default branch from remote show output
    const match = remoteRefs.match(/HEAD branch: (\S+)/)
    if (match && match[1]) {
      return `origin/${match[1]}`
    }
  }

  // Third try: check which common branches exist
  const candidates = ['origin/main', 'origin/staging', 'origin/master']
  for (const candidate of candidates) {
    const { code } = await localExecBridge.execGitCommand({
      args: ['rev-parse', '--verify', candidate],
      preserveOutputOnError: false,
    })
    if (code === 0) {
      return candidate
    }
  }

  return null
}

/**
 * Check if we're in a shallow clone by looking for <gitDir>/shallow.
 */
function isShallowClone(): Promise<boolean> {
  return isShallowCloneFs()
}

/**
 * Capture untracked files (git diff doesn't include them).
 * Respects size limits and skips binary files.
 */
async function captureUntrackedFiles(): Promise<
  Array<{ path: string; content: string }>
> {
  const { stdout, code } = await localExecBridge.execGitCommand({
    args: ['ls-files', '--others', '--exclude-standard'],
    preserveOutputOnError: false,
  })

  const trimmed = stdout.trim()
  if (code !== 0 || !trimmed) {
    return []
  }

  const files = trimmed.split('\n').filter(Boolean)
  const result: Array<{ path: string; content: string }> = []
  let totalSize = 0

  for (const filePath of files) {
    // Check file count limit
    if (result.length >= MAX_FILE_COUNT) {
      logForDebugging(
        `Untracked file capture: reached max file count (${MAX_FILE_COUNT})`,
      )
      break
    }

    // Skip binary files by extension - zero I/O
    if (hasBinaryExtension(filePath)) {
      continue
    }

    try {
      const stats = await stat(filePath)
      const fileSize = stats.size

      // Skip files exceeding per-file limit
      if (fileSize > MAX_FILE_SIZE_BYTES) {
        logForDebugging(
          `Untracked file capture: skipping ${filePath} (exceeds ${MAX_FILE_SIZE_BYTES} bytes)`,
        )
        continue
      }

      // Check total size limit
      if (totalSize + fileSize > MAX_TOTAL_SIZE_BYTES) {
        logForDebugging(
          `Untracked file capture: reached total size limit (${MAX_TOTAL_SIZE_BYTES} bytes)`,
        )
        break
      }

      // Empty file - no need to open
      if (fileSize === 0) {
        result.push({ path: filePath, content: '' })
        continue
      }

      // Binary sniff on up to SNIFF_BUFFER_SIZE bytes. Caps binary-file reads
      // at SNIFF_BUFFER_SIZE even though MAX_FILE_SIZE_BYTES allows up to 500MB.
      // If the file fits in the sniff buffer we reuse it as the content; for
      // larger text files we fall back to readFile with encoding so the runtime
      // decodes to a string without materializing a full-size Buffer in JS.
      const sniffSize = Math.min(SNIFF_BUFFER_SIZE, fileSize)
      const fd = await open(filePath, 'r')
      try {
        const sniffBuf = Buffer.alloc(sniffSize)
        const { bytesRead } = await fd.read(sniffBuf, 0, sniffSize, 0)
        const sniff = sniffBuf.subarray(0, bytesRead)

        if (isBinaryContent(sniff)) {
          continue
        }

        let content: string
        if (fileSize <= sniffSize) {
          // Sniff already covers the whole file
          content = sniff.toString('utf-8')
        } else {
          // readFile with encoding decodes to string directly, avoiding a
          // full-size Buffer living alongside the decoded string. The extra
          // open/close is cheaper than doubling peak memory for large files.
          content = await readFile(filePath, 'utf-8')
        }

        result.push({ path: filePath, content })
        totalSize += fileSize
      } finally {
        await fd.close()
      }
    } catch (err) {
      // Skip files we can't read
      logForDebugging(`Failed to read untracked file ${filePath}: ${err}`)
    }
  }

  return result
}

/**
 * Preserve git state for issue submission.
 * Uses remote base for more stable replay capability.
 *
 * Edge cases handled:
 * - Detached HEAD: falls back to merge-base with default branch directly
 * - No remote: returns null for remote fields, uses HEAD-only mode
 * - Shallow clone: falls back to HEAD-only mode
 */
export async function preserveGitStateForIssue(): Promise<PreservedGitState | null> {
  try {
    const isGit = await getIsGit()
    if (!isGit) {
      return null
    }

    // Check for shallow clone - fall back to simpler mode
    if (await isShallowClone()) {
      logForDebugging('Shallow clone detected, using HEAD-only mode for issue')
      const [{ stdout: patch }, untrackedFiles] = await Promise.all([
        localExecBridge.execGitCommand({ args: ['diff', 'HEAD'] }),
        captureUntrackedFiles(),
      ])
      return {
        remote_base_sha: null,
        remote_base: null,
        patch: patch || '',
        untracked_files: untrackedFiles,
        format_patch: null,
        head_sha: null,
        branch_name: null,
      }
    }

    // Find the best remote base
    const remoteBase = await findRemoteBase()

    if (!remoteBase) {
      // No remote found - use HEAD-only mode
      logForDebugging('No remote found, using HEAD-only mode for issue')
      const [{ stdout: patch }, untrackedFiles] = await Promise.all([
        localExecBridge.execGitCommand({ args: ['diff', 'HEAD'] }),
        captureUntrackedFiles(),
      ])
      return {
        remote_base_sha: null,
        remote_base: null,
        patch: patch || '',
        untracked_files: untrackedFiles,
        format_patch: null,
        head_sha: null,
        branch_name: null,
      }
    }

    // Get the merge-base with remote
    const { stdout: mergeBase, code: mergeBaseCode } =
      await localExecBridge.execGitCommand({
        args: ['merge-base', 'HEAD', remoteBase],
        preserveOutputOnError: false,
      })

    if (mergeBaseCode !== 0 || !mergeBase.trim()) {
      // Merge-base failed - fall back to HEAD-only
      logForDebugging('Merge-base failed, using HEAD-only mode for issue')
      const [{ stdout: patch }, untrackedFiles] = await Promise.all([
        localExecBridge.execGitCommand({ args: ['diff', 'HEAD'] }),
        captureUntrackedFiles(),
      ])
      return {
        remote_base_sha: null,
        remote_base: null,
        patch: patch || '',
        untracked_files: untrackedFiles,
        format_patch: null,
        head_sha: null,
        branch_name: null,
      }
    }

    const remoteBaseSha = mergeBase.trim()

    // All 5 commands below depend only on remoteBaseSha — run them in parallel.
    // ~5×90ms serial → ~90ms parallel on Bun native (used by /issue and /share).
    const [
      { stdout: patch },
      untrackedFiles,
      { stdout: formatPatchOut, code: formatPatchCode },
      { stdout: headSha },
      { stdout: branchName },
    ] = await Promise.all([
      // Patch from merge-base to current state (including staged changes)
      localExecBridge.execGitCommand({ args: ['diff', remoteBaseSha] }),
      // Untracked files captured separately
      captureUntrackedFiles(),
      // format-patch for committed changes between merge-base and HEAD.
      // Preserves the actual commit chain (author, date, message) so replay
      // containers can reconstruct the branch with real commits instead of a
      // squashed diff. Uses --stdout to emit all patches as a single text stream.
      localExecBridge.execGitCommand({
        args: ['format-patch', `${remoteBaseSha}..HEAD`, '--stdout'],
      }),
      // HEAD SHA for replay
      localExecBridge.execGitCommand({ args: ['rev-parse', 'HEAD'] }),
      // Branch name for replay
      localExecBridge.execGitCommand({
        args: ['rev-parse', '--abbrev-ref', 'HEAD'],
      }),
    ])

    let formatPatch: string | null = null
    if (formatPatchCode === 0 && formatPatchOut && formatPatchOut.trim()) {
      formatPatch = formatPatchOut
    }

    const trimmedBranch = branchName?.trim()
    return {
      remote_base_sha: remoteBaseSha,
      remote_base: remoteBase,
      patch: patch || '',
      untracked_files: untrackedFiles,
      format_patch: formatPatch,
      head_sha: headSha?.trim() || null,
      branch_name:
        trimmedBranch && trimmedBranch !== 'HEAD' ? trimmedBranch : null,
    }
  } catch (err) {
    logError(err)
    return null
  }
}

function isLocalHost(host: string): boolean {
  const hostWithoutPort = host.split(':')[0] ?? ''
  return (
    hostWithoutPort === 'localhost' ||
    /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(hostWithoutPort)
  )
}

/**
 * Checks if the current working directory appears to be a bare git repository
 * or has been manipulated to look like one (sandbox escape attack vector).
 *
 * SECURITY: Git's is_git_directory() function (setup.c:417-455) checks for:
 * 1. HEAD file - Must be a valid ref
 * 2. objects/ directory - Must exist and be accessible
 * 3. refs/ directory - Must exist and be accessible
 *
 * If all three exist in the current directory (not in a .git subdirectory),
 * Git treats the current directory as a bare repository and will execute
 * hooks/pre-commit and other hook scripts from the cwd.
 *
 * Attack scenario:
 * 1. Attacker creates HEAD, objects/, refs/, and hooks/pre-commit in cwd
 * 2. Attacker deletes or corrupts .git/HEAD to invalidate the normal git directory
 * 3. When user runs 'git status', Git treats cwd as the git dir and runs the hook
 *
 * @returns true if the cwd looks like a bare/exploited git directory
 */
/* eslint-disable custom-rules/no-sync-fs -- sync permission-eval check */
export function isCurrentDirectoryBareGitRepo(): boolean {
  const fs = getFsImplementation()
  const cwd = getCwd()

  const gitPath = join(cwd, '.git')
  try {
    const stats = fs.statSync(gitPath)
    if (stats.isFile()) {
      // worktree/submodule — Git follows the gitdir reference
      return false
    }
    if (stats.isDirectory()) {
      const gitHeadPath = join(gitPath, 'HEAD')
      try {
        // SECURITY: check isFile(). An attacker creating .git/HEAD as a
        // DIRECTORY would pass a bare statSync but Git's setup_git_directory
        // rejects it (not a valid HEAD) and falls back to cwd discovery.
        if (fs.statSync(gitHeadPath).isFile()) {
          // normal repo — .git/HEAD valid, Git won't fall back to cwd
          return false
        }
        // .git/HEAD exists but is not a regular file — fall through
      } catch {
        // .git exists but no HEAD — fall through to bare-repo check
      }
    }
  } catch {
    // no .git — fall through to bare-repo indicator check
  }

  // No valid .git/HEAD found. Check if cwd has bare git repo indicators.
  // Be cautious — flag if ANY of these exist without a valid .git reference.
  // Per-indicator try/catch so an error on one doesn't mask another.
  try {
    if (fs.statSync(join(cwd, 'HEAD')).isFile()) return true
  } catch {
    // no HEAD
  }
  try {
    if (fs.statSync(join(cwd, 'objects')).isDirectory()) return true
  } catch {
    // no objects/
  }
  try {
    if (fs.statSync(join(cwd, 'refs')).isDirectory()) return true
  } catch {
    // no refs/
  }
  return false
}
/* eslint-enable custom-rules/no-sync-fs */
