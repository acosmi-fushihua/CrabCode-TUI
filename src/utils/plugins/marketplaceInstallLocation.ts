import { posix, win32 } from 'node:path'

/**
 * Normalize an install location that may cross a process boundary as an
 * AbsolutePathBuf. Windows' `path.isAbsolute()` also accepts root-relative
 * paths (`\\foo`), so require either a drive-qualified root or a complete UNC
 * server/share root and reject Win32 device namespaces.
 */
export function normalizeSafeAbsoluteMarketplaceInstallLocationForPlatform(
  value: unknown,
  platform: NodeJS.Platform | string,
): string | null {
  const pathApi = platform === 'win32' ? win32 : posix
  if (
    typeof value !== 'string' ||
    value.length === 0 ||
    value.includes('\0') ||
    !pathApi.isAbsolute(value)
  ) {
    return null
  }

  const normalized = pathApi.normalize(value)
  if (platform !== 'win32') return normalized

  const root = pathApi.parse(normalized).root
  if (/^[a-z]:[\\/]$/iu.test(root)) return normalized
  if (!root.startsWith('\\\\')) return null
  const uncSegments = root
    .slice(2)
    .split(/[\\/]+/u)
    .filter(Boolean)
  if (
    uncSegments.length < 2 ||
    uncSegments[0] === '.' ||
    uncSegments[0] === '?'
  ) {
    return null
  }
  return normalized
}

export function normalizeSafeAbsoluteMarketplaceInstallLocation(
  value: unknown,
): string | null {
  return normalizeSafeAbsoluteMarketplaceInstallLocationForPlatform(
    value,
    process.platform,
  )
}
