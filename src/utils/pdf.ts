import { randomUUID } from 'crypto'
import { access, mkdir, readdir, readFile } from 'fs/promises'
import { dirname, join } from 'path'
import {
  PDF_MAX_EXTRACT_SIZE,
  PDF_TARGET_RAW_SIZE,
  PDF_TEXT_LAYER_MIN_DENSITY,
} from '../constants/apiLimits.js'
import { errorMessage } from './errors.js'
import { localExecBridge } from 'src/runtime/localProcess.js'
import { formatFileSize } from './format.js'
import { getFsImplementation } from './fsOperations.js'
import {
  resolvePopplerPath,
  siblingPdftotextPath,
  type PopplerPaths,
} from './officeParse/poppler.js'
import { getToolResultsDir } from './toolResultStorage.js'

// ─── PDF 终态错误文案真源（文案即契约）─────────────────────────
// terminalToolError.ts 以字符串 includes 判据把下列文案分类为会话内不可重试终态；
// tests/unit/terminal-tool-error-classification.test.ts 直接 import 本组常量构造
// forward 用例（producer↔判据由测试桥接钉死）。改任一文案必须同步签名判据 +
// 分类测试。
/** 签名 pdf_empty。 */
export const PDF_EMPTY_MESSAGE_PREFIX = 'PDF file is empty'
/** 签名 pdf_corrupted 变体（magic bytes 缺失）。 */
export const PDF_INVALID_HEADER_MESSAGE_PREFIX =
  'File is not a valid PDF (missing %PDF- header)'
/** 签名 pdf_corrupted 变体（渲染产出零页）。 */
export const PDFTOPPM_NO_PAGES_MESSAGE =
  'pdftoppm produced no output pages. The PDF may be invalid.'
/** 签名 pdf_password_protected。 */
export const PDF_PASSWORD_PROTECTED_MESSAGE =
  'PDF is password-protected. Please provide an unprotected version.'
/** 签名 pdf_corrupted（pdftotext/pdftoppm stderr 判损）。 */
export const PDF_CORRUPTED_MESSAGE = 'PDF file is corrupted or invalid.'
/** 签名 poppler_missing。 */
export const POPPLER_MISSING_MESSAGE =
  'PDF page rendering requires poppler, which is not installed. ' +
  'Install it from CrabCode settings → 多媒体 (or the first-launch 解析服务 step), ' +
  'or manually (macOS: `brew install poppler`; Debian/Ubuntu: `apt-get install poppler-utils`), ' +
  'or set CRABCODE_POPPLER_PATH.'
/** 签名 pdf_model_unsupported（FileReadTool 以此为首句拼完整指引）。 */
export const PDF_MODEL_UNSUPPORTED_MESSAGE_PREFIX =
  'Reading full PDFs is not supported with this model'

export type PDFError = {
  reason:
    | 'empty'
    | 'too_large'
    | 'password_protected'
    | 'corrupted'
    | 'unknown'
    | 'unavailable'
  message: string
}

export type PDFResult<T> =
  | { success: true; data: T }
  | { success: false; error: PDFError }

/**
 * Read a PDF file and return it as base64-encoded data.
 * @param filePath Path to the PDF file
 * @returns Result containing PDF data or a structured error
 */
export async function readPDF(filePath: string): Promise<
  PDFResult<{
    type: 'pdf'
    file: {
      filePath: string
      base64: string
      originalSize: number
    }
  }>
> {
  try {
    const fs = getFsImplementation()
    const stats = await fs.stat(filePath)
    const originalSize = stats.size

    // Check if file is empty
    if (originalSize === 0) {
      // 文案即契约:terminalToolError.ts 签名 pdf_empty 按此字符串判据,改文案必须同步
      return {
        success: false,
        error: {
          reason: 'empty',
          message: `${PDF_EMPTY_MESSAGE_PREFIX}: ${filePath}`,
        },
      }
    }

    // Check if PDF exceeds maximum size
    // The API has a 32MB total request limit. After base64 encoding (~33% larger),
    // a PDF must be under ~20MB raw to leave room for conversation context.
    if (originalSize > PDF_TARGET_RAW_SIZE) {
      return {
        success: false,
        error: {
          reason: 'too_large',
          message: `PDF file exceeds maximum allowed size of ${formatFileSize(PDF_TARGET_RAW_SIZE)}.`,
        },
      }
    }

    const fileBuffer = await readFile(filePath)

    // Validate PDF magic bytes — reject files that aren't actually PDFs
    // (e.g., HTML files renamed to .pdf) before they enter conversation context.
    // Once an invalid PDF document block is in the message history, every subsequent
    // API call fails with 400 "The PDF specified was not valid" and the session
    // becomes unrecoverable without /clear.
    const header = fileBuffer.subarray(0, 5).toString('ascii')
    if (!header.startsWith('%PDF-')) {
      // 文案即契约:terminalToolError.ts 签名 pdf_corrupted 变体判据,改文案必须同步
      return {
        success: false,
        error: {
          reason: 'corrupted',
          message: `${PDF_INVALID_HEADER_MESSAGE_PREFIX}: ${filePath}`,
        },
      }
    }

    const base64 = fileBuffer.toString('base64')

    // Note: We cannot check page count here without parsing the PDF
    // The API will enforce the 100-page limit and return an error if exceeded

    return {
      success: true,
      data: {
        type: 'pdf',
        file: {
          filePath,
          base64,
          originalSize,
        },
      },
    }
  } catch (e: unknown) {
    return {
      success: false,
      error: {
        reason: 'unknown',
        message: errorMessage(e),
      },
    }
  }
}

export type PopplerCallDeps = {
  /** Injectable poppler resolver (tests only); defaults to `resolvePopplerPath`. */
  resolve?: () => Promise<PopplerPaths | null>
}

// Cached poppler resolution (W-OFFICE-POPPLER-UNIFY PR-2): caches the resolved
// *paths* (vendored bin + libDir, or system PATH command names), not just a
// boolean — `extractPDFPages`/`getPDFPageCount` both need the resolved command
// path + libDir, not just a yes/no. Only SUCCESSFUL resolutions are cached for
// the lifetime of the process; an absent probe (null) is re-probed on every
// call. Caching the negative can wedge the TUI because installation runs in the
// control worker (which calls `resetPdftoppmCache()`), but FileReadTool runs
// in a long-lived per-connection agent worker whose stale "absent" survived
// until a window restart (2026-07-24 根因重审, R1 负缓存).
let popplerResolution: PopplerPaths | undefined

/**
 * Reset the poppler resolution cache. Used by tests, and after an install
 * completes so the next call re-probes instead of reusing a stale "absent".
 */
export function resetPdftoppmCache(): void {
  popplerResolution = undefined
  pdftotextResolution = undefined
}

async function resolvePoppler(deps: PopplerCallDeps): Promise<PopplerPaths | null> {
  if (popplerResolution !== undefined) return popplerResolution
  const resolve = deps.resolve ?? resolvePopplerPath
  const resolved = await resolve()
  // Negative results are deliberately NOT cached: a mid-session install must
  // become visible on the next call without any cross-process invalidation.
  if (resolved) popplerResolution = resolved
  return resolved
}

/**
 * Env override for vendored poppler's dynamic library search path (方案 A,
 * W-OFFICE-POPPLER-UNIFY 裁决2): mac/linux vendored pdftoppm/pdfinfo builds are
 * NOT self-contained (dynamically link libpoppler/libfreetype/libfontconfig/…),
 * so exec must point the loader at the vendored `lib/` dir. System PATH
 * binaries resolve their own libs via the system package manager — no env
 * injection there. Windows vendored builds are self-contained (dll alongside
 * the exe) — `libDir` is always undefined there (see `poppler.ts`).
 */
function popplerEnv(
  resolved: PopplerPaths,
): Record<string, string | undefined> | undefined {
  if (!resolved.libDir) return undefined
  const key = process.platform === 'darwin' ? 'DYLD_LIBRARY_PATH' : 'LD_LIBRARY_PATH'
  return { [key]: resolved.libDir }
}

/**
 * Get the number of pages in a PDF file using `pdfinfo` (from poppler-utils).
 * Returns `null` if pdfinfo is not available or if the page count cannot be determined.
 */
export async function getPDFPageCount(
  filePath: string,
  deps: PopplerCallDeps = {},
): Promise<number | null> {
  const resolved = await resolvePoppler(deps)
  if (!resolved) return null
  const { code, stdout } = await localExecBridge.execCommand({
    command: resolved.pdfinfo,
    args: [filePath],
    timeout: 10_000,
    env: popplerEnv(resolved),
  })
  if (code !== 0) {
    return null
  }
  const match = /^Pages:\s+(\d+)/m.exec(stdout)
  if (!match) {
    return null
  }
  const count = parseInt(match[1]!, 10)
  return isNaN(count) ? null : count
}

export type PDFExtractPagesResult = {
  type: 'parts'
  file: {
    filePath: string
    originalSize: number
    count: number
    outputDir: string
  }
}

// W-DOC-VISION-QUALITY-REMEDIATION PR-3a — pdftotext resolution, cached like
// `popplerResolution` (both reset by `resetPdftoppmCache`; only successful
// resolutions are cached, absent re-probes every call — same负缓存 fix as
// above). Kept separate because pdftotext is NOT guaranteed alongside
// pdftoppm: vendored archives currently ship only pdftoppm/pdfinfo (PR-10
// adds pdftotext to new archives), while PATH installs (brew/apt
// poppler-utils) ship all three.
let pdftotextResolution: string | undefined

async function resolvePdftotext(deps: PopplerCallDeps): Promise<string | null> {
  if (pdftotextResolution !== undefined) return pdftotextResolution
  const resolved = await resolvePoppler(deps)
  if (!resolved) {
    return null
  }
  if (resolved.pdftoppm === 'pdftoppm') {
    // PATH-resolved poppler: probe pdftotext the same way poppler.ts probes
    // pdftoppm (`-v` prints the version to stderr; old builds exit non-zero).
    try {
      const { code, stderr } = await localExecBridge.execCommand({
        command: 'pdftotext',
        args: ['-v'],
        timeout: 5_000,
      })
      if (code === 0 || stderr.length > 0) {
        pdftotextResolution = 'pdftotext'
        return pdftotextResolution
      }
    } catch {
      // fall through — absent is not cached
    }
    return null
  }
  // Vendored / CRABCODE_POPPLER_PATH layout: sibling binary in the same bin
  // dir as pdftoppm. Existence-checked because current vendored archives lack
  // it (missing → silent fallback to the page-image chain, never an error).
  // A missing sibling is NOT cached: "设置 → 重新下载" installs a new archive
  // that includes pdftotext, and the very next call must see it.
  // 路径形状的唯一真源在 poppler.ts —— 与安装器判定"这份 vendored 安装完不完
  // 整"用的是同一条,不允许两处各写一份平台后缀分支。
  const candidate = siblingPdftotextPath(dirname(resolved.pdftoppm))
  try {
    await access(candidate)
    pdftotextResolution = candidate
    return pdftotextResolution
  } catch {
    return null
  }
}

export type PDFTextExtraction = {
  /** Full extracted text (pdftotext -layout; pages separated by \f). */
  text: string
  /** Page count derived from form-feed separators in the output. */
  pageCount: number
}

/**
 * Whether a pdftotext extraction has a usable text layer, or the PDF is a
 * scan/image-only document that must fall back to the page-image chain.
 * Average non-whitespace chars per page against PDF_TEXT_LAYER_MIN_DENSITY.
 */
export function pdfTextMeetsDensityGate(
  extraction: PDFTextExtraction,
  minDensity: number = PDF_TEXT_LAYER_MIN_DENSITY,
): boolean {
  if (extraction.pageCount <= 0) return false
  const dense = extraction.text.replace(/\s/g, '').length
  return dense / extraction.pageCount >= minDensity
}

/**
 * J8（2026-07-04 审计 PR-10）— 混合 PDF 的**页级诚实标注**。整体密度门是
 * document-average：一份"9 页稠密文本 + 3 页整页扫描图"的混合 PDF 整卷通过，
 * 但那 3 页的内容静默丢失且无任何提示。本函数按 `\f` 分页计每页密度，给
 * 稀疏页在原位插入「[第 N 页为图像内容，未转写]」标注行。**不改整体门**
 * （整卷回退 vision-sidecar 会重新引入计费面）。
 */
export function annotateSparsePages(
  extraction: PDFTextExtraction,
  minDensity: number = PDF_TEXT_LAYER_MIN_DENSITY,
): string {
  const pages = extraction.text.split('\f')
  // pdftotext 在末页后有 trailing \f → 最后一段是空尾巴，不算页。
  const hasTrailer = pages.length > 1 && pages[pages.length - 1] === ''
  const effective = hasTrailer ? pages.slice(0, -1) : pages
  const annotated = effective.map((page, i) => {
    const dense = page.replace(/\s/g, '').length
    if (dense >= minDensity) return page
    const note = `[第 ${i + 1} 页为图像内容，未转写]`
    return page.trim().length === 0 ? `${note}\n` : `${page}\n${note}\n`
  })
  return annotated.join('\f') + (hasTrailer ? '\f' : '')
}

/**
 * Extract a PDF's text layer via pdftotext (W-DOC-VISION-QUALITY-REMEDIATION
 * PR-3a). High-fidelity path for text-only main models: real text instead of
 * a lossy vision-sidecar transcription of page images.
 *
 * `reason: 'unavailable'` when pdftotext cannot be resolved — callers degrade
 * SILENTLY to the existing page-image / document-block chains (裁决 D6:
 * vendored archives don't ship pdftotext yet; erroring would turn an
 * enhancement into a regression).
 */
export async function extractPDFText(
  filePath: string,
  options?: { firstPage?: number; lastPage?: number },
  deps: PopplerCallDeps = {},
): Promise<PDFResult<PDFTextExtraction>> {
  try {
    const fs = getFsImplementation()
    const stats = await fs.stat(filePath)
    if (stats.size === 0) {
      return {
        success: false,
        error: { reason: 'empty', message: `PDF file is empty: ${filePath}` },
      }
    }
    if (stats.size > PDF_MAX_EXTRACT_SIZE) {
      return {
        success: false,
        error: {
          reason: 'too_large',
          message: `PDF file exceeds maximum allowed size for text extraction (${formatFileSize(PDF_MAX_EXTRACT_SIZE)}).`,
        },
      }
    }

    const pdftotext = await resolvePdftotext(deps)
    if (!pdftotext) {
      return {
        success: false,
        error: {
          reason: 'unavailable',
          message: 'pdftotext is not available on this machine.',
        },
      }
    }
    const resolved = await resolvePoppler(deps)

    const args = ['-layout']
    if (options?.firstPage) {
      args.push('-f', String(options.firstPage))
    }
    if (options?.lastPage && options.lastPage !== Infinity) {
      args.push('-l', String(options.lastPage))
    }
    args.push(filePath, '-') // '-' → stdout
    const { code, stdout, stderr } = await localExecBridge.execCommand({
      command: pdftotext,
      args,
      timeout: 60_000,
      env: resolved ? popplerEnv(resolved) : undefined,
    })

    if (code !== 0) {
      if (/password/i.test(stderr)) {
        return {
          success: false,
          error: {
            reason: 'password_protected',
            // 文案即契约:terminalToolError.ts 签名 pdf_password_protected 按此字符串判据
            message: PDF_PASSWORD_PROTECTED_MESSAGE,
          },
        }
      }
      if (/damaged|corrupt|invalid/i.test(stderr)) {
        return {
          success: false,
          error: {
            reason: 'corrupted',
            // 文案即契约:terminalToolError.ts 签名 pdf_corrupted 按此字符串判据
            message: PDF_CORRUPTED_MESSAGE,
          },
        }
      }
      return {
        success: false,
        error: { reason: 'unknown', message: `pdftotext failed: ${stderr}` },
      }
    }

    // pdftotext separates pages with \f and emits a trailing \f after the
    // last page; count separators, not segments, so an empty scan page still
    // counts toward the density denominator.
    const separators = (stdout.match(/\f/g) ?? []).length
    const pageCount = Math.max(separators, 1)
    return { success: true, data: { text: stdout, pageCount } }
  } catch (e: unknown) {
    return {
      success: false,
      error: { reason: 'unknown', message: errorMessage(e) },
    }
  }
}

/**
 * Check whether poppler (`pdftoppm`) is resolvable — vendored, system PATH, or
 * `CRABCODE_POPPLER_PATH` override. A successful resolution is cached for the
 * lifetime of the process; an absent result is re-probed on every call so a
 * mid-session install becomes visible without a restart (see `resolvePoppler`).
 */
export async function isPdftoppmAvailable(
  deps: PopplerCallDeps = {},
): Promise<boolean> {
  return (await resolvePoppler(deps)) !== null
}

/**
 * Extract PDF pages as JPEG images using pdftoppm.
 * Produces page-01.jpg, page-02.jpg, etc. in an output directory.
 * This enables reading large PDFs and works with all API providers.
 *
 * @param filePath Path to the PDF file
 * @param options Optional page range (1-indexed, inclusive)
 */
export async function extractPDFPages(
  filePath: string,
  options?: { firstPage?: number; lastPage?: number },
  deps: PopplerCallDeps = {},
): Promise<PDFResult<PDFExtractPagesResult>> {
  try {
    const fs = getFsImplementation()
    const stats = await fs.stat(filePath)
    const originalSize = stats.size

    if (originalSize === 0) {
      return {
        success: false,
        error: { reason: 'empty', message: `PDF file is empty: ${filePath}` },
      }
    }

    if (originalSize > PDF_MAX_EXTRACT_SIZE) {
      return {
        success: false,
        error: {
          reason: 'too_large',
          message: `PDF file exceeds maximum allowed size for text extraction (${formatFileSize(PDF_MAX_EXTRACT_SIZE)}).`,
        },
      }
    }

    const resolved = await resolvePoppler(deps)
    if (!resolved) {
      return {
        success: false,
        error: {
          reason: 'unavailable',
          // 文案即契约:terminalToolError.ts 签名 poppler_missing 按此字符串判据,改文案必须同步
          message: POPPLER_MISSING_MESSAGE,
        },
      }
    }

    const uuid = randomUUID()
    const outputDir = join(getToolResultsDir(), `pdf-${uuid}`)
    await mkdir(outputDir, { recursive: true })

    // pdftoppm produces files like <prefix>-01.jpg, <prefix>-02.jpg, etc.
    const prefix = join(outputDir, 'page')
    const args = ['-jpeg', '-r', '100']
    if (options?.firstPage) {
      args.push('-f', String(options.firstPage))
    }
    if (options?.lastPage && options.lastPage !== Infinity) {
      args.push('-l', String(options.lastPage))
    }
    args.push(filePath, prefix)
    const { code, stderr } = await localExecBridge.execCommand({
      command: resolved.pdftoppm,
      args,
      timeout: 120_000,
      env: popplerEnv(resolved),
    })

    if (code !== 0) {
      if (/password/i.test(stderr)) {
        return {
          success: false,
          error: {
            reason: 'password_protected',
            // 文案即契约:terminalToolError.ts 签名 pdf_password_protected 按此字符串判据
            message: PDF_PASSWORD_PROTECTED_MESSAGE,
          },
        }
      }
      if (/damaged|corrupt|invalid/i.test(stderr)) {
        return {
          success: false,
          error: {
            reason: 'corrupted',
            // 文案即契约:terminalToolError.ts 签名 pdf_corrupted 按此字符串判据
            message: PDF_CORRUPTED_MESSAGE,
          },
        }
      }
      return {
        success: false,
        error: { reason: 'unknown', message: `pdftoppm failed: ${stderr}` },
      }
    }

    // Read generated image files and sort naturally
    const entries = await readdir(outputDir)
    const imageFiles = entries.filter(f => f.endsWith('.jpg')).sort()
    const pageCount = imageFiles.length

    if (pageCount === 0) {
      // 文案即契约:terminalToolError.ts 签名 pdf_corrupted 变体判据,改文案必须同步
      return {
        success: false,
        error: {
          reason: 'corrupted',
          message: PDFTOPPM_NO_PAGES_MESSAGE,
        },
      }
    }

    const count = imageFiles.length

    return {
      success: true,
      data: {
        type: 'parts',
        file: {
          filePath,
          originalSize,
          outputDir,
          count,
        },
      },
    }
  } catch (e: unknown) {
    return {
      success: false,
      error: {
        reason: 'unknown',
        message: errorMessage(e),
      },
    }
  }
}
