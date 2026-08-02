import { readFileSync, realpathSync, statSync } from 'node:fs'
import { basename, dirname, join, normalize, resolve, sep } from 'node:path'

type DiagnosticProvider = (
  level: 'info',
  event: string,
  data?: Record<string, unknown>,
) => void

let diagnosticProvider: DiagnosticProvider | null = null

export function installWorkspaceProjectIdentityDiagnostics(
  provider: DiagnosticProvider,
): void {
  diagnosticProvider = provider
}

type PathCache<Result> = {
  clear(): void
  size(): number
  delete(key: string): boolean
  get(key: string): Result | undefined
  has(key: string): boolean
}

type MemoizedPathFunction<Result> = {
  (path: string): Result
  cache: PathCache<Result>
}

function memoizePath<Result>(
  compute: (path: string) => Result,
  maximum: number,
): MemoizedPathFunction<Result> {
  const values = new Map<string, Result>()
  const memoized = ((path: string): Result => {
    if (values.has(path)) {
      const value = values.get(path) as Result
      values.delete(path)
      values.set(path, value)
      return value
    }
    const value = compute(path)
    values.set(path, value)
    if (values.size > maximum) {
      const oldest = values.keys().next().value
      if (oldest !== undefined) values.delete(oldest)
    }
    return value
  }) as MemoizedPathFunction<Result>
  memoized.cache = {
    clear: () => values.clear(),
    size: () => values.size,
    delete: key => values.delete(key),
    get: key => values.get(key),
    has: key => values.has(key),
  }
  return memoized
}

const findGitRootMemoized = memoizePath((startPath: string): string | null => {
  const startTime = Date.now()
  diagnosticProvider?.('info', 'find_git_root_started')

  let current = resolve(startPath)
  const root = current.substring(0, current.indexOf(sep) + 1) || sep
  let statCount = 0

  while (current !== root) {
    try {
      const gitPath = join(current, '.git')
      statCount += 1
      const stats = statSync(gitPath)
      if (stats.isDirectory() || stats.isFile()) {
        diagnosticProvider?.('info', 'find_git_root_completed', {
          duration_ms: Date.now() - startTime,
          stat_count: statCount,
          found: true,
        })
        return current.normalize('NFC')
      }
    } catch {
      // Continue towards the filesystem root.
    }
    const parent = dirname(current)
    if (parent === current) break
    current = parent
  }

  try {
    const gitPath = join(root, '.git')
    statCount += 1
    const stats = statSync(gitPath)
    if (stats.isDirectory() || stats.isFile()) {
      diagnosticProvider?.('info', 'find_git_root_completed', {
        duration_ms: Date.now() - startTime,
        stat_count: statCount,
        found: true,
      })
      return root.normalize('NFC')
    }
  } catch {
    // Root is not a repository.
  }

  diagnosticProvider?.('info', 'find_git_root_completed', {
    duration_ms: Date.now() - startTime,
    stat_count: statCount,
    found: false,
  })
  return null
}, 50)

/**
 * Find the nearest .git file or directory. The cache surface intentionally
 * matches the historical git.ts export so existing invalidation remains
 * available after that module delegates here.
 */
export const findWorkspaceGitRoot = findGitRootMemoized

const canonicalRootMemoized = memoizePath((gitRoot: string): string => {
  try {
    const gitContent = readFileSync(join(gitRoot, '.git'), 'utf8').trim()
    if (!gitContent.startsWith('gitdir:')) {
      return gitRoot
    }
    const worktreeGitDir = resolve(
      gitRoot,
      gitContent.slice('gitdir:'.length).trim(),
    )
    const commonDir = resolve(
      worktreeGitDir,
      readFileSync(join(worktreeGitDir, 'commondir'), 'utf8').trim(),
    )

    // Both structural checks are required. Repository-controlled .git and
    // commondir files must not be able to borrow another trusted repository's
    // project identity.
    if (resolve(dirname(worktreeGitDir)) !== join(commonDir, 'worktrees')) {
      return gitRoot
    }
    const backlink = realpathSync(
      readFileSync(join(worktreeGitDir, 'gitdir'), 'utf8').trim(),
    )
    if (backlink !== join(realpathSync(gitRoot), '.git')) {
      return gitRoot
    }
    if (basename(commonDir) !== '.git') {
      return commonDir.normalize('NFC')
    }
    return dirname(commonDir).normalize('NFC')
  } catch {
    return gitRoot
  }
}, 50)

export const findCanonicalWorkspaceGitRoot = Object.assign(
  (startPath: string): string | null => {
    const root = findWorkspaceGitRoot(startPath)
    return root === null ? null : canonicalRootMemoized(root)
  },
  { cache: canonicalRootMemoized.cache },
)

export function normalizeWorkspaceProjectKey(path: string): string {
  return normalize(path).replace(/\\/g, '/')
}

export function resolveWorkspaceProjectKey(path: string): string {
  const resolved = resolve(path)
  const gitRoot = findCanonicalWorkspaceGitRoot(resolved)
  return normalizeWorkspaceProjectKey(gitRoot ?? resolved)
}
