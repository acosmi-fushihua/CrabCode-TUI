/**
 * W-OFFICE-PARSE PR-CC4b — pandoc 直转文本引擎（docx / odt / rtf → markdown）。
 *
 * 用户裁决（2026-06-20）：docx 等文字处理文档应**直接转文本**，而非 LibreOffice→PDF。
 * 直转 markdown 的收益：更轻（pandoc ~150MB vs LibreOffice ~400MB）、更省 token
 * （文本 vs PDF 页图）、**纯文本模型直接可读、无需视觉兜底**。表格/演示/版式类
 * （xlsx/pptx/旧 doc）仍走 LibreOffice→PDF（pandoc 读不了 xlsx/pptx，版式也需视觉保真）。
 *
 * W-DOCX-EMBEDDED-IMAGE（2026-07-01）：早期实现有意省略 `--extract-media`（"we
 * want text, not extracted image files"），代价是内嵌图片全丢（悬空
 * `media/...` 引用，媒体从未落盘）。现在改为 `--extract-media` 落盘真实图片 +
 * FileReadTool 侧把它们作为额外 image block 附加，texts 优势不变、图片补回。
 *
 * D3（2026-07-01）：rtf 加入本路由（`composerAttachments.ts` 前端附件白名单早已
 * 允许 rtf，此前无解析分支接管，落到通用文本读取路径看到裸控制字）。本机实测
 * `pandoc -f rtf --extract-media=...` 与 docx/odt 同样支持媒体抽取（同一套
 * `sanitizePandocMediaReferences` 天然覆盖，零额外代码）。
 *

 * License boundary: pandoc is GPL. This code only invokes it as an arms-length subprocess
 * （exec 一个独立二进制），**绝不静态/动态链接进 CrabCode**——与调用 `soffice` /
 * `rg` / `pdftoppm` 子进程同范式，GPL「separate programs / mere aggregation」合规。
 * vendored 时物理隔离在 `<configHome>/vendor/pandoc/` + 随附源 offer。
 *
 * pandoc 解析顺序：CRABCODE_PANDOC_PATH → vendored → PATH `pandoc`。
 */

import { mkdtemp, stat } from 'node:fs/promises'
import { access } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import * as path from 'node:path'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'
import { platformVendorKey } from './libreoffice.js'

/**
 * 走 pandoc 直转文本的扩展名：文字处理文档（docx / odt / rtf）+ 电子书（epub）。
 * 其余 office 类型（xls/xlsx/ppt/pptx/doc/ods/odp）pandoc 不支持或不适合 →
 * 由 LibreOffice→PDF 承接。
 *
 * 要求：扩展名字面量必须与 pandoc `-f` 输入格式名一致（见下方 `from` 的直传用法）
 * —— 新增成员前用 `pandoc --list-input-formats` 核实字面量匹配，否则会静默传错
 * 格式名给 pandoc。当前四者（docx/odt/rtf/epub）均已核实与 pandoc 输入格式名
 * 逐字一致（docx/odt/rtf: D3 2026-07-01；epub: W-DOC-VISION-QUALITY-REMEDIATION
 * PR-4 2026-07-01,`pandoc --list-input-formats | grep epub` 确认）。
 */
export const PANDOC_TEXT_EXTENSIONS: ReadonlySet<string> = new Set([
  'docx',
  'odt',
  'rtf',
  'epub',
])

/** True if `ext`（带/不带点）应走 pandoc 直转文本。 */
export function isPandocTextExtension(ext: string): boolean {
  const normalized = ext.startsWith('.') ? ext.slice(1) : ext
  return PANDOC_TEXT_EXTENSIONS.has(normalized.toLowerCase())
}

/** Root vendor dir for the pandoc binary (per platform); null if unsupported. */
export function vendorPandocDir(
  deps: { configHome?: string; platform?: NodeJS.Platform; arch?: string } = {},
): string | null {
  const key = platformVendorKey(
    deps.platform ?? process.platform,
    deps.arch ?? process.arch,
  )
  if (!key) return null
  const home = deps.configHome ?? getCrabCodeConfigHomeDir()
  return path.join(home, 'vendor', 'pandoc', key)
}

/** vendored pandoc 二进制路径（单文件，无 .app bundle，与 ripgrep 同布局）。 */
export function vendoredPandocPath(
  deps: { configHome?: string; platform?: NodeJS.Platform; arch?: string } = {},
): string | null {
  const dir = vendorPandocDir(deps)
  if (!dir) return null
  const platform = deps.platform ?? process.platform
  const bin = platform === 'win32' ? 'pandoc.exe' : 'pandoc'
  return path.join(dir, bin)
}

export type PandocResolveDeps = {
  exists?: (p: string) => Promise<boolean>
  probeVersion?: (bin: string) => Promise<boolean>
  env?: Record<string, string | undefined>
  platform?: NodeJS.Platform
  arch?: string
  configHome?: string
}

async function defaultExists(p: string): Promise<boolean> {
  try {
    await access(p)
    return true
  } catch {
    return false
  }
}

async function defaultProbeVersion(bin: string): Promise<boolean> {
  try {
    const { code } = await localExecBridge.execCommand({
      command: bin,
      args: ['--version'],
      timeout: 10_000,
    })
    return code === 0
  } catch {
    return false
  }
}

/** Resolve a usable pandoc binary path/command, or null. */
export async function resolvePandocPath(
  deps: PandocResolveDeps = {},
): Promise<string | null> {
  const exists = deps.exists ?? defaultExists
  const probeVersion = deps.probeVersion ?? defaultProbeVersion
  const env = deps.env ?? process.env

  const override = env.CRABCODE_PANDOC_PATH
  if (typeof override === 'string' && override.trim().length > 0) {
    return override.trim()
  }

  const vendored = vendoredPandocPath({
    configHome: deps.configHome,
    platform: deps.platform,
    arch: deps.arch,
  })
  if (vendored && (await exists(vendored))) return vendored

  if (await probeVersion('pandoc')) return 'pandoc'
  return null
}

export type PandocConvertResult =
  | { success: true; mdPath: string; mediaDir: string }
  | { success: false; error: string }

export type PandocConvertDeps = {
  exec?: (req: {
    command: string
    args: string[]
    timeout: number
  }) => Promise<{ code: number; stdout: string; stderr: string }>
  exists?: (p: string) => Promise<boolean>
  makeOutDir?: () => Promise<string>
  /** 2026-07-04 审计 PR-10 — 产物字节数（空产物判失败）。默认真 fs.stat。 */
  statSize?: (p: string) => Promise<number>
}

/**
 * Convert a docx/odt to a GitHub-flavored-markdown file via pandoc. Returns the
 * produced `.md` path; the caller (FileReadTool) then reads it through the
 * normal text path (token guards, offset/limit) — same recursion trick as the
 * office→PDF branch, but landing on `md` instead of `pdf`.
 *
 * `--wrap=none` keeps lines unwrapped (cleaner for diff/grep); `-t gfm` gives
 * readable tables/lists.
 *
 * `--extract-media=<outDir>` (W-DOCX-EMBEDDED-IMAGE, 2026-07-01) extracts
 * embedded images to disk instead of leaving dangling `media/...` references
 * in the markdown — pandoc rewrites those references to absolute paths under
 * `outDir` (subfolder name varies by source format: `media/` for docx,
 * `Pictures/` for odt). `mediaDir` (== `outDir`) is returned so the caller can
 * find + sanitize those references and attach the real image bytes as image
 * blocks (see `sanitizePandocMediaReferences` in FileReadTool.ts) — this
 * function itself does no markdown post-processing, it only runs pandoc.
 */
export async function convertDocxToMarkdown(
  srcPath: string,
  pandoc: string,
  ext: string,
  deps: PandocConvertDeps = {},
): Promise<PandocConvertResult> {
  const exec = deps.exec ?? localExecBridge.execCommand.bind(localExecBridge)
  const exists = deps.exists ?? defaultExists
  const makeOutDir =
    deps.makeOutDir ?? (() => mkdtemp(path.join(tmpdir(), 'crabcode-pandoc-')))

  const normalized = ext.startsWith('.') ? ext.slice(1) : ext
  // Extension literal IS the pandoc -f format name (see PANDOC_TEXT_EXTENSIONS
  // doc comment — this identity is verified there, not assumed here).
  const from = normalized.toLowerCase()
  const outDir = await makeOutDir()
  const base = path.basename(srcPath, path.extname(srcPath))
  const mdPath = path.join(outDir, `${base}.md`)

  const { code, stderr } = await exec({
    command: pandoc,
    args: [
      '-f',
      from,
      '-t',
      'gfm',
      '--wrap=none',
      `--extract-media=${outDir}`,
      '-o',
      mdPath,
      srcPath,
    ],
    timeout: 60_000,
  })

  // 2026-07-04 审计 PR-10:成功判据加 exit code + 非空产物——pandoc 部分失败
  // （exit≠0）也可能留下残缺 mdPath,仅以"文件存在"判成功会把截断产物当全量。
  if (code === 0 && (await exists(mdPath))) {
    const statSize =
      deps.statSize ??
      (async (p: string) => (await stat(p).catch(() => null))?.size ?? 0)
    if ((await statSize(mdPath)) > 0) {
      return { success: true, mdPath, mediaDir: outDir }
    }
    return {
      success: false,
      error: `pandoc 转换 "${path.basename(srcPath)}" 产出空文件 (exit 0 但 0 字节).`,
    }
  }
  return {
    success: false,
    error:
      `pandoc 转换 "${path.basename(srcPath)}" 为文本失败 (exit ${code}).` +
      `${stderr ? ` ${stderr.trim().slice(0, 300)}` : ''}`,
  }
}
