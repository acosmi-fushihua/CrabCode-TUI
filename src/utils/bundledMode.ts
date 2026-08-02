/**
 * Detects if the current runtime is Bun.
 * Returns true when:
 * - Running a JS file via the `bun` command
 * - Running a Bun-compiled standalone executable
 */
export function isRunningWithBun(): boolean {
  // https://bun.com/guides/util/detect-bun
  return process.versions.bun !== undefined
}

/**
 * Detects if running as a Bun-compiled standalone executable.
 * This checks for embedded files which are present in compiled binaries.
 */
export function isInBundledMode(): boolean {
  return (
    typeof Bun !== 'undefined' &&
    Array.isArray(Bun.embeddedFiles) &&
    Bun.embeddedFiles.length > 0
  )
}

/**
 * Native TUI release layout detection. A release uses an external Bun,
 * `dist/tui-runtime/index.js`, and the Rust TUI launcher in one install root;
 * it is not a
 * `bun --compile` 单文件 —— `Bun.embeddedFiles` 恒空，isInBundledMode 探测
 * 不到，安装形态会误判 unknown，`crabcode update` 走不进 native 更新分支。
 *
 * 判据（三条同时成立才算，repo dev 跑 dist 不会误中）：
 * 1. 当前脚本是 `<root>/dist/tui-runtime/index.js`
 * 2. `<root>/crabcode-tui(.exe)` 启动器是普通文件
 * 3. 正在执行的 bun 就是 `<root>/bun(.exe)`（dev 用的是 ~/.local/share 的
 *    共享 bun，不在 repo 根）
 *
 * 返回安装根绝对路径（realpath 解析后），非 flat bundle 形态返回 null。
 */
export function getFlatBundleInstallRoot(): string | null {
  try {
    // 延迟 require：保持本模块零顶层依赖（被低层模块广泛 import）
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const fs = require('fs') as typeof import('fs')
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const path = require('path') as typeof import('path')

    const scriptPath = process.argv[1]
    if (!scriptPath) return null
    const realScript = fs.realpathSync(path.resolve(scriptPath))
    if (path.basename(realScript) !== 'index.js') return null
    const scriptDir = path.dirname(realScript)
    const isTuiEntry =
      path.basename(scriptDir) === 'tui-runtime' &&
      path.basename(path.dirname(scriptDir)) === 'dist'
    if (!isTuiEntry) return null
    const root = path.dirname(path.dirname(scriptDir))

    const ext = process.platform === 'win32' ? '.exe' : ''
    const launcher = path.join(root, `crabcode-tui${ext}`)
    const bun = path.join(root, `bun${ext}`)
    if (!fs.lstatSync(launcher).isFile() || !fs.lstatSync(bun).isFile()) {
      return null
    }

    const realExec = fs.realpathSync(process.execPath)
    if (realExec !== bun) return null

    return root
  } catch {
    return null
  }
}
