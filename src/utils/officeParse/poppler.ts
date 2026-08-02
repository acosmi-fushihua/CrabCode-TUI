/**
 * W-OFFICE-POPPLER-UNIFY PR-1 — poppler(pdftoppm/pdfinfo)引擎解析层。
 *
 * poppler 是"文本模型读 PDF/office 文档"唯一路径的必需二进制（office→PDF→页图 /
 * PDF→页图 都靠 `pdftoppm`，`pdfinfo` 探测页数）。此前完全游离于 pandoc/LibreOffice
 * 那套 app 内引擎安装器体系之外（根因见同名审计+方案 doc）——本文件把它纳入同一套
 * vendored 三级解析范式，镜像 `pandoc.ts` 的结构。
 *
 * poppler 与 pandoc（单静态文件）的两处本质差异：
 *   1. 多可执行文件：`pdftoppm` + `pdfinfo`（vendored 布局 `<key>/bin/{pdftoppm,pdfinfo}`，
 *      canonical 探测锚点 = pdftoppm，pdfinfo 由归档预打包保证同在）。
 *   2. 动态库依赖（mac/linux 非自包含）：vendored 布局多一个 `<key>/lib` 目录；
 *      resolvePopplerPath 对 vendored 情况额外返回 `libDir`，调用方（`pdf.ts`）在
 *      exec 时对 vendored 情况注入 `DYLD_LIBRARY_PATH`(mac)/`LD_LIBRARY_PATH`(linux)
 *      = libDir（系统 PATH 情况不注入，系统包自带库解析）。Windows 的
 *      poppler-windows 构建自包含（含 dll），无此问题，libDir 恒为 undefined。
 *
 * 解析顺序：CRABCODE_POPPLER_PATH（指向 bin 目录，视为可信，不探测/无 libDir）→
 * vendored → PATH。
 */

import { access } from 'node:fs/promises'
import * as path from 'node:path'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'
import { platformVendorKey } from './libreoffice.js'

/** 解析出的可用 poppler 二进制路径 + (vendored 时) 动态库目录。 */
export type PopplerPaths = {
  pdftoppm: string
  pdfinfo: string
  /** 仅 vendored 场景非 undefined（mac/linux 非自包含二进制需要的动态库目录）。 */
  libDir?: string
}

/** Root vendor dir for the poppler binaries (per platform); null if unsupported. */
export function vendorPopplerDir(
  deps: { configHome?: string; platform?: NodeJS.Platform; arch?: string } = {},
): string | null {
  const key = platformVendorKey(
    deps.platform ?? process.platform,
    deps.arch ?? process.arch,
  )
  if (!key) return null
  const home = deps.configHome ?? getCrabCodeConfigHomeDir()
  return path.join(home, 'vendor', 'poppler', key)
}

/**
 * vendored poppler 二进制 + 动态库目录路径（不探测是否存在，纯路径计算）。
 * 布局：`<vendorDir>/bin/{pdftoppm,pdfinfo}{,.exe}` + `<vendorDir>/lib`(非 win32)。
 */
export function vendoredPopplerPaths(
  deps: { configHome?: string; platform?: NodeJS.Platform; arch?: string } = {},
): PopplerPaths | null {
  const dir = vendorPopplerDir(deps)
  if (!dir) return null
  const platform = deps.platform ?? process.platform
  const binDir = path.join(dir, 'bin')
  const pdftoppmBin = platform === 'win32' ? 'pdftoppm.exe' : 'pdftoppm'
  const pdfinfoBin = platform === 'win32' ? 'pdfinfo.exe' : 'pdfinfo'
  const result: PopplerPaths = {
    pdftoppm: path.join(binDir, pdftoppmBin),
    pdfinfo: path.join(binDir, pdfinfoBin),
  }
  // Windows poppler-windows 构建自包含（dll 随二进制同目录），mac/linux 需要
  // 显式动态库搜索路径（方案 A：调用方 exec 时注入 env，见 pdf.ts）。
  if (platform !== 'win32') {
    result.libDir = path.join(dir, 'lib')
  }
  return result
}

/**
 * pdftotext 在 vendored / `CRABCODE_POPPLER_PATH` 布局下的兄弟路径（与 pdftoppm
 * 同一 bin 目录）。
 *
 * pdftotext **不是** canonical 锚点：2026-07-01 之前打的 mac/linux 归档只含
 * pdftoppm/pdfinfo，装了那批归档的存量机器上它并不存在。所以本函数只负责"路径
 * 长什么样"这一条真源，存在性由各调用方自己判：
 *   - `pdf.ts::resolvePdftotext` —— 在 → 走文本层直读；不在 → 静默退回页图链
 *   - `download.ts::downloadPopplerEngine` —— 不在 = 安装不完整，「重新下载」
 *     必须真的重下重解（否则半截安装永远修不好）
 */
export function siblingPdftotextPath(
  binDir: string,
  platform: NodeJS.Platform = process.platform,
): string {
  return path.join(binDir, platform === 'win32' ? 'pdftotext.exe' : 'pdftotext')
}

export type PopplerResolveDeps = {
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

/**
 * pdftoppm 无 `--version`，只有 `-v`（老版本有时非 0 退出但仍把版本信息打到
 * stderr）——沿用 `pdf.ts` 现有 `isPdftoppmAvailable` 的判定口径：code===0 或
 * stderr 非空即视为可用。
 */
async function defaultProbeVersion(bin: string): Promise<boolean> {
  try {
    const { code, stderr } = await localExecBridge.execCommand({
      command: bin,
      args: ['-v'],
      timeout: 5_000,
    })
    return code === 0 || stderr.length > 0
  } catch {
    return false
  }
}

/**
 * Resolve usable poppler binaries, or null if none available.
 * Order: CRABCODE_POPPLER_PATH env override（指向 bin 目录，可信、不探测、无 libDir）
 * → vendored（exists 探测 pdftoppm 作 canonical 锚点）→ PATH（probeVersion 探测）。
 */
export async function resolvePopplerPath(
  deps: PopplerResolveDeps = {},
): Promise<PopplerPaths | null> {
  const exists = deps.exists ?? defaultExists
  const probeVersion = deps.probeVersion ?? defaultProbeVersion
  const env = deps.env ?? process.env
  const platform = deps.platform ?? process.platform

  // 1. Explicit override — trusted, no probing (CI / power users), 与
  // pandoc/soffice 的 env override 语义一致。指向 bin 目录（含两个二进制）。
  const override = env.CRABCODE_POPPLER_PATH
  if (typeof override === 'string' && override.trim().length > 0) {
    const dir = override.trim()
    const pdftoppmBin = platform === 'win32' ? 'pdftoppm.exe' : 'pdftoppm'
    const pdfinfoBin = platform === 'win32' ? 'pdfinfo.exe' : 'pdfinfo'
    const resolved: PopplerPaths = {
      pdftoppm: path.join(dir, pdftoppmBin),
      pdfinfo: path.join(dir, pdfinfoBin),
    }
    // FIX-10 (#2 尖角): 若 override 指向 vendored-style 布局(bin/ 旁有兄弟 lib/)
    // 且是 mac/linux(非自包含,需 DYLD/LD_LIBRARY_PATH),自动补 libDir 供调用方
    // (pdf.ts)注入——**仅当兄弟 lib/ 真存在时**(存在→注入更稳;不存在→维持现状,
    // 绝不劣化)。指向系统 poppler(无兄弟 lib/,靠系统包解析库)的 power-user 不受
    // 影响。Windows 自包含(dll 随 exe),libDir 恒 undefined。
    if (platform !== 'win32') {
      const libDir = path.join(dir, '..', 'lib')
      if (await exists(libDir)) {
        resolved.libDir = libDir
      }
    }
    return resolved
  }

  // 2. Vendored runtime download — canonical anchor is pdftoppm.
  const vendored = vendoredPopplerPaths({
    configHome: deps.configHome,
    platform,
    arch: deps.arch,
  })
  if (vendored && (await exists(vendored.pdftoppm))) return vendored

  // 3. PATH lookup.
  if (await probeVersion('pdftoppm')) {
    return { pdftoppm: 'pdftoppm', pdfinfo: 'pdfinfo' }
  }

  return null
}
