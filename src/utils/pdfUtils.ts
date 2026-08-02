import { getMainLoopModel } from './model/model.js'
import { getCachedModelCapabilities } from './model/modelCapabilities.js'

// Document extensions that are handled specially
export const DOCUMENT_EXTENSIONS = new Set(['pdf'])

/**
 * Parse a page range string into firstPage/lastPage numbers.
 * Supported formats:
 * - "5" → { firstPage: 5, lastPage: 5 }
 * - "1-10" → { firstPage: 1, lastPage: 10 }
 * - "3-" → { firstPage: 3, lastPage: Infinity }
 *
 * Returns null on invalid input (non-numeric, zero, inverted range).
 * Pages are 1-indexed.
 */
export function parsePDFPageRange(
  pages: string,
): { firstPage: number; lastPage: number } | null {
  const trimmed = pages.trim()
  if (!trimmed) {
    return null
  }

  // "N-" open-ended range
  if (trimmed.endsWith('-')) {
    const first = parseInt(trimmed.slice(0, -1), 10)
    if (isNaN(first) || first < 1) {
      return null
    }
    return { firstPage: first, lastPage: Infinity }
  }

  const dashIndex = trimmed.indexOf('-')
  if (dashIndex === -1) {
    // Single page: "5"
    const page = parseInt(trimmed, 10)
    if (isNaN(page) || page < 1) {
      return null
    }
    return { firstPage: page, lastPage: page }
  }

  // Range: "1-10"
  const first = parseInt(trimmed.slice(0, dashIndex), 10)
  const last = parseInt(trimmed.slice(dashIndex + 1), 10)
  if (isNaN(first) || isNaN(last) || first < 1 || last < 1 || last < first) {
    return null
  }
  return { firstPage: first, lastPage: last }
}

/**
 * Whether the gateway will attempt to parse a PDF *document block* for the
 * current model.
 *
 * NOTE (W-MULTIMODAL-INPUT P3.2): this checks only whether the SDK has a
 * capability RECORD for the model — it is NOT a true "this model supports PDF"
 * signal. There is no per-model `supports_pdf` field upstream (`@acosmi/
 * sdk-ts` `ModelCapabilities` exposes none), so genuine per-model PDF
 * capability cannot be determined locally and would need a gateway field
 * (cross-team).
 *
 * 2026-07-25 跨仓复核（把两条长期假设查成事实）：
 *   1. `supports_pdf` 在网关 monorepo **全库零出现**（managed model 结构 / SDK
 *      三语言类型 / admin web 类型三处均无）——不是"SDK 没透出来"，是平台侧确实
 *      没有。想按模型 gate PDF 必须先由网关补字段，客户端无从本地臆造（§1）。
 *   2. 本注释原先声称"网关对所有模型都会解析 document block"——**无代码支撑**：
 *      后端搜不到任何 document 内容块 / PDF 解析处理，走的是 Anthropic 兼容格式
 *      的**原样透传**，PDF 能不能被理解完全取决于上游模型自身。所以纯文本模型
 *      收到 document block 基本等于白传。真正的正路是本机 pdftotext 文本层直读
 *      （`pdf.ts::extractPDFText`）→ 页图链 → document block 兜底。
 *
 * Behavior is intentionally LEFT AS-IS rather than gated on image modality:
 * binding it to `inputModalities` would make `FileReadTool` throw "not
 * supported" for PDFs on text-only / unknown models (including the default
 * model). Models with no capability record fall back to the poppler
 * page-extraction path, whose page images now feed the capability-aware media
 * layer (see FileReadTool PDF branch).
 */
export function isPDFSupported(): boolean {
  const model = getMainLoopModel()
  const caps = getCachedModelCapabilities(model)
  return caps !== undefined
}

/**
 * Check if a file extension is a PDF document.
 * @param ext File extension (with or without leading dot)
 */
export function isPDFExtension(ext: string): boolean {
  const normalized = ext.startsWith('.') ? ext.slice(1) : ext
  return DOCUMENT_EXTENSIONS.has(normalized.toLowerCase())
}
