/**
 * 2026-07-24 文档解析根因重审 F1a — agent Bash 探测面对齐 vendored 引擎真源。
 *
 * 根因（M1 探测盲区）：设置页/解析链走 resolver（env override → vendored →
 * PATH），而模型在 Bash 里自检用 `which`/`brew` —— 只看 PATH 变量，结构性看不见
 * `<configHome>/vendor/**`。Mac 实证：vendored pdftoppm 成功出页图 8 分钟后，
 * 管家 `which`×7 报"三件套全未安装"。
 *
 * 修复：把**确实存在**的 vendored 引擎目录 append（刻意不 prepend——不遮蔽用户
 * 自装的系统引擎；与 `bun_worker.rs` sibling-cli 的版本钉死型 prepend 语义不同）
 * 到 Bash 子进程 PATH 尾部。平台差异：
 *   - win32：poppler `bin/` + pandoc 根目录 + libreoffice `program/`（三者自包含）
 *   - darwin/linux：仅 pandoc 根目录 + libreoffice 可执行目录；**poppler 不入
 *     PATH**——mac/linux vendored poppler 非自包含（需 DYLD/LD_LIBRARY_PATH 指向
 *     兄弟 `lib/`，见 `poppler.ts` 头注），入 PATH 会造出"找得到但一跑就 dyld
 *     报错"的半死态，比"找不到"更糟。其可见性由 BashTool prompt 提示承担（F1b）。
 *
 * 存在性判据 = canonical 引擎二进制真实在盘（与 resolver 同口径），而非裸目录
 * 存在——残留空目录不该进 PATH。判据每次调用现探不缓存（负缓存正是 R1 根因，
 * 见 `pdf.ts`）；每次 Bash spawn 多 ≤3 个 fs.access，相对进程 spawn 成本可忽略。
 */

import { access } from 'node:fs/promises'
import * as path from 'node:path'
import { vendoredSofficePath } from './libreoffice.js'
import { vendoredPandocPath } from './pandoc.js'
import { vendorPopplerDir } from './poppler.js'

export type VendoredEngineDirDeps = {
  platform?: NodeJS.Platform
  arch?: string
  configHome?: string
  /** Injectable for tests; defaults to fs access(). */
  exists?: (p: string) => Promise<boolean>
}

async function defaultExists(p: string): Promise<boolean> {
  try {
    await access(p)
    return true
  } catch {
    return false
  }
}

type EngineDirCandidate = {
  /** Directory to append to PATH when the anchor binary exists. */
  dir: string
  /** Canonical engine binary probed for existence (resolver 同口径). */
  anchor: string
}

/**
 * Pure path computation (no I/O): the per-platform candidate directories and
 * their canonical anchor binaries. Deduplicated, order-stable
 * (pandoc → libreoffice → poppler[win32]).
 */
export function candidateVendoredEngineDirs(
  deps: VendoredEngineDirDeps = {},
): EngineDirCandidate[] {
  const platform = deps.platform ?? process.platform
  const candidates: EngineDirCandidate[] = []

  const pandocBin = vendoredPandocPath(deps)
  if (pandocBin) {
    candidates.push({ dir: path.dirname(pandocBin), anchor: pandocBin })
  }

  const sofficeBin = vendoredSofficePath(deps)
  if (sofficeBin) {
    candidates.push({ dir: path.dirname(sofficeBin), anchor: sofficeBin })
  }

  if (platform === 'win32') {
    const popplerDir = vendorPopplerDir(deps)
    if (popplerDir) {
      const binDir = path.join(popplerDir, 'bin')
      candidates.push({
        dir: binDir,
        anchor: path.join(binDir, 'pdftoppm.exe'),
      })
    }
  }

  const seen = new Set<string>()
  return candidates.filter(c => {
    if (seen.has(c.dir)) return false
    seen.add(c.dir)
    return true
  })
}

/**
 * The vendored engine directories whose anchor binary actually exists on disk
 * right now. Never throws; probe failures count as absent.
 */
export async function existingVendoredEngineDirs(
  deps: VendoredEngineDirDeps = {},
): Promise<string[]> {
  const exists = deps.exists ?? defaultExists
  const out: string[] = []
  for (const candidate of candidateVendoredEngineDirs(deps)) {
    try {
      if (await exists(candidate.anchor)) out.push(candidate.dir)
    } catch {
      // absent — a probe failure must never break command execution
    }
  }
  return out
}

/**
 * Append `dirs` to a PATH value (tail; never prepend). Entries already present
 * are skipped — comparison is case-insensitive on win32 (PATH 语义), exact
 * elsewhere. Empty `dirs` returns the input unchanged.
 */
export function appendDirsToPathValue(
  current: string | undefined,
  dirs: string[],
  platform: NodeJS.Platform = process.platform,
): string | undefined {
  if (dirs.length === 0) return current
  const delimiter = platform === 'win32' ? ';' : ':'
  const normalize = (p: string): string =>
    platform === 'win32' ? p.toLowerCase() : p
  const present = new Set(
    (current ?? '')
      .split(delimiter)
      .filter(seg => seg.length > 0)
      .map(normalize),
  )
  const additions = dirs.filter(d => {
    const key = normalize(d)
    if (present.has(key)) return false
    present.add(key)
    return true
  })
  if (additions.length === 0) return current
  const base = current && current.length > 0 ? current : undefined
  return base ? `${base}${delimiter}${additions.join(delimiter)}` : additions.join(delimiter)
}

/**
 * Mutate a spawn env in place: append `dirs` onto its PATH variable. The PATH
 * key is discovered case-insensitively（Windows 环境里常见 `Path`）; when no
 * PATH-like key exists and there is something to add, a literal `PATH` key is
 * created.
 */
export function appendVendoredEngineDirsToEnvPath(
  env: Record<string, string | undefined>,
  dirs: string[],
  platform: NodeJS.Platform = process.platform,
): void {
  if (dirs.length === 0) return
  const pathKey =
    Object.keys(env).find(k => k.toUpperCase() === 'PATH') ?? 'PATH'
  env[pathKey] = appendDirsToPathValue(env[pathKey], dirs, platform)
}
