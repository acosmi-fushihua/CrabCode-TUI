import { posix, win32 } from 'path'

const GIT_SPARSE_GLOB_META = /[*?\[\]]/u
const WINDOWS_DRIVE_PREFIX = /^[a-z]:/iu

export class MarketplaceSparsePathError extends Error {
  constructor(
    readonly index: number,
    readonly value: string,
    reason: string,
  ) {
    super(`sparse path at index ${index} ${reason}: ${JSON.stringify(value)}`)
    this.name = 'MarketplaceSparsePathError'
  }
}

/**
 * Validate git sparse-checkout cone paths and return a portable normalized
 * representation. This helper is shared by every marketplace ingress so a
 * No caller can smuggle pattern-mode or traversal
 * syntax into `git sparse-checkout set --cone`.
 */
export function normalizeMarketplaceSparsePaths(
  paths: readonly string[],
): string[] {
  return paths.map((rawPath, index) => {
    if (rawPath.trim().length === 0) {
      throw new MarketplaceSparsePathError(index, rawPath, 'must not be blank')
    }
    if (rawPath.includes('\0')) {
      throw new MarketplaceSparsePathError(
        index,
        rawPath,
        'must not contain a NUL byte',
      )
    }

    const portablePath = rawPath.replace(/\\/gu, '/')
    if (
      posix.isAbsolute(portablePath) ||
      win32.isAbsolute(rawPath) ||
      WINDOWS_DRIVE_PREFIX.test(rawPath)
    ) {
      throw new MarketplaceSparsePathError(index, rawPath, 'must be relative')
    }
    if (portablePath.split('/').includes('..')) {
      throw new MarketplaceSparsePathError(
        index,
        rawPath,
        "must not contain a '..' segment",
      )
    }
    if (GIT_SPARSE_GLOB_META.test(portablePath)) {
      throw new MarketplaceSparsePathError(
        index,
        rawPath,
        'must not contain git glob metacharacters (*, ?, [, ])',
      )
    }

    return posix.normalize(portablePath)
  })
}
