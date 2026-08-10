import { homedir } from 'os'
import { dirname, isAbsolute, join, normalize, relative, resolve } from 'path'
import { fileURLToPath } from 'url'
import { getCwd } from './cwd.js'
import { getFsImplementation } from './fsOperations.js'
import { getPlatform } from './platform.js'
import { posixPathToWindowsPath } from './windowsPaths.js'

/**
 * Expands a path that may contain tilde notation (~) to an absolute path.
 *
 * On Windows, POSIX-style paths (e.g., `/c/Users/...`) are automatically converted
 * to Windows format (e.g., `C:\Users\...`). The function always returns paths in
 * the native format for the current platform.
 *
 * @param path - The path to expand, may contain:
 *   - `~` - expands to user's home directory
 *   - `~/path` - expands to path within user's home directory
 *   - absolute paths - returned normalized
 *   - relative paths - resolved relative to baseDir
 *   - POSIX paths on Windows - converted to Windows format
 * @param baseDir - The base directory for resolving relative paths (defaults to current working directory)
 * @returns The expanded absolute path in the native format for the current platform
 *
 * @throws {Error} If path is invalid
 *
 * @example
 * expandPath('~') // '/home/user'
 * expandPath('~/Documents') // '/home/user/Documents'
 * expandPath('./src', '/project') // '/project/src'
 * expandPath('/absolute/path') // '/absolute/path'
 */
export function expandPath(path: string, baseDir?: string): string {
  // Set default baseDir to getCwd() if not provided
  const actualBaseDir = baseDir ?? getCwd() ?? getFsImplementation().cwd()

  // Input validation
  if (typeof path !== 'string') {
    throw new TypeError(`Path must be a string, received ${typeof path}`)
  }

  if (typeof actualBaseDir !== 'string') {
    throw new TypeError(
      `Base directory must be a string, received ${typeof actualBaseDir}`,
    )
  }

  // Security: Check for null bytes
  if (path.includes('\0') || actualBaseDir.includes('\0')) {
    throw new Error('Path contains null bytes')
  }

  // Handle empty or whitespace-only paths
  const trimmedPath = path.trim()
  if (!trimmedPath) {
    return normalize(actualBaseDir).normalize('NFC')
  }

  // Handle home directory notation
  if (trimmedPath === '~') {
    return homedir().normalize('NFC')
  }

  if (trimmedPath.startsWith('~/')) {
    return join(homedir(), trimmedPath.slice(2)).normalize('NFC')
  }

  // On Windows, convert POSIX-style paths (e.g., /c/Users/...) to Windows format
  let processedPath = trimmedPath
  if (getPlatform() === 'windows' && trimmedPath.match(/^\/[a-z]\//i)) {
    try {
      processedPath = posixPathToWindowsPath(trimmedPath)
    } catch {
      // If conversion fails, use original path
      processedPath = trimmedPath
    }
  }

  // Handle absolute paths
  if (isAbsolute(processedPath)) {
    return normalize(processedPath).normalize('NFC')
  }

  // Handle relative paths
  return resolve(actualBaseDir, processedPath).normalize('NFC')
}

/**
 * Converts an absolute path to a relative path from cwd, to save tokens in
 * tool output. If the path is outside cwd (relative path would start with ..),
 * returns the absolute path unchanged so it stays unambiguous.
 *
 * @param absolutePath - The absolute path to relativize
 * @returns Relative path if under cwd, otherwise the original absolute path
 */
export function toRelativePath(absolutePath: string): string {
  const relativePath = relative(getCwd(), absolutePath)
  // If the relative path would go outside cwd (starts with ..), keep absolute
  return relativePath.startsWith('..') ? absolutePath : relativePath
}

/**
 * Gets the directory path for a given file or directory path.
 * If the path is a directory, returns the path itself.
 * If the path is a file or doesn't exist, returns the parent directory.
 *
 * @param path - The file or directory path
 * @returns The directory path
 */
export function getDirectoryForPath(path: string): string {
  const absolutePath = expandPath(path)
  // SECURITY: Skip filesystem operations for UNC paths to prevent NTLM credential leaks.
  if (absolutePath.startsWith('\\\\') || absolutePath.startsWith('//')) {
    return dirname(absolutePath)
  }
  try {
    const stats = getFsImplementation().statSync(absolutePath)
    if (stats.isDirectory()) {
      return absolutePath
    }
  } catch {
    // Path doesn't exist or can't be accessed
  }
  // If it's not a directory or doesn't exist, return the parent directory
  return dirname(absolutePath)
}

/**
 * Checks if a path contains directory traversal patterns that navigate to parent directories.
 *
 * @param path - The path to check for traversal patterns
 * @returns true if the path contains traversal (e.g., '../', '..\', or ends with '..')
 */
export function containsPathTraversal(path: string): boolean {
  return /(?:^|[\\/])\.\.(?:[\\/]|$)/.test(path)
}

// Re-export from the shared zero-dep source.
export { sanitizePath } from './sessionStoragePortable.js'

/**
 * Normalizes a path for use as a JSON config key.
 * On Windows, paths can have inconsistent separators (C:\path vs C:/path)
 * depending on whether they come from git, Node.js APIs, or user input.
 * This normalizes to forward slashes for consistent JSON serialization.
 *
 * @param path - The path to normalize
 * @returns The normalized path with consistent forward slashes
 */
export function normalizePathForConfigKey(path: string): string {
  // First use Node's normalize to resolve . and .. segments
  const normalized = normalize(path)
  // Then convert all backslashes to forward slashes for consistent JSON keys
  // This is safe because forward slashes work in Windows paths for most operations
  return normalized.replace(/\\/g, '/')
}

/**
 * Converts a `file://` URI to a native filesystem path. Non-`file://` input is
 * returned unchanged, so callers can pass through custom schemes untouched.
 *
 * 用它替换手写的 `uri.slice(7)` / `uri.replace('file://', '')`。那种朴素前缀算术
 * 只对**我们自己手拼**的 `file://` + 裸路径成立，对规范形态一概失效：
 *
 *   file://C:\p\a.ts            剥7 -> 'C:\p\a.ts'            ← 只有这个形态能命中
 *   file:///C:/p/a.ts           剥7 -> '/C:/p/a.ts'           ← 多一个前导斜杠，比较必挂
 *   file:///C:/p/%E6%94%AF.ts   剥7 -> '/C:/p/%E6%94%AF.ts'   ← 百分号编码原样留着
 *   file://server/share/a.ts    剥7 -> 'server/share/a.ts'    ← UNC 主机名被当成目录
 *
 * 而 `file:///…`（三斜杠 + 百分号编码）正是 RFC 8089 与 LSP/MCP 规范要求的形态，
 * 也是 VS Code / JetBrains 等第三方回给我们的形态 —— 换句话说，朴素剥离**只认
 * 我们自己写错的那一种**。fileURLToPath 上面四种全部正确还原（实测，2026-08-06）。
 *
 * 解析失败才退回朴素剥离：唯一已知触发是在 POSIX 上收到 `file://C:\…`（host 非空，
 * Node 抛 ERR_INVALID_FILE_URL_HOST）。这条兜底保证本函数**永不比旧行为更差**。
 *
 * @param uri - A `file://` URI, or any other string (returned as-is)
 * @returns The native filesystem path, or the original string for non-file URIs
 */
export function fileUriToPath(uri: string): string {
  if (!uri.startsWith('file://')) {
    return uri
  }
  try {
    return fileURLToPath(uri)
  } catch {
    return uri.slice('file://'.length)
  }
}
