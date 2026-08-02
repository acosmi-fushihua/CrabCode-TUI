/**
 * XLSX pure-JavaScript fallback parser.
 *
 * xlsx 格式(ECMA-376 OOXML)是 ZIP 容器 + XML,公开完整规范,纯语言层可解析,
 * 不需要外部渲染引擎——这条路径专门服务"抽结构化数据"(列头/sheet 名/数据类型/
 * 样例行)场景,与 LibreOffice→PDF 路径服务的"需要版式视觉保真"场景是两种不同
 * 语义需求。只在 `resolveSofficePath()` 解析失败时被 FileReadTool 调用,不改变
 * soffice 可用时的既有行为。
 *
 * 解析库 = **exceljs**(4.x, MIT), 通过 `stream.xlsx.WorkbookReader` **流式**读取:
 * 每 sheet 只读到渲染所需的前若干行即停(见 `ROW_READ_CAP`), 从不物化整表 →
 * 根治超大表 OOM(R3 第一道)。再叠加输入体积护栏 `XLSX_FALLBACK_MAX_BYTES`,
 * 超限 fail-loud(R3 第二道), 不静默截断、不 OOM。
 *
 * 为何换掉 xlsx@0.18.5(R1): 那是 SheetJS 的 npm 终版, 带原型污染
 * (CVE-2023-30533) + ReDoS(CVE-2024-22363), `^0.18.5` 永远升不到补丁, 而本路径
 * 解析的正是外部/用户提供的 xlsx = 真实攻击面。exceljs 有活跃维护, 且其解析路径
 * 无同类未修复公告(仅 transitive uuid 的 buf-越界 moderate, exceljs 不传 buf,
 * 路径不可达)。
 *
 * 范围:仅 .xlsx(OOXML)。不覆盖旧版二进制 .xls(BIFF,非 ZIP+XML)、不覆盖
 * .ods(同类变体但未被事故触发,未纳入本次范围)。
 */

import { mkdtemp, stat, writeFile as writeFileAsync } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import * as path from 'node:path'
import * as ExcelJS from 'exceljs'

import type { MediaModalityClass } from '../model/imageModality.js'

/** 走纯 JS 兜底解析的扩展名:目前仅 xlsx(见上方范围说明)。 */
export const XLSX_FALLBACK_EXTENSIONS: ReadonlySet<string> = new Set(['xlsx'])

/** True if `ext`(带/不带点)应走纯 JS xlsx 兜底解析。 */
export function isXlsxFallbackExtension(ext: string): boolean {
  const normalized = ext.startsWith('.') ? ext.slice(1) : ext
  return XLSX_FALLBACK_EXTENSIONS.has(normalized.toLowerCase())
}

/**
 * W-DOC-VISION-QUALITY-REMEDIATION PR-2 (F2) — xlsx 路由按主模型模态分叉的
 * 纯决策函数(FileReadTool office 分支的第一道判定)。
 *
 * text_only:LibreOffice→PDF→页图链的终点是"数据图片的有损 sidecar 转写",
 * 数据保真严格劣于 exceljs 精确单元格值 → 数据抽取优先。
 * unknown:页图/文档块链依赖未经真机验证的网关行为,数据抽取无此依赖 → 同样
 * 优先(裁决 D2)。仅真视觉模型(supported)保留版式保真的视觉链。
 * 注意这是"优先"不是"独占":exceljs 失败(加密/损坏/超 50MB)仍落回 LibreOffice 链。
 */
export function shouldPreferXlsxDataExtraction(
  ext: string,
  modality: MediaModalityClass,
): boolean {
  return isXlsxFallbackExtension(ext) && modality !== 'supported'
}

/** 每个 sheet 预览的最大数据行数(超出如实标注截断,不静默丢)。 */
const MAX_PREVIEW_ROWS = 50
/** 单个 workbook 渲染的最大 sheet 数(超出如实标注截断,不静默丢)。 */
const MAX_SHEETS = 50
/** 用于列类型推断的采样行数(是 preview 的子集,不额外读取)。 */
const TYPE_SAMPLE_ROWS = 20

/**
 * 每 sheet 从流里读取的最大行数(含表头)。渲染只需表头 + 前 `MAX_PREVIEW_ROWS`
 * 数据行(类型采样是这前 50 行的子集),故读 `1 + MAX_PREVIEW_ROWS + 1`:多读的
 * 那 1 行仅用于探测"是否还有更多数据行"(→ 诚实截断标注),读到即停,永不物化整表。
 */
const ROW_READ_CAP = 1 + MAX_PREVIEW_ROWS + 1

/**
 * 内置 xlsx 兜底可接受的最大输入字节数。50MB:结构化数据抽取(列头/类型/样例)场景
 * 绰绰有余;超大文件应走 Bash/脚本流式处理而非塞进单次工具读。超限 fail-loud,
 * 不静默截断、不冒 OOM 风险。
 */
export const XLSX_FALLBACK_MAX_BYTES = 50 * 1024 * 1024

/** 一个 sheet 的归一化数据:`rows[0]` 是表头行,其余是数据行(均已 `normalizeCell`)。 */
export type SheetData = {
  name: string
  /** 含表头,最多 `ROW_READ_CAP` 行(超出即停,见 `truncatedRows`)。 */
  rows: unknown[][]
  /** 数据行数是否超过 `MAX_PREVIEW_ROWS`(即被行上限截断,还有未读的数据行)。 */
  truncatedRows: boolean
}

/** `readSheets` 的返回:已读 sheets + workbook 的 sheet 数是否超过 `MAX_SHEETS`。 */
export type ReadSheetsResult = {
  sheets: SheetData[]
  truncatedSheets: boolean
}

export type XlsxFallbackResult =
  | { success: true; mdPath: string }
  | { success: false; error: string }

export type XlsxFallbackDeps = {
  /**
   * Defaults to a streaming exceljs reader (`readSheetsWithExcelJS`). Injected
   * in tests to feed pre-normalized `SheetData` without touching any library.
   */
  readSheets?: (srcPath: string) => Promise<ReadSheetsResult>
  /** 2026-07-04 审计 PR-10 — 入口原因（决定文档标题的诚实措辞）：
   *  'text_model' = 文本主模型优先走内置结构化读取（LibreOffice 可能在装）；
   *  缺省 = LibreOffice 不可用的 fallback 入口。 */
  reason?: 'text_model' | 'engine_unavailable'
  /** exceljs 流式竞态的非流式降级读取器（测试注入面）。默认真
   *  readSheetsWithExcelJSNonStreaming。 */
  readSheetsNonStreaming?: (srcPath: string) => Promise<ReadSheetsResult>
  /** Defaults to `fs.stat(srcPath).size` — the byte-guard input. */
  statFileBytes?: (srcPath: string) => Promise<number>
  /** Defaults to mkdtemp under the OS temp dir. */
  makeOutDir?: () => Promise<string>
  /** Defaults to fs.writeFile with utf8 encoding. */
  writeFile?: (path: string, content: string) => Promise<void>
}

/**
 * Normalize one exceljs cell value into a primitive the renderer understands.
 * exceljs surfaces rich objects for formulas / rich text / hyperlinks / errors;
 * the renderer only wants the displayable scalar. Output is fed to the existing
 * `inferCellType`/`formatCell` (whose `unknown` input contract is unchanged).
 */
export function normalizeCell(v: ExcelJS.CellValue): unknown {
  if (v === null || v === undefined) return null
  if (typeof v === 'number' || typeof v === 'string' || typeof v === 'boolean') {
    return v
  }
  if (v instanceof Date) return v
  if (typeof v === 'object') {
    // Rich text → concatenated plain text.
    if ('richText' in v && Array.isArray(v.richText)) {
      return v.richText.map((rt) => rt.text).join('')
    }
    // Formula / shared formula → the computed result (empty when not cached).
    if ('formula' in v || 'sharedFormula' in v) {
      const result = (v as ExcelJS.CellFormulaValue).result
      if (result === undefined || result === null) return null
      if (typeof result === 'object' && 'error' in result) return result.error
      return result
    }
    // Hyperlink → its display text.
    if ('hyperlink' in v && 'text' in v) {
      return (v as ExcelJS.CellHyperlinkValue).text
    }
    // Error cell → its error code string.
    if ('error' in v) return (v as ExcelJS.CellErrorValue).error
  }
  return String(v)
}

/** exceljs stream `WorksheetReader` carries `name`/`id` at runtime (d.ts omits them). */
type StreamWorksheet = ExcelJS.stream.xlsx.WorksheetReader & {
  name?: string
  id?: number
}

/**
 * Default `readSheets`: stream the workbook with exceljs, reading at most
 * `ROW_READ_CAP` rows per sheet and at most `MAX_SHEETS` sheets — never
 * materializing the full grid. Returns normalized `SheetData[]` plus whether
 * the workbook had more than `MAX_SHEETS` sheets.
 */
// 2026-07-04 审计（PR-4 附带发现，Windows 实证升级版）— exceljs 流式读取器在
// Bun/Windows 上的读侧竞态抛 "undefined is not an object (evaluating
// 'this.model.sheets')"。**进程内 10 轮探针坐实：毒化是进程级持久态**——同进程
// 前几次流式读成功后，其后所有流式读终身同败（长寿 worker 进程读几次 xlsx 后
// 内置路径永久失效），因此同路径重试无效。策略：流式首选（内存最省），命中
// 竞态指纹即**降级非流式整读**（Workbook.xlsx.readFile，完全不同代码路径；
// 上游 XLSX_FALLBACK_MAX_BYTES=50MB 字节闸已兜内存，行/表 cap 读后同样施加）。
// 非指纹错误原样上抛不吞。根因归属 exceljs/Bun 上游。
const EXCELJS_MODEL_RACE_FINGERPRINT = /this\.model\.sheets|\(reading 'sheets'\)/

/** 非流式整读（竞态降级路径）。行成形与流式路径逐点对齐：row.values 1 基、
 *  slice(1)、normalizeCell；MAX_SHEETS / ROW_READ_CAP 同样施加。 */
async function readSheetsWithExcelJSNonStreaming(
  srcPath: string,
): Promise<ReadSheetsResult> {
  const wb = new ExcelJS.Workbook()
  await wb.xlsx.readFile(srcPath)
  const sheets: SheetData[] = []
  let truncatedSheets = false
  for (const worksheet of wb.worksheets) {
    if (sheets.length >= MAX_SHEETS) {
      truncatedSheets = true
      break
    }
    const rows: unknown[][] = []
    let truncatedRows = false
    worksheet.eachRow({ includeEmpty: false }, (row) => {
      if (rows.length >= ROW_READ_CAP) {
        truncatedRows = true
        return
      }
      const values = Array.isArray(row.values)
        ? (row.values as ExcelJS.CellValue[])
        : []
      rows.push(values.slice(1).map(normalizeCell))
    })
    sheets.push({
      name: worksheet.name ?? `Sheet${sheets.length + 1}`,
      rows,
      truncatedRows,
    })
  }
  return { sheets, truncatedSheets }
}

async function readSheetsWithRaceFallback(
  primary: (srcPath: string) => Promise<ReadSheetsResult>,
  fallback: (srcPath: string) => Promise<ReadSheetsResult>,
  srcPath: string,
): Promise<ReadSheetsResult> {
  try {
    return await primary(srcPath)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    if (!EXCELJS_MODEL_RACE_FINGERPRINT.test(msg)) throw e
    return await fallback(srcPath)
  }
}

async function readSheetsWithExcelJS(srcPath: string): Promise<ReadSheetsResult> {
  const reader = new ExcelJS.stream.xlsx.WorkbookReader(srcPath, {
    worksheets: 'emit',
    sharedStrings: 'cache',
    hyperlinks: 'ignore',
    // 'cache' (not 'ignore'): date cells are recognized via their numFmt style —
    // without the style table, exceljs surfaces dates as raw Excel serial
    // numbers (e.g. 45355) instead of Date objects, losing the date semantics
    // this "extract structured data" path exists to convey. The style table is
    // a workbook-level deduped pool (tens–hundreds of entries, independent of
    // row count) — NOT an OOM source; row/byte caps already bound cell data.
    styles: 'cache',
    entries: 'ignore',
  })

  const sheets: SheetData[] = []
  let truncatedSheets = false

  for await (const worksheetRaw of reader) {
    if (sheets.length >= MAX_SHEETS) {
      truncatedSheets = true
      break
    }
    const worksheet = worksheetRaw as StreamWorksheet
    const rows: unknown[][] = []
    let truncatedRows = false
    for await (const row of worksheet) {
      if (rows.length >= ROW_READ_CAP) {
        truncatedRows = true
        break
      }
      // row.values is 1-indexed (index 0 is a hole); slice(1) → column array.
      const values = Array.isArray(row.values)
        ? (row.values as ExcelJS.CellValue[])
        : []
      rows.push(values.slice(1).map(normalizeCell))
    }
    sheets.push({
      name: worksheet.name ?? `Sheet${sheets.length + 1}`,
      rows,
      truncatedRows,
    })
  }

  return { sheets, truncatedSheets }
}

function inferCellType(value: unknown): string {
  if (value === null || value === undefined || value === '') return 'empty'
  if (typeof value === 'number') return 'number'
  if (typeof value === 'boolean') return 'boolean'
  if (value instanceof Date) return 'date'
  return 'string'
}

function formatCell(value: unknown): string {
  if (value === null || value === undefined) return ''
  if (value instanceof Date) return value.toISOString()
  return String(value).replace(/\|/g, '\\|').replace(/\r?\n/g, ' ')
}

function renderSheet(
  sheetName: string,
  rows: unknown[][],
  truncatedRows: boolean,
): string {
  if (rows.length === 0) {
    return `## Sheet: ${sheetName} (empty)\n`
  }

  const header = rows[0] ?? []
  const dataRows = rows.slice(1)
  // 2026-07-04 审计 PR-10:列数 = max(表头列数, 数据行最大列数)——真实表格常有
  // 表头行短于数据行（无标题的溢出列），只按表头计会静默截掉超宽列的数据。
  const colCount = Math.max(
    header.length,
    ...dataRows.map((r) => r.length),
    0,
  )
  // 超出表头的列（数据行比表头宽的溢出列）以「列N」占位表头；表头内的空单元
  // 保持既有 `(col N)` / 空格惯例不变。
  const overflowLabel = (c: number): string | null =>
    c >= header.length ? `列${c + 1}` : null
  const preview = dataRows.slice(0, MAX_PREVIEW_ROWS)
  const lines: string[] = []
  // When the row stream was capped, the exact total is unknown — report the
  // shown count with a "+" (at least this many, more exist) rather than a
  // fabricated precise total. When not capped, the count is exact and the
  // wording is unchanged from the original.
  const rowCountLabel = truncatedRows
    ? `${preview.length}+ data rows`
    : `${dataRows.length} data rows`
  lines.push(`## Sheet: ${sheetName} (${colCount} columns × ${rowCountLabel})`)
  lines.push('')

  // Column type inference from a sample of data rows — types are informational
  // (helps a model decide how to treat a column), not a strict schema.
  const sampleForType = dataRows.slice(0, TYPE_SAMPLE_ROWS)
  lines.push('| Column | Inferred type |')
  lines.push('|---|---|')
  for (let c = 0; c < colCount; c++) {
    const types = new Set<string>()
    for (const row of sampleForType) {
      types.add(inferCellType(row[c]))
    }
    types.delete('empty')
    const typeLabel = types.size === 0 ? 'empty' : [...types].join(' | ')
    const colLabel =
      formatCell(header[c]) || overflowLabel(c) || `(col ${c + 1})`
    lines.push(`| ${colLabel} | ${typeLabel} |`)
  }
  lines.push('')

  if (truncatedRows) {
    lines.push(
      `### Preview (first ${preview.length} data rows; more rows exist but were not scanned — use Bash with a script for the full dataset)`,
    )
  } else {
    lines.push(
      `### Preview (first ${preview.length} of ${dataRows.length} data rows)`,
    )
  }
  lines.push('')
  lines.push(
    `| ${Array.from({ length: colCount }, (_, c) => formatCell(header[c]) || overflowLabel(c) || ' ').join(' | ')} |`,
  )
  lines.push(`| ${Array.from({ length: colCount }, () => '---').join(' | ')} |`)
  for (const row of preview) {
    lines.push(
      `| ${Array.from({ length: colCount }, (_, c) => formatCell(row[c])).join(' | ')} |`,
    )
  }
  if (!truncatedRows && dataRows.length > preview.length) {
    lines.push('')
    lines.push(
      `_(${dataRows.length - preview.length} more data rows not shown — use Bash with a script for the full dataset.)_`,
    )
  }
  lines.push('')
  return lines.join('\n')
}

/**
 * Parse an .xlsx file entirely in-process (no external binary) and render a
 * markdown summary (sheet names, column headers, inferred types, row-count
 * preview) to a temp file. Returns the produced `.md` path; the caller
 * (FileReadTool) re-enters the normal text path the same way the pandoc
 * branch does (token guards, offset/limit apply downstream, unchanged).
 */
export async function convertXlsxToMarkdown(
  srcPath: string,
  deps: XlsxFallbackDeps = {},
): Promise<XlsxFallbackResult> {
  const readSheets = deps.readSheets ?? readSheetsWithExcelJS
  const statFileBytes =
    deps.statFileBytes ?? (async (p: string) => (await stat(p)).size)
  const makeOutDir =
    deps.makeOutDir ?? (() => mkdtemp(path.join(tmpdir(), 'crabcode-xlsx-')))
  const write = deps.writeFile ?? ((p: string, c: string) => writeFileAsync(p, c, 'utf8'))

  // Byte guard (R3 second line): fail loud on oversized input rather than risk
  // OOM / pathological parse time. A stat failure is non-fatal — fall through
  // and let the streaming read surface the real read error.
  try {
    const bytes = await statFileBytes(srcPath)
    if (bytes > XLSX_FALLBACK_MAX_BYTES) {
      const mb = (bytes / (1024 * 1024)).toFixed(1)
      const limitMb = Math.round(XLSX_FALLBACK_MAX_BYTES / (1024 * 1024))
      return {
        success: false,
        error:
          `"${path.basename(srcPath)}" (${mb} MB) exceeds the built-in xlsx ` +
          `fallback size limit (${limitMb} MB) — use Bash with a script to ` +
          `process the full spreadsheet.`,
      }
    }
  } catch {
    // ignore — the read below will report an actionable error if the file is bad
  }

  let result: ReadSheetsResult
  try {
    result = await readSheetsWithRaceFallback(
      readSheets,
      deps.readSheetsNonStreaming ?? readSheetsWithExcelJSNonStreaming,
      srcPath,
    )
  } catch (e) {
    return {
      success: false,
      error:
        `built-in xlsx parser failed to read "${path.basename(srcPath)}": ` +
        `${e instanceof Error ? e.message : String(e)}`,
    }
  }

  const { sheets, truncatedSheets } = result
  if (sheets.length === 0) {
    return {
      success: false,
      error: `"${path.basename(srcPath)}" has no sheets (empty or corrupt workbook).`,
    }
  }

  // 2026-07-04 审计 PR-10:标题按入口分叉——text_model 入口（文本主模型优先走
  // 内置结构化读取,LibreOffice 可能装着）沿用"(LibreOffice unavailable)"是撒谎。
  const reasonLabel =
    deps.reason === 'text_model'
      ? 'text-only main model'
      : 'LibreOffice unavailable'
  const sections: string[] = [
    `# ${path.basename(srcPath)} — parsed via built-in xlsx reader (${reasonLabel})`,
    '',
  ]
  for (const sheet of sheets) {
    sections.push(renderSheet(sheet.name, sheet.rows, sheet.truncatedRows))
  }
  if (truncatedSheets) {
    sections.push(
      `_(more sheets not shown — this workbook has more than ${MAX_SHEETS} sheets.)_`,
    )
  }

  const outDir = await makeOutDir()
  const base = path.basename(srcPath, path.extname(srcPath))
  const mdPath = path.join(outDir, `${base}.md`)
  await write(mdPath, sections.join('\n'))
  return { success: true, mdPath }
}
