/**
 * LibreOffice headless office→PDF engine resolver.
 *
 * CrabCode reads docx/xlsx/pptx (and legacy doc/xls/ppt + ODF) by converting
 * them to PDF with a headless `soffice` (LibreOffice) and routing the PDF
 * through the EXISTING PDF chain in FileReadTool (page-image extraction for
 * text-only models, document block otherwise). No new model capability, no new
 * SDK/gateway dependency — office just rides the PDF rails.
 *
 * `soffice` resolution order (first hit wins):
 *   1. CRABCODE_SOFFICE_PATH env override (explicit; CI / power users).
 *   2. Vendored runtime download: <configHome>/vendor/libreoffice/<platformKey>/
 *      (populated on demand from settings).
 *   3. System install: macOS /Applications bundle, then PATH `soffice` /
 *      `libreoffice` (users who already have LibreOffice use it with 0 download).
 *
 * Conversion is NEVER attempted when no `soffice` is resolvable — the caller
 * (FileReadTool) returns a clear install hint instead of blocking the turn on a
 * ~400MB synchronous download (审计问题 2: lazy sync download would freeze the
 * agent turn for minutes; download is an explicit background action, not a
 * tool-call side effect).
 *
 * This module lives in the TUI's Bun runtime so it can use filesystem and child
 * process APIs directly.
 */

import { mkdtemp } from 'node:fs/promises'
import { access } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import * as path from 'node:path'
import { pathToFileURL } from 'node:url'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'

/**
 * Office extensions CrabCode can parse via LibreOffice. Mirrors exactly the
 * office entries in `src/constants/files.ts` BINARY_EXTENSIONS — these are the
 * files the binary gate used to reject outright.
 */
export const OFFICE_EXTENSIONS: ReadonlySet<string> = new Set([
  'doc',
  'docx',
  'xls',
  'xlsx',
  'ppt',
  'pptx',
  'odt',
  'ods',
  'odp',
])

/** True if `ext` (with or without a leading dot) is a parseable office file. */
export function isOfficeExtension(ext: string): boolean {
  const normalized = ext.startsWith('.') ? ext.slice(1) : ext
  return OFFICE_EXTENSIONS.has(normalized.toLowerCase())
}

/**
 * Platform vendor key, aligned 1:1 with the ripgrep vendor layout
 * (`bootstrap.rs` vendor_ripgrep_relpath): {arch}-{os}. Returns null on an
 * unsupported platform/arch (caller falls back to system / errors clearly).
 */
export function platformVendorKey(
  platform: NodeJS.Platform = process.platform,
  arch: string = process.arch,
): string | null {
  if (platform === 'darwin') {
    if (arch === 'arm64') return 'arm64-darwin'
    if (arch === 'x64') return 'x64-darwin'
  } else if (platform === 'linux') {
    if (arch === 'x64') return 'x64-linux'
    if (arch === 'arm64') return 'arm64-linux'
  } else if (platform === 'win32') {
    if (arch === 'x64') return 'x64-win32'
  }
  return null
}

/** Root vendor dir for the LibreOffice runtime download (per platform). */
export function vendorLibreOfficeDir(
  deps: { configHome?: string; platform?: NodeJS.Platform; arch?: string } = {},
): string | null {
  const key = platformVendorKey(
    deps.platform ?? process.platform,
    deps.arch ?? process.arch,
  )
  if (!key) return null
  const home = deps.configHome ?? getCrabCodeConfigHomeDir()
  return path.join(home, 'vendor', 'libreoffice', key)
}

/**
 * Canonical `soffice` path inside the vendor dir. The downloader (PR-CC1)
 * normalizes every platform's archive into this layout so the probe is a single
 * `exists` check — exactly like ripgrep's `dist/vendor/ripgrep/<key>/rg`.
 */
export function vendoredSofficePath(
  deps: { configHome?: string; platform?: NodeJS.Platform; arch?: string } = {},
): string | null {
  const dir = vendorLibreOfficeDir(deps)
  if (!dir) return null
  const platform = deps.platform ?? process.platform
  if (platform === 'darwin') {
    return path.join(dir, 'LibreOffice.app', 'Contents', 'MacOS', 'soffice')
  }
  if (platform === 'win32') {
    return path.join(dir, 'program', 'soffice.exe')
  }
  return path.join(dir, 'program', 'soffice')
}

/** macOS system-wide install location (the most common case on a Mac). */
const MAC_SYSTEM_SOFFICE =
  '/Applications/LibreOffice.app/Contents/MacOS/soffice'

export type SofficeResolveDeps = {
  /** Defaults to fs.access existence check. */
  exists?: (p: string) => Promise<boolean>
  /** Defaults to localExecBridge.execCommand; returns the exit code. */
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

/**
 * Resolve a usable `soffice` binary path/command, or null if none is available.
 * Order: env override → vendored download → macOS bundle → PATH (soffice,
 * libreoffice). Pure aside from the injected fs/exec probes.
 */
export async function resolveSofficePath(
  deps: SofficeResolveDeps = {},
): Promise<string | null> {
  const exists = deps.exists ?? defaultExists
  const probeVersion = deps.probeVersion ?? defaultProbeVersion
  const env = deps.env ?? process.env
  const platform = deps.platform ?? process.platform

  // 1. Explicit override — trusted, no probing (CI / power users).
  const override = env.CRABCODE_SOFFICE_PATH
  if (typeof override === 'string' && override.trim().length > 0) {
    return override.trim()
  }

  // 2. Vendored runtime download.
  const vendored = vendoredSofficePath({
    configHome: deps.configHome,
    platform,
    arch: deps.arch,
  })
  if (vendored && (await exists(vendored))) return vendored

  // 3a. macOS system bundle.
  if (platform === 'darwin' && (await exists(MAC_SYSTEM_SOFFICE))) {
    return MAC_SYSTEM_SOFFICE
  }

  // 3b. PATH lookup via `--version` (cheap, definitive).
  for (const bin of ['soffice', 'libreoffice']) {
    if (await probeVersion(bin)) return bin
  }

  return null
}

export type OfficeConvertResult =
  | { success: true; pdfPath: string }
  | { success: false; error: string }

export type ConvertDeps = {
  /** Defaults to localExecBridge.execCommand. */
  exec?: (req: {
    command: string
    args: string[]
    timeout: number
  }) => Promise<{ code: number; stdout: string; stderr: string }>
  /** Defaults to fs.access existence check. */
  exists?: (p: string) => Promise<boolean>
  /** Defaults to mkdtemp under the OS temp dir. */
  makeOutDir?: () => Promise<string>
  /**
   * Wall-clock watchdog budget (ms) that bounds the whole conversion regardless
   * of whether the underlying exec promise ever settles (根因 B liveness). Defaults
   * to the exec timeout + a 15s grace. Injectable for tests.
   */
  watchdogMs?: number
}

/**
 * Convert one office file to PDF via headless LibreOffice. Returns the produced
 * PDF path or a structured error. Output goes to an isolated temp dir; the
 * intermediate PDF is small and short-lived (OS temp cleanup).
 *
 * Uses `--convert-to pdf` with `-env:UserInstallation` pinned to a per-call
 * profile dir so concurrent conversions don't fight over the shared LibreOffice
 * profile lock (a well-known headless-soffice gotcha).
 */
export async function convertOfficeToPdf(
  srcPath: string,
  soffice: string,
  deps: ConvertDeps = {},
): Promise<OfficeConvertResult> {
  const exec = deps.exec ?? localExecBridge.execCommand.bind(localExecBridge)
  const exists = deps.exists ?? defaultExists
  const makeOutDir =
    deps.makeOutDir ?? (() => mkdtemp(path.join(tmpdir(), 'crabcode-office-')))

  const outDir = await makeOutDir()
  const profileDir = path.join(outDir, '.profile')
  // LibreOffice requires a canonical file URL; string concatenation produces
  // an invalid drive-letter URL on Windows.
  const profileUrl = pathToFileURL(profileDir).href

  // 根因 B（liveness）— execa 的 `timeout` 在 Windows 下形同虚设：soffice.exe 启动后
  // 再 spawn soffice.bin 孙进程并继承 stdout/stderr 管道；execa 超时只对直接子进程发
  // SIGTERM（Windows 不杀进程树），且其 promise 在 resolve 前要 await 流 `end`，而孙
  // 进程持有的管道写端不关 → 流永不 end → execa promise 永不 settle → 整个 turn 永久
  // 挂起、零提示。
  //
  // 修复 = 一道独立于 execa promise 的 wall-clock 看门狗：不依赖 execa/子进程是否
  // settle，只要墙钟到点就强制返回结构化 error，保证 office 转换路径**总能在有界时间
  // 内 settle**，turn 拿到清晰报错而非永久空转。scoped 在 office 路径，不动被 git 等
  // 大量共享的 execFileNoThrowWithCwd 全局 execa 行为。
  // 残留：泄漏的 soffice.bin 子进程（OS 回收）；完整 tree-kill 需改 ExecBridge 契约 +
  // Windows 实机验证，记为已知边界（见 SoT 根因 B）。
  const EXEC_TIMEOUT_MS = 120_000
  // 比 execa 内部 timeout 稍宽，正常路径仍由 execa 自身在 ~秒级 settle；只有 execa
  // 卡死（Windows grandchild-pipe）时才由看门狗在此兜底接管。
  const WATCHDOG_MS = deps.watchdogMs ?? EXEC_TIMEOUT_MS + 15_000

  let watchdogTimer: ReturnType<typeof setTimeout> | undefined
  const watchdog = new Promise<{ timedOut: true }>((resolve) => {
    watchdogTimer = setTimeout(() => resolve({ timedOut: true }), WATCHDOG_MS)
  })

  const execResult = await Promise.race([
    exec({
      command: soffice,
      args: [
        '--headless',
        '--norestore',
        '--nolockcheck',
        `-env:UserInstallation=${profileUrl}`,
        '--convert-to',
        'pdf',
        '--outdir',
        outDir,
        srcPath,
      ],
      timeout: EXEC_TIMEOUT_MS,
    }).then((r) => ({ timedOut: false as const, ...r })),
    watchdog,
  ])
  if (watchdogTimer) clearTimeout(watchdogTimer)

  if (execResult.timedOut) {
    return {
      success: false,
      error: libreofficeConvertTimeoutMessage(srcPath, WATCHDOG_MS),
    }
  }
  const { code, stderr } = execResult

  // soffice names the output <basename>.pdf in --outdir, regardless of exit
  // nuance. Verify the artifact rather than trusting the exit code alone
  // (headless soffice sometimes returns 0 while emitting nothing on bad input).
  const base = path.basename(srcPath, path.extname(srcPath))
  const pdfPath = path.join(outDir, `${base}.pdf`)
  if (await exists(pdfPath)) {
    return { success: true, pdfPath }
  }
  return {
    success: false,
    error: libreofficeConvertFailedMessage(srcPath, code, stderr),
  }
}

// ─── Office 转换终态错误文案真源（文案即契约）──────────────────
// terminalToolError.ts 以字符串判据分类；分类测试直接 import 下列 builder 构造
// forward 用例（producer↔判据由测试桥接钉死）。改文案必须同步签名判据 + 分类
// 测试。

/** 签名 office_convert_timeout（'timed out converting' + 'to PDF' 判据）。 */
export function libreofficeConvertTimeoutMessage(
  srcPath: string,
  watchdogMs: number,
): string {
  return (
    `LibreOffice timed out converting "${path.basename(srcPath)}" to PDF ` +
    `(>${Math.round(watchdogMs / 1000)}s). The conversion process did not ` +
    `settle (a known Windows headless-soffice failure mode where the child ` +
    `process holds inherited pipes). Retrying the same file will likely time ` +
    `out again; try another file, or ask the user to convert it to PDF manually.`
  )
}

/** 签名 office_convert_failed（'failed to convert' + 'to PDF' 判据）。 */
export function libreofficeConvertFailedMessage(
  srcPath: string,
  code: number | null,
  stderr: string,
): string {
  return (
    `LibreOffice failed to convert "${path.basename(srcPath)}" to PDF ` +
    `(exit ${code}).${stderr ? ` ${stderr.trim().slice(0, 300)}` : ''}`
  )
}

/**
 * Human-facing guidance shown when an office file is read but no parsing engine
 * is available. Deliberately actionable (install paths + the in-app option) and
 * does NOT trigger any download itself (审计问题 2). For word docs (docx/odt)
 * the lighter pandoc path is mentioned first; otherwise LibreOffice.
 *
 * 文案即契约:terminalToolError.ts 签名 office_engine_missing 按 'office parsing
 * needs a parsing engine, which is not installed' 字符串判据,改文案必须同步
 * 判据与分类测试（测试 import 本函数构造 forward 用例）。
 */
export function officeEngineMissingMessage(filePath: string, ext?: string): string {
  const normalized = (ext ?? path.extname(filePath)).replace(/^\./, '').toLowerCase()
  const isWordDoc =
    normalized === 'docx' || normalized === 'odt' || normalized === 'rtf'
  const lines = [
    `Cannot read "${path.basename(filePath)}": office parsing needs a parsing engine, which is not installed.`,
    `Options:`,
    `  • Install the document parsing engine from CrabCode settings → 多媒体 (or the first-launch 解析服务 step).`,
  ]
  if (isWordDoc) {
    lines.push(
      `  • Word docs convert to text via pandoc — install it (macOS: \`brew install pandoc\`; Debian/Ubuntu: \`apt-get install pandoc\`) or set CRABCODE_PANDOC_PATH.`,
    )
  }
  lines.push(
    `  • Or install LibreOffice system-wide (macOS: libreoffice.org; Debian/Ubuntu: \`apt-get install libreoffice\`), or set CRABCODE_SOFFICE_PATH.`,
  )
  return lines.join('\n')
}
