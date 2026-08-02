
import type { Base64ImageSource } from '../../types/api-types.js'
import type { ToolArtifactCandidate } from '../../types/toolArtifact.js'
import {
  mkdtemp,
  open as openFileAsync,
  readdir,
  readFile as readFileAsync,
  writeFile as writeFileAsync,
} from 'fs/promises'
import { tmpdir } from 'os'
import * as path from 'path'
import { posix, win32 } from 'path'
import { z } from 'zod/v4'
import {
  DOCX_EMBEDDED_IMAGE_MAX_COUNT,
  PDF_AT_MENTION_INLINE_THRESHOLD,
  PDF_EXTRACT_SIZE_THRESHOLD,
  PDF_MAX_PAGES_PER_READ,
} from '../../constants/apiLimits.js'
import { hasBinaryExtension, isBinaryContent } from '../../constants/files.js'
import { memoryFreshnessNote } from '../../memdir/memoryAge.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../../services/analytics/growthbook.js'
import { logEvent } from '../../services/analytics/index.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  getFileExtensionForAnalytics,
} from '../../services/analytics/metadata.js'
import {
  countTokensWithAPI,
  roughTokenCountEstimationForFileType,
} from '../../services/tokenEstimation.js'
import {
  activateConditionalSkillsForPaths,
  addSkillDirectories,
  discoverSkillDirsForPaths,
} from '../../skills/loadSkillsDir.js'
import type { ToolUseContext } from '../../Tool.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { getCwd } from '../../utils/cwd.js'
import { getCrabCodeConfigHomeDir, isEnvTruthy } from '../../utils/envUtils.js'
import { getErrnoCode, isENOENT } from '../../utils/errors.js'
import {
  addLineNumbers,
  FILE_NOT_FOUND_CWD_NOTE,
  findSimilarFile,
  getFileModificationTimeAsync,
  suggestPathUnderCwd,
} from '../../utils/file.js'
import { logFileOperation } from '../../utils/fileOperationAnalytics.js'
import { formatFileSize } from '../../utils/format.js'
import { getFsImplementation } from '../../utils/fsOperations.js'
import {
  compressImageBufferWithTokenLimit,
  createImageMetadataText,
  detectImageFormatFromBuffer,
  type ImageDimensions,
  ImageResizeError,
  maybeResizeAndDownsampleImageBuffer,
} from '../../utils/imageResizer.js'
import { persistBinaryContent } from '../../utils/mcpOutputStorage.js'
import {
  isTranscodeImageExtension,
  TRANSCODE_IMAGE_MAX_BYTES,
  transcodeImageToPng,
  transcodeImageToPngWithInfo,
} from '../../utils/imageTranscode.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { logError } from '../../utils/log.js'
import { isAutoMemFile } from '../../utils/memoryFileDetection.js'
import { createUserMessage } from '../../utils/messages.js'
import { mainLoopModelImageModality } from '../../utils/model/imageModality.js'
import { getMainLoopModel } from '../../utils/model/model.js'
import { getCachedCapabilityWithDefaultFallback } from '../../utils/model/modelCapabilities.js'
import {
  mapNotebookCellsToToolResult,
  readNotebook,
} from '../../utils/notebook.js'
import { expandPath } from '../../utils/path.js'
import {
  extractPDFPages,
  annotateSparsePages,
  extractPDFText,
  getPDFPageCount,
  PDF_MODEL_UNSUPPORTED_MESSAGE_PREFIX,
  pdfTextMeetsDensityGate,
  readPDF,
} from '../../utils/pdf.js'
import {
  convertOfficeToPdf,
  isOfficeExtension,
  officeEngineMissingMessage,
  resolveSofficePath,
} from '../../utils/officeParse/libreoffice.js'
import {
  convertDocxToMarkdown,
  isPandocTextExtension,
  resolvePandocPath,
} from '../../utils/officeParse/pandoc.js'
import {
  convertXlsxToMarkdown,
  isXlsxFallbackExtension,
  shouldPreferXlsxDataExtraction,
} from '../../utils/officeParse/xlsxFallback.js'
import {
  isPDFExtension,
  isPDFSupported,
  parsePDFPageRange,
} from '../../utils/pdfUtils.js'
import {
  checkReadPermissionForTool,
  matchingRuleForInput,
} from '../../utils/permissions/filesystem.js'
import type { PermissionDecision } from '../../utils/permissions/PermissionResult.js'
import { matchWildcardPattern } from '../../utils/permissions/shellRuleMatching.js'
import { readFileInRange } from '../../utils/readFileInRange.js'
import { semanticNumber } from '../../utils/semanticNumber.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import { BASH_TOOL_NAME } from '../BashTool/toolName.js'
import { getDefaultFileReadingLimits } from './limits.js'
import {
  DESCRIPTION,
  FILE_READ_TOOL_NAME,
  FILE_UNCHANGED_STUB,
  LINE_FORMAT_INSTRUCTION,
  OFFSET_INSTRUCTION_DEFAULT,
  OFFSET_INSTRUCTION_TARGETED,
  renderPromptTemplate,
} from './prompt.js'
import {
  fileReadToolUseSummary as getToolUseSummary,
  fileReadUserFacingName as userFacingName,
} from '../toolMetadata.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'

/**
 * Build image content blocks from a directory of extracted PDF page JPEGs.
 * Shared by the explicit `pages` path and the size/capability-driven
 * extraction path (W-MULTIMODAL-INPUT P3). Each page is resized/downsampled to
 * honor the API image limits. Returns [] when the directory has no .jpg pages.
 */
async function buildPDFPageImageBlocks(outputDir: string): Promise<
  Array<{
    type: 'image'
    source: {
      type: 'base64'
      media_type: Base64ImageSource['media_type']
      data: string
    }
  }>
> {
  const entries = await readdir(outputDir)
  const imageFiles = entries.filter(f => f.endsWith('.jpg')).sort()
  return Promise.all(
    imageFiles.map(async f => {
      const imgPath = path.join(outputDir, f)
      const imgBuffer = await readFileAsync(imgPath)
      const resized = await maybeResizeAndDownsampleImageBuffer(
        imgBuffer,
        imgBuffer.length,
        'jpeg',
      )
      return {
        type: 'image' as const,
        source: {
          type: 'base64' as const,
          media_type:
            `image/${resized.mediaType}` as Base64ImageSource['media_type'],
          data: resized.buffer.toString('base64'),
        },
      }
    }),
  )
}

/**
 * pandoc's `--extract-media` (W-DOCX-EMBEDDED-IMAGE) rewrites docx/odt image
 * references to absolute filesystem paths under its temp `mediaDir` — left
 * as-is, the model would see a raw local path (a dead-end it can't act on,
 * and a needless leak of local temp-dir structure) instead of the actual
 * image. This scans the pandoc-produced markdown for those references,
 * replaces each with an honest inline marker, and returns the
 * ordered/deduped list of image files to attach as real image blocks.
 *
 * Per the "no silent degradation" finding (SoT 复合审计报告 M1): every
 * reference gets a marker that reflects what actually happens to it —
 * attached (`[Image N]`), not attached because the format can't be rendered
 * inline (emf/wmf/bmp/tiff/svg/...), or not attached because the document
 * exceeds `DOCX_EMBEDDED_IMAGE_MAX_COUNT`. None of these collapse into a
 * silent drop.
 *
 * pandoc's extract-media subfolder name varies by source format (`media/`
 * for docx, `Pictures/` for odt, verified against real pandoc output) —
 * matched here by absolute path prefix, not a hardcoded subfolder name.
 *
 * Image attachment is document-wide, not scoped to the caller's offset/limit
 * line range — docx has no page concept to align a line range against, and
 * doing so would add complexity disproportionate to the benefit.
 */
export function sanitizePandocMediaReferences(
  markdown: string,
  mediaDir: string,
): {
  text: string
  images: Array<{ absPath: string }>
  unsupportedCount: number
  truncatedCount: number
} {
  const attachedIndexByPath = new Map<string, number>()
  const images: Array<{ absPath: string }> = []
  const unsupportedPaths = new Set<string>()
  const truncatedPaths = new Set<string>()

  // 分隔符 + URL 编码归一化后再做前缀判定:Windows `mkdtemp` 产**反斜杠** mediaDir,
  // 而 pandoc `--extract-media` 重写引用惯用**正斜杠**(含空格路径还可能 `%20` 编码)
  // → 不归一化则 `startsWith` 在 Windows 恒 false → 引用既不清洗也不附图,原始绝对
  // temp 路径泄露给模型。仅影响 Windows;darwin/linux 下 decodePath 幂等、无反斜杠,
  // 行为逐字不变。
  const decodePath = (p: string): string => {
    try {
      return decodeURIComponent(p)
    } catch {
      return p
    }
  }
  const normForCompare = (p: string): string => decodePath(p).replace(/\\/g, '/')
  const normMediaDir = normForCompare(mediaDir)
  const isOurMedia = (src: string): boolean =>
    normForCompare(src).startsWith(normMediaDir)

  const describe = (absPath: string, alt: string | undefined): string => {
    const ext = path.extname(absPath).slice(1).toLowerCase()
    // PR-5: bmp/tiff embeds are attachable now (transcoded to PNG in
    // buildDocxEmbeddedImageBlocks) — keeps this layer consistent with the
    // direct-Read image chain (审计 F4 点名的两层不一致).
    if (!IMAGE_EXTENSIONS.has(ext) && !isTranscodeImageExtension(ext)) {
      unsupportedPaths.add(absPath)
      return `[embedded image: unsupported format ".${ext || '?'}", not attached]`
    }
    const existingIndex = attachedIndexByPath.get(absPath)
    if (existingIndex !== undefined) {
      return alt ? `[Image ${existingIndex}: ${alt}]` : `[Image ${existingIndex}]`
    }
    if (attachedIndexByPath.size >= DOCX_EMBEDDED_IMAGE_MAX_COUNT) {
      truncatedPaths.add(absPath)
      return `[embedded image: not attached — document exceeds the ${DOCX_EMBEDDED_IMAGE_MAX_COUNT}-image limit per read]`
    }
    const index = attachedIndexByPath.size + 1
    attachedIndexByPath.set(absPath, index)
    images.push({ absPath })
    return alt ? `[Image ${index}: ${alt}]` : `[Image ${index}]`
  }

  // HTML form: pandoc's gfm writer emits `<img src="..." .../>` (optionally
  // wrapped in <figure>/<figcaption>) whenever the source carries extra
  // attrs (width/height/caption) — the common case reading FROM docx/odt.
  let text = markdown.replace(/<img\s+([^>]*?)\/?>/g, (match, attrs: string) => {
    const srcMatch = attrs.match(/\bsrc="([^"]*)"/)
    if (!srcMatch || !isOurMedia(srcMatch[1])) return match
    const altMatch = attrs.match(/\balt="([^"]*)"/)
    // Pass the decoded path so downstream reads/keys use the real file path
    // (not a %20-encoded form that fs can't open).
    return describe(decodePath(srcMatch[1]), altMatch?.[1] || undefined)
  })

  // Markdown form fallback: `![alt](path)`, in case a source/pandoc version
  // emits plain image syntax instead of HTML for media in our extract dir.
  // Two src forms: `![alt](path)` (no spaces) and `![alt](<path with space>)`
  // (angle-bracket form pandoc uses for paths containing spaces, e.g. a Windows
  // `C:\Users\John Doe\...` temp dir). Both must be cleaned, else a space-bearing
  // Windows temp path slips through the plain-path branch and leaks.
  text = text.replace(
    /!\[([^\]]*)\]\(\s*(?:<([^>]*)>|([^)\s]+))(?:\s+"[^"]*")?\s*\)/g,
    (
      match,
      alt: string,
      bracketed: string | undefined,
      plain: string | undefined,
    ) => {
      const src = bracketed ?? plain ?? ''
      if (!isOurMedia(src)) return match
      return describe(decodePath(src), alt || undefined)
    },
  )

  // Drop the now-redundant <figure>/<figcaption> wrapper tags (the img they
  // wrapped is already replaced above) — the caption text itself, inside
  // <figcaption>, is left in place.
  text = text
    .replace(/<figure>\s*/g, '')
    .replace(/\s*<\/figure>/g, '')
    .replace(/<figcaption[^>]*>\s*/g, '')
    .replace(/\s*<\/figcaption>/g, '')

  return {
    text,
    images,
    unsupportedCount: unsupportedPaths.size,
    truncatedCount: truncatedPaths.size,
  }
}

/**
 * Reads the (already deduped/capped) extracted docx/odt image files and
 * builds image content blocks, same shape and same resize treatment as
 * `buildPDFPageImageBlocks` — attached via `newMessages` so they flow
 * through `applyMediaCapabilityPolicy`'s vision-sidecar degradation exactly
 * like PDF page images already do (queryModel.ts:934, "PDF page-images are
 * image blocks and so flow through this same policy" — embedded docx images
 * are no different).
 */
async function buildDocxEmbeddedImageBlocks(
  images: Array<{ absPath: string }>,
): Promise<
  Array<{
    type: 'image'
    source: {
      type: 'base64'
      media_type: Base64ImageSource['media_type']
      data: string
    }
  }>
> {
  return Promise.all(
    images.map(async ({ absPath }) => {
      let buffer: Buffer = await readFileAsync(absPath)
      let ext = path.extname(absPath).slice(1).toLowerCase()
      if (isTranscodeImageExtension(ext)) {
        // PR-5: same pure-JS transcode as the direct-Read chain; a failure
        // here propagates as the transcoder's honest error (caught by the
        // best-effort try around embedded-image extraction).
        buffer = transcodeImageToPng(buffer, ext)
        ext = 'png'
      }
      const resized = await maybeResizeAndDownsampleImageBuffer(
        buffer,
        buffer.length,
        ext,
      )
      return {
        type: 'image' as const,
        source: {
          type: 'base64' as const,
          media_type:
            `image/${resized.mediaType}` as Base64ImageSource['media_type'],
          data: resized.buffer.toString('base64'),
        },
      }
    }),
  )
}

// Device files that would hang the process: infinite output or blocking input.
// Checked by path only (no I/O). Safe devices like /dev/null are intentionally omitted.
const BLOCKED_DEVICE_PATHS = new Set([
  // Infinite output — never reach EOF
  '/dev/zero',
  '/dev/random',
  '/dev/urandom',
  '/dev/full',
  // Blocks waiting for input
  '/dev/stdin',
  '/dev/tty',
  '/dev/console',
  // Nonsensical to read
  '/dev/stdout',
  '/dev/stderr',
  // fd aliases for stdin/stdout/stderr
  '/dev/fd/0',
  '/dev/fd/1',
  '/dev/fd/2',
])

function isBlockedDevicePath(filePath: string): boolean {
  if (BLOCKED_DEVICE_PATHS.has(filePath)) return true
  // /proc/self/fd/0-2 and /proc/<pid>/fd/0-2 are Linux aliases for stdio
  if (
    filePath.startsWith('/proc/') &&
    (filePath.endsWith('/fd/0') ||
      filePath.endsWith('/fd/1') ||
      filePath.endsWith('/fd/2'))
  )
    return true
  return false
}

/** Sniff window for the text-branch binary gate (PR-4) — matches files.ts's
 * BINARY_CHECK_SIZE so isBinaryContent sees its full sample. */
const BINARY_SNIFF_PREFIX_BYTES = 8192

/**
 * Read the first raw bytes of a file for content sniffing (PR-4). Raw, NOT
 * UTF-8 decoded — decoding first would mangle exactly the bytes that mark a
 * file as binary. Returns null when the prefix cannot be read (the main read
 * path will surface its own error).
 */
async function readRawFilePrefix(filePath: string): Promise<Buffer | null> {
  try {
    const handle = await openFileAsync(filePath, 'r')
    try {
      const buf = Buffer.alloc(BINARY_SNIFF_PREFIX_BYTES)
      const { bytesRead } = await handle.read(buf, 0, BINARY_SNIFF_PREFIX_BYTES, 0)
      return buf.subarray(0, bytesRead)
    } finally {
      await handle.close()
    }
  } catch {
    return null
  }
}

// Narrow no-break space (U+202F) used by some macOS versions in screenshot filenames
const THIN_SPACE = String.fromCharCode(8239)

/**
 * Resolves macOS screenshot paths that may have different space characters.
 * macOS uses either regular space or thin space (U+202F) before AM/PM in screenshot
 * filenames depending on the macOS version. This function tries the alternate space
 * character if the file doesn't exist with the given path.
 *
 * @param filePath - The normalized file path to resolve
 * @returns The path to the actual file on disk (may differ in space character)
 */
/**
 * For macOS screenshot paths with AM/PM, the space before AM/PM may be a
 * regular space or a thin space depending on the macOS version.  Returns
 * the alternate path to try if the original doesn't exist, or undefined.
 */
function getAlternateScreenshotPath(filePath: string): string | undefined {
  const filename = path.basename(filePath)
  const amPmPattern = /^(.+)([ \u202F])(AM|PM)(\.png)$/
  const match = filename.match(amPmPattern)
  if (!match) return undefined

  const currentSpace = match[2]
  const alternateSpace = currentSpace === ' ' ? THIN_SPACE : ' '
  return filePath.replace(
    `${currentSpace}${match[3]}${match[4]}`,
    `${alternateSpace}${match[3]}${match[4]}`,
  )
}

// File read listeners - allows other services to be notified when files are read
type FileReadListener = (filePath: string, content: string) => void
const fileReadListeners: FileReadListener[] = []

export function registerFileReadListener(
  listener: FileReadListener,
): () => void {
  fileReadListeners.push(listener)
  return () => {
    const i = fileReadListeners.indexOf(listener)
    if (i >= 0) fileReadListeners.splice(i, 1)
  }
}

export class MaxFileReadTokenExceededError extends Error {
  constructor(
    public tokenCount: number,
    public maxTokens: number,
  ) {
    super(
      `File content (${tokenCount} tokens) exceeds maximum allowed tokens (${maxTokens}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.`,
    )
    this.name = 'MaxFileReadTokenExceededError'
  }
}

// Common image extensions
const IMAGE_EXTENSIONS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp'])

/**
 * Detects if a file path is a session-related file for analytics logging.
 * Only matches files within the CrabCode config directory (e.g., ~/.crabcode).
 * Returns the type of session file or null if not a session file.
 */
function detectSessionFileType(
  filePath: string,
): 'session_memory' | 'session_transcript' | null {
  const configDir = getCrabCodeConfigHomeDir()

  // Only match files within the CrabCode config directory
  if (!filePath.startsWith(configDir)) {
    return null
  }

  // Normalize path to use forward slashes for consistent matching across platforms
  const normalizedPath = filePath.split(win32.sep).join(posix.sep)

  // Session memory files: ~/.crabcode/session-memory/*.md (including summary.md)
  if (
    normalizedPath.includes('/session-memory/') &&
    normalizedPath.endsWith('.md')
  ) {
    return 'session_memory'
  }

  // Session JSONL transcript files: ~/.crabcode/projects/*/*.jsonl
  if (
    normalizedPath.includes('/projects/') &&
    normalizedPath.endsWith('.jsonl')
  ) {
    return 'session_transcript'
  }

  return null
}

const inputSchema = lazySchema(() =>
  z.strictObject({
    file_path: z.string().describe('The absolute path to the file to read'),
    offset: semanticNumber(z.number().int().nonnegative().optional()).describe(
      'The line number to start reading from. Only provide if the file is too large to read at once',
    ),
    limit: semanticNumber(z.number().int().positive().optional()).describe(
      'The number of lines to read. Only provide if the file is too large to read at once.',
    ),
    pages: z
      .string()
      .optional()
      .describe(
        `Page range for PDF files (e.g., "1-5", "3", "10-20"). Only applicable to PDF files. Maximum ${PDF_MAX_PAGES_PER_READ} pages per request.`,
      ),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

export type Input = z.infer<InputSchema>

const outputSchema = lazySchema(() => {
  // Define the media types supported for images
  const imageMediaTypes = z.enum([
    'image/jpeg',
    'image/png',
    'image/gif',
    'image/webp',
  ])

  return z.discriminatedUnion('type', [
    z.object({
      type: z.literal('text'),
      file: z.object({
        filePath: z.string().describe('The path to the file that was read'),
        content: z.string().describe('The content of the file'),
        numLines: z
          .number()
          .describe('Number of lines in the returned content'),
        startLine: z.number().describe('The starting line number'),
        totalLines: z.number().describe('Total number of lines in the file'),
      }),
    }),
    z.object({
      type: z.literal('image'),
      file: z.object({
        base64: z.string().describe('Base64-encoded image data'),
        type: imageMediaTypes.describe('The MIME type of the image'),
        originalSize: z.number().describe('Original file size in bytes'),
        dimensions: z
          .object({
            originalWidth: z
              .number()
              .optional()
              .describe('Original image width in pixels'),
            originalHeight: z
              .number()
              .optional()
              .describe('Original image height in pixels'),
            displayWidth: z
              .number()
              .optional()
              .describe('Displayed image width in pixels (after resizing)'),
            displayHeight: z
              .number()
              .optional()
              .describe('Displayed image height in pixels (after resizing)'),
          })
          .optional()
          .describe('Image dimension info for coordinate mapping'),
      }),
    }),
    z.object({
      type: z.literal('notebook'),
      file: z.object({
        filePath: z.string().describe('The path to the notebook file'),
        cells: z.array(z.any()).describe('Array of notebook cells'),
      }),
    }),
    z.object({
      type: z.literal('pdf'),
      file: z.object({
        filePath: z.string().describe('The path to the PDF file'),
        base64: z.string().describe('Base64-encoded PDF data'),
        originalSize: z.number().describe('Original file size in bytes'),
      }),
    }),
    z.object({
      type: z.literal('parts'),
      file: z.object({
        filePath: z.string().describe('The path to the PDF file'),
        originalSize: z.number().describe('Original file size in bytes'),
        count: z.number().describe('Number of pages extracted'),
        outputDir: z
          .string()
          .describe('Directory containing extracted page images'),
      }),
    }),
    z.object({
      type: z.literal('file_unchanged'),
      file: z.object({
        filePath: z.string().describe('The path to the file'),
      }),
    }),
  ])
})
type OutputSchema = ReturnType<typeof outputSchema>

export type Output = z.infer<OutputSchema>

export const FileReadTool = buildTool({
  name: FILE_READ_TOOL_NAME,
  searchHint: 'read files, images, PDFs, notebooks',
  // Output is bounded by maxTokens (validateContentTokens). Persisting to a
  // file the model reads back with Read is circular — never persist.
  maxResultSizeChars: Infinity,
  strict: true,
  async description() {
    return DESCRIPTION
  },
  async prompt() {
    const limits = getDefaultFileReadingLimits()
    const maxSizeInstruction = limits.includeMaxSizeInPrompt
      ? `. Files larger than ${formatFileSize(limits.maxSizeBytes)} will return an error; use offset and limit for larger files`
      : ''
    const offsetInstruction = limits.targetedRangeNudge
      ? OFFSET_INSTRUCTION_TARGETED
      : OFFSET_INSTRUCTION_DEFAULT
    return renderPromptTemplate(
      pickLineFormatInstruction(),
      maxSizeInstruction,
      offsetInstruction,
    )
  },
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  userFacingName,
  getToolUseSummary,
  getActivityDescription(input) {
    const summary = getToolUseSummary(input)
    return summary ? `Reading ${summary}` : 'Reading file'
  },
  isConcurrencySafe() {
    return true
  },
  isReadOnly() {
    return true
  },
  toAutoClassifierInput(input) {
    return input.file_path
  },
  isSearchOrReadCommand() {
    return { isSearch: false, isRead: true }
  },
  getPath({ file_path }): string {
    return file_path || getCwd()
  },
  backfillObservableInput(input) {
    // hooks.mdx documents file_path as absolute; expand so hook allowlists
    // can't be bypassed via ~ or relative paths.
    if (typeof input.file_path === 'string') {
      input.file_path = expandPath(input.file_path)
    }
  },
  async preparePermissionMatcher({ file_path }) {
    return pattern => matchWildcardPattern(pattern, file_path)
  },
  async checkPermissions(input, context): Promise<PermissionDecision> {
    const appState = context.getAppState()
    return checkReadPermissionForTool(
      FileReadTool,
      input,
      appState.toolPermissionContext,
    )
  },
  ...createToolPresentationDelegates(FILE_READ_TOOL_NAME, [
    'renderToolUseMessage',
    'renderToolUseTag',
    'renderToolResultMessage',
    'renderToolUseErrorMessage',
  ]),
  // UI.tsx:140 — ALL types render summary chrome only: "Read N lines",
  // "Read image (42KB)". Never the content itself. The model-facing
  // serialization (below) sends content + CYBER_RISK_MITIGATION_REMINDER
  // + line prefixes; UI shows none of it. Nothing to index. Caught by
  // the render-fidelity test when this initially claimed file.content.
  extractSearchText() {
    return ''
  },
  async validateInput({ file_path, pages }, toolUseContext: ToolUseContext) {
    // Validate pages parameter (pure string parsing, no I/O)
    if (pages !== undefined) {
      const parsed = parsePDFPageRange(pages)
      if (!parsed) {
        return {
          result: false,
          message: `Invalid pages parameter: "${pages}". Use formats like "1-5", "3", or "10-20". Pages are 1-indexed.`,
          errorCode: 7,
        }
      }
      const rangeSize =
        parsed.lastPage === Infinity
          ? PDF_MAX_PAGES_PER_READ + 1
          : parsed.lastPage - parsed.firstPage + 1
      if (rangeSize > PDF_MAX_PAGES_PER_READ) {
        return {
          result: false,
          message: `Page range "${pages}" exceeds maximum of ${PDF_MAX_PAGES_PER_READ} pages per request. Please use a smaller range.`,
          errorCode: 8,
        }
      }
    }

    // Path expansion + deny rule check (no I/O)
    const fullFilePath = expandPath(file_path)

    const appState = toolUseContext.getAppState()
    const denyRule = matchingRuleForInput(
      fullFilePath,
      appState.toolPermissionContext,
      'read',
      'deny',
    )
    if (denyRule !== null) {
      return {
        result: false,
        message:
          'File is in a directory that is denied by your permission settings.',
        errorCode: 1,
      }
    }

    // SECURITY: UNC path check (no I/O) — defer filesystem operations
    // until after user grants permission to prevent NTLM credential leaks
    const isUncPath =
      fullFilePath.startsWith('\\\\') || fullFilePath.startsWith('//')
    if (isUncPath) {
      return { result: true }
    }

    // Binary extension check (string check on extension only, no I/O).
    // PDF, images, and SVG are excluded - this tool renders them natively.
    // Office files (docx/xlsx/pptx/...) are also excluded: callInner converts
    // them to PDF via LibreOffice (W-OFFICE-PARSE PR-CC4). When no engine is
    // installed, callInner returns a clear install hint — a better UX than the
    // generic "cannot read binary" rejection here. Pandoc-text extensions are
    // exempted the same way (PR-4: epub is in BINARY_EXTENSIONS as a ZIP
    // container but routes through pandoc; docx/odt/rtf are covered by the
    // office exemption or absent from BINARY_EXTENSIONS).
    const ext = path.extname(fullFilePath).toLowerCase()
    if (
      hasBinaryExtension(fullFilePath) &&
      !isPDFExtension(ext) &&
      !IMAGE_EXTENSIONS.has(ext.slice(1)) &&
      !isTranscodeImageExtension(ext) &&
      !isOfficeExtension(ext) &&
      !isPandocTextExtension(ext)
    ) {
      // PR-5 (裁决 D8): .ico stays rejected (multi-size container), but the
      // rejection now says HOW to get at the content.
      const imageHint =
        ext === '.ico'
          ? ' If you need this image\'s content, convert it to PNG first (e.g. via Bash: `sips -s format png` on macOS or ImageMagick).'
          : ''
      return {
        result: false,
        message: `This tool cannot read binary files. The file appears to be a binary ${ext} file. Please use appropriate tools for binary file analysis.${imageHint}`,
        errorCode: 4,
      }
    }

    // Block specific device files that would hang (infinite output or blocking input).
    // This is a path-based check with no I/O — safe special files like /dev/null are allowed.
    if (isBlockedDevicePath(fullFilePath)) {
      return {
        result: false,
        message: `Cannot read '${file_path}': this device file would block or produce infinite output.`,
        errorCode: 9,
      }
    }

    return { result: true }
  },
  async call(
    { file_path, offset = 1, limit = undefined, pages },
    context,
    _canUseTool?,
    parentMessage?,
  ) {
    const { readFileState, fileReadingLimits } = context

    const defaults = getDefaultFileReadingLimits()
    const maxSizeBytes =
      fileReadingLimits?.maxSizeBytes ?? defaults.maxSizeBytes
    const maxTokens = fileReadingLimits?.maxTokens ?? defaults.maxTokens

    // Telemetry: track when callers override default read limits.
    // Only fires on override (low volume) — event count = override frequency.
    if (fileReadingLimits !== undefined) {
      logEvent('tengu_file_read_limits_override', {
        hasMaxTokens: fileReadingLimits.maxTokens !== undefined,
        hasMaxSizeBytes: fileReadingLimits.maxSizeBytes !== undefined,
      })
    }

    const ext = path.extname(file_path).toLowerCase().slice(1)
    // Use expandPath for consistent path normalization with FileEditTool/FileWriteTool
    // (especially handles whitespace trimming and Windows path separators)
    const fullFilePath = expandPath(file_path)

    // Dedup: if we've already read this exact range and the file hasn't
    // changed on disk, return a stub instead of re-sending the full content.
    // The earlier Read tool_result is still in context — two full copies
    // waste cache_creation tokens on every subsequent turn. BQ proxy shows
    // ~18% of Read calls are same-file collisions (up to 2.64% of fleet
    // cache_creation). Only applies to text/notebook reads — images/PDFs
    // aren't cached in readFileState so won't match here.
    //
    // Ant soak: 1,734 dedup hits in 2h, no Read error regression.
    // Killswitch pattern: GB can disable if the stub message confuses
    // the model externally.
    // 3P default: killswitch off = dedup enabled. Client-side only — no
    // server support needed, safe for Bedrock/Vertex/Foundry.
    const dedupKillswitch = getFeatureValue_CACHED_MAY_BE_STALE(
      'tengu_read_dedup_killswitch',
      false,
    )
    const existingState = dedupKillswitch
      ? undefined
      : readFileState.get(fullFilePath)
    // Only dedup entries that came from a prior Read (offset is always set
    // by Read). Edit/Write store offset=undefined — their readFileState
    // entry reflects post-edit mtime, so deduping against it would wrongly
    // point the model at the pre-edit Read content.
    if (
      existingState &&
      !existingState.isPartialView &&
      existingState.offset !== undefined
    ) {
      const rangeMatch =
        existingState.offset === offset && existingState.limit === limit
      if (rangeMatch) {
        try {
          const mtimeMs = await getFileModificationTimeAsync(fullFilePath)
          if (mtimeMs === existingState.timestamp) {
            const analyticsExt = getFileExtensionForAnalytics(fullFilePath)
            logEvent('tengu_file_read_dedup', {
              ...(analyticsExt !== undefined && { ext: analyticsExt }),
            })
            return {
              data: {
                type: 'file_unchanged' as const,
                file: { filePath: file_path },
              },
            }
          }
        } catch {
          // stat failed — fall through to full read
        }
      }
    }

    // Discover skills from this file's path (fire-and-forget, non-blocking)
    // Skip in simple mode - no skills available
    const cwd = getCwd()
    if (!isEnvTruthy(process.env.CRABCODE_SIMPLE)) {
      const newSkillDirs = await discoverSkillDirsForPaths([fullFilePath], cwd)
      if (newSkillDirs.length > 0) {
        // Store discovered dirs for attachment display
        for (const dir of newSkillDirs) {
          context.dynamicSkillDirTriggers?.add(dir)
        }
        // Don't await - let skill loading happen in the background
        addSkillDirectories(newSkillDirs).catch(() => {})
      }

      // Activate conditional skills whose path patterns match this file
      activateConditionalSkillsForPaths([fullFilePath], cwd)
    }

    try {
      return await callInner(
        file_path,
        fullFilePath,
        fullFilePath,
        ext,
        offset,
        limit,
        pages,
        maxSizeBytes,
        maxTokens,
        readFileState,
        context,
        parentMessage?.message.id,
      )
    } catch (error) {
      // Handle file-not-found: suggest similar files
      const code = getErrnoCode(error)
      if (code === 'ENOENT') {
        // macOS screenshots may use a thin space or regular space before
        // AM/PM — try the alternate before giving up.
        const altPath = getAlternateScreenshotPath(fullFilePath)
        if (altPath) {
          try {
            return await callInner(
              file_path,
              fullFilePath,
              altPath,
              ext,
              offset,
              limit,
              pages,
              maxSizeBytes,
              maxTokens,
              readFileState,
              context,
              parentMessage?.message.id,
            )
          } catch (altError) {
            if (!isENOENT(altError)) {
              throw altError
            }
            // Alt path also missing — fall through to friendly error
          }
        }

        const similarFilename = findSimilarFile(fullFilePath)
        const cwdSuggestion = await suggestPathUnderCwd(fullFilePath)
        let message = `File does not exist. ${FILE_NOT_FOUND_CWD_NOTE} ${getCwd()}.`
        if (cwdSuggestion) {
          message += ` Did you mean ${cwdSuggestion}?`
        } else if (similarFilename) {
          message += ` Did you mean ${similarFilename}?`
        }
        throw new Error(message)
      }
      throw error
    }
  },
  mapToolResultToToolResultBlockParam(data, toolUseID) {
    switch (data.type) {
      case 'image': {
        return {
          tool_use_id: toolUseID,
          type: 'tool_result',
          content: [
            {
              type: 'image',
              source: {
                type: 'base64',
                data: data.file.base64,
                media_type: data.file.type,
              },
            },
          ],
        }
      }
      case 'notebook':
        return mapNotebookCellsToToolResult(data.file.cells, toolUseID)
      case 'pdf':
        // Return PDF metadata only - the actual content is sent as a supplemental DocumentBlockParam
        return {
          tool_use_id: toolUseID,
          type: 'tool_result',
          content: `PDF file read: ${data.file.filePath} (${formatFileSize(data.file.originalSize)})`,
        }
      case 'parts':
        // Extracted page images are read and sent as image blocks in mapToolResultToAPIMessage
        return {
          tool_use_id: toolUseID,
          type: 'tool_result',
          content: `PDF pages extracted: ${data.file.count} page(s) from ${data.file.filePath} (${formatFileSize(data.file.originalSize)})`,
        }
      case 'file_unchanged':
        return {
          tool_use_id: toolUseID,
          type: 'tool_result',
          content: FILE_UNCHANGED_STUB,
        }
      case 'text': {
        let content: string

        if (data.file.content) {
          content =
            memoryFileFreshnessPrefix(data) +
            formatFileLines(data.file) +
            (shouldIncludeFileReadMitigation()
              ? CYBER_RISK_MITIGATION_REMINDER
              : '')
        } else {
          // Determine the appropriate warning message
          content =
            data.file.totalLines === 0
              ? '<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>'
              : `<system-reminder>Warning: the file exists but is shorter than the provided offset (${data.file.startLine}). The file has ${data.file.totalLines} lines.</system-reminder>`
        }

        return {
          tool_use_id: toolUseID,
          type: 'tool_result',
          content,
        }
      }
    }
  },
} satisfies ToolDef<InputSchema, Output>)

function pickLineFormatInstruction(): string {
  return LINE_FORMAT_INSTRUCTION
}

/** Format file content with line numbers. */
function formatFileLines(file: { content: string; startLine: number }): string {
  return addLineNumbers(file)
}

export const CYBER_RISK_MITIGATION_REMINDER =
  '\n\n<system-reminder>\nWhenever you read a file, you should consider whether it would be considered malware. You CAN and SHOULD provide analysis of malware, what it is doing. But you MUST refuse to improve or augment the code. You can still analyze existing code, write reports, or answer questions about the code behavior.\n</system-reminder>\n'

// SDK caps: models with max_effort capability are exempt from the
// cyber-risk mitigation reminder. caps undefined → SDK default model fallback
// → conservative include if still undefined.
function shouldIncludeFileReadMitigation(): boolean {
  const model = getMainLoopModel()
  const supportsMaxEffort = getCachedCapabilityWithDefaultFallback(
    model,
    'supports_max_effort',
  )
  return !supportsMaxEffort
}

/**
 * Side-channel from call() to mapToolResultToToolResultBlockParam: mtime
 * of auto-memory files, keyed by the `data` object identity. Avoids
 * adding a presentation-only field to the output schema (which flows
 * into SDK types) and avoids sync fs in the mapper. WeakMap auto-GCs
 * when the data object becomes unreachable after rendering.
 */
const memoryFileMtimes = new WeakMap<object, number>()

function memoryFileFreshnessPrefix(data: object): string {
  const mtimeMs = memoryFileMtimes.get(data)
  if (mtimeMs === undefined) return ''
  return memoryFreshnessNote(mtimeMs)
}

async function validateContentTokens(
  content: string,
  ext: string,
  maxTokens?: number,
): Promise<void> {
  const effectiveMaxTokens =
    maxTokens ?? getDefaultFileReadingLimits().maxTokens

  const tokenEstimate = roughTokenCountEstimationForFileType(content, ext)
  if (!tokenEstimate || tokenEstimate <= effectiveMaxTokens / 4) return

  const tokenCount = await countTokensWithAPI(content)
  const effectiveCount = tokenCount ?? tokenEstimate

  if (effectiveCount > effectiveMaxTokens) {
    throw new MaxFileReadTokenExceededError(effectiveCount, effectiveMaxTokens)
  }
}

export type ImageResult = {
  type: 'image'
  file: {
    base64: string
    type: Base64ImageSource['media_type']
    originalSize: number
    dimensions?: ImageDimensions
  }
}

function createImageResponse(
  buffer: Buffer,
  mediaType: string,
  originalSize: number,
  dimensions?: ImageDimensions,
): ImageResult {
  return {
    type: 'image',
    file: {
      base64: buffer.toString('base64'),
      type: `image/${mediaType}` as Base64ImageSource['media_type'],
      originalSize,
      dimensions,
    },
  }
}

export async function persistReadImagePreviewArtifact(
  data: ImageResult,
  filePath: string,
  persist: typeof persistBinaryContent = persistBinaryContent,
): Promise<ToolArtifactCandidate[] | undefined> {
  const previewBytes = Buffer.from(data.file.base64, 'base64')
  const preview = await persist(
    previewBytes,
    data.file.type,
    `read-image-preview-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
  )
  if ('error' in preview) return undefined
  return [{
    id: path.basename(preview.filepath),
    kind: 'image',
    mimeType: data.file.type,
    displayName: `${path.parse(filePath).name}.${preview.ext}`,
    location: { type: 'runtimePath', path: preview.filepath },
    byteSize: preview.size,
  }]
}

/**
 * Inner implementation of call, separated to allow ENOENT handling in the outer call.
 */
async function callInner(
  file_path: string,
  fullFilePath: string,
  resolvedFilePath: string,
  ext: string,
  offset: number,
  limit: number | undefined,
  pages: string | undefined,
  maxSizeBytes: number,
  maxTokens: number,
  readFileState: ToolUseContext['readFileState'],
  context: ToolUseContext,
  messageId: string | undefined,
): Promise<{
  data: Output
  artifacts?: ToolArtifactCandidate[]
  newMessages?: ReturnType<typeof createUserMessage>[]
}> {
  // --- Office (docx/xlsx/pptx/doc/xls/ppt/odt/ods/odp/rtf) — W-OFFICE-PARSE PR-CC4 ---
  // Dual-engine routing (PR-CC4b):
  //   • docx/odt/rtf (word docs) → pandoc → markdown TEXT (light, token-efficient,
  //     text-model-native, no vision needed). Re-enter callInner as 'md'.
  //     Embedded images are extracted + attached as image blocks alongside
  //     the text (W-DOCX-EMBEDDED-IMAGE) — no longer dropped.
  //   • xls/xlsx/ppt/pptx/doc/ods/odp (sheets/slides/legacy) → LibreOffice → PDF
  //     (layout/tables/slides need visual fidelity; pandoc can't read them).
  //     Re-enter callInner as 'pdf' (page-image extraction for text-only models,
  //     document block otherwise, size/token guards all apply).
  // No conversion is ever attempted without an installed engine — we surface
  // install guidance instead of blocking the turn on a download (审计问题 2).
  // D3 (2026-07-01): rtf is pandoc-routable but NOT in OFFICE_EXTENSIONS (it
  // has no LibreOffice-fallback design intent the way xls/xlsx/ppt do — it's
  // purely a "pandoc can read this text format" case). Without this `||`,
  // rtf would skip this entire block and fall through to the generic binary
  // text reader, seeing raw `{\rtf1...}` control words (the exact D3 bug).
  if (isOfficeExtension(ext) || isPandocTextExtension(ext)) {
    // Prefer pandoc → text for word documents.
    if (isPandocTextExtension(ext)) {
      const pandoc = await resolvePandocPath()
      if (pandoc) {
        const conv = await convertDocxToMarkdown(resolvedFilePath, pandoc, ext)
        if (conv.success) {
          logFileOperation({
            operation: 'read',
            tool: 'FileReadTool',
            filePath: fullFilePath,
            content: `office → markdown via pandoc (${ext})`,
          })

          // W-DOCX-EMBEDDED-IMAGE: sanitize dangling/absolute-path image
          // refs pandoc's --extract-media leaves in the markdown, and
          // collect the real image files to attach as image blocks. Never
          // let this best-effort enhancement break the core text read — any
          // failure here just falls back to pandoc's raw (unsanitized)
          // markdown, same as before this feature existed.
          let embeddedImageBlocks: Awaited<
            ReturnType<typeof buildDocxEmbeddedImageBlocks>
          > = []
          try {
            const rawMd = await readFileAsync(conv.mdPath, 'utf8')
            const { text, images, unsupportedCount, truncatedCount } =
              sanitizePandocMediaReferences(rawMd, conv.mediaDir)
            if (text !== rawMd) {
              await writeFileAsync(conv.mdPath, text, 'utf8')
            }
            if (images.length > 0) {
              embeddedImageBlocks = await buildDocxEmbeddedImageBlocks(images)
            }
            if (images.length > 0 || unsupportedCount > 0 || truncatedCount > 0) {
              logEvent('tengu_docx_embedded_image_extraction', {
                attachedCount: images.length,
                unsupportedCount,
                truncatedCount,
              })
            }
          } catch (e) {
            logError(e)
          }

          // Re-enter as markdown text; pages is meaningless for text → drop it.
          const inner = await callInner(
            file_path,
            fullFilePath,
            conv.mdPath,
            'md',
            offset,
            limit,
            undefined,
            maxSizeBytes,
            maxTokens,
            readFileState,
            context,
            messageId,
          )
          if (embeddedImageBlocks.length === 0) return inner
          return {
            data: inner.data,
            newMessages: [
              ...(inner.newMessages ?? []),
              createUserMessage({ content: embeddedImageBlocks, isMeta: true }),
            ],
          }
        }
        // pandoc failed → fall through to LibreOffice→PDF as a fallback.
      }
      // pandoc unavailable → fall through to LibreOffice→PDF.
    }

    // W-DOC-VISION-QUALITY-REMEDIATION PR-2 (F2): xlsx routes by the MAIN
    // MODEL'S image modality, not by which engine happens to be installed.
    if (shouldPreferXlsxDataExtraction(ext, mainLoopModelImageModality())) {
      const xlsxConv = await convertXlsxToMarkdown(resolvedFilePath, {
        // PR-10:入口原因 → 文档标题诚实措辞（此入口 LibreOffice 可能装着）。
        reason: 'text_model',
      })
      if (xlsxConv.success) {
        logFileOperation({
          operation: 'read',
          tool: 'FileReadTool',
          filePath: fullFilePath,
          content: `office → markdown via built-in xlsx reader (main model not vision-capable, ${ext})`,
        })
        return callInner(
          file_path,
          fullFilePath,
          xlsxConv.mdPath,
          'md',
          offset,
          limit,
          undefined,
          maxSizeBytes,
          maxTokens,
          readFileState,
          context,
          messageId,
        )
      }
      // exceljs failed (encrypted / corrupt / over the size cap) → fall
      // through to the LibreOffice chain, which may still handle it.
    }

    const soffice = await resolveSofficePath()
    if (!soffice) {
      // W-XLSX-NATIVE-FALLBACK: xlsx is ZIP+XML (OOXML) — parseable in pure JS
      // with no external binary. Unlike ppt/doc/legacy-xls (which need visual
      // layout fidelity, hence LibreOffice), a data-extraction read of xlsx
      // has no such requirement, so it doesn't have to fail-closed just
      // because LibreOffice isn't installed.
      if (isXlsxFallbackExtension(ext)) {
        const xlsxConv = await convertXlsxToMarkdown(resolvedFilePath, {
          reason: 'engine_unavailable',
        })
        if (xlsxConv.success) {
          logFileOperation({
            operation: 'read',
            tool: 'FileReadTool',
            filePath: fullFilePath,
            content: `office → markdown via built-in xlsx reader (LibreOffice unavailable, ${ext})`,
          })
          return callInner(
            file_path,
            fullFilePath,
            xlsxConv.mdPath,
            'md',
            offset,
            limit,
            undefined,
            maxSizeBytes,
            maxTokens,
            readFileState,
            context,
            messageId,
          )
        }
        throw new Error(
          `${officeEngineMissingMessage(file_path, ext)}\n\nAlso tried the built-in xlsx reader as a fallback, but it failed: ${xlsxConv.error}`,
        )
      }
      throw new Error(officeEngineMissingMessage(file_path, ext))
    }
    const conv = await convertOfficeToPdf(resolvedFilePath, soffice)
    if (!conv.success) {
      throw new Error(conv.error)
    }
    logFileOperation({
      operation: 'read',
      tool: 'FileReadTool',
      filePath: fullFilePath,
      content: `office → PDF via LibreOffice (${ext})`,
    })
    // Re-enter as PDF: resolvedFilePath points at the converted PDF; the
    // original office path is kept for fullFilePath logging/dedup keys.
    return callInner(
      file_path,
      fullFilePath,
      conv.pdfPath,
      'pdf',
      offset,
      limit,
      pages,
      maxSizeBytes,
      maxTokens,
      readFileState,
      context,
      messageId,
    )
  }

  // --- Notebook ---
  if (ext === 'ipynb') {
    const cells = await readNotebook(resolvedFilePath)
    const cellsJson = jsonStringify(cells)

    const cellsJsonBytes = Buffer.byteLength(cellsJson)
    if (cellsJsonBytes > maxSizeBytes) {
      throw new Error(
        `Notebook content (${formatFileSize(cellsJsonBytes)}) exceeds maximum allowed size (${formatFileSize(maxSizeBytes)}). ` +
          `Use ${BASH_TOOL_NAME} with jq to read specific portions:\n` +
          `  cat "${file_path}" | jq '.cells[:20]' # First 20 cells\n` +
          `  cat "${file_path}" | jq '.cells[100:120]' # Cells 100-120\n` +
          `  cat "${file_path}" | jq '.cells | length' # Count total cells\n` +
          `  cat "${file_path}" | jq '.cells[] | select(.cell_type=="code") | .source' # All code sources`,
      )
    }

    await validateContentTokens(cellsJson, ext, maxTokens)

    // Get mtime via async stat (single call, no prior existence check)
    const stats = await getFsImplementation().stat(resolvedFilePath)
    readFileState.set(fullFilePath, {
      content: cellsJson,
      timestamp: Math.floor(stats.mtimeMs),
      offset,
      limit,
    })
    context.nestedMemoryAttachmentTriggers?.add(fullFilePath)

    const data = {
      type: 'notebook' as const,
      file: { filePath: file_path, cells },
    }

    logFileOperation({
      operation: 'read',
      tool: 'FileReadTool',
      filePath: fullFilePath,
      content: cellsJson,
    })

    return { data }
  }

  // --- Image (single read, no double-read) ---
  if (IMAGE_EXTENSIONS.has(ext) || isTranscodeImageExtension(ext)) {
    // Images have their own size limits (token budget + compression) —
    // don't apply the text maxSizeBytes cap.
    //
    // PR-5 (F4): bmp/tiff first transcode to PNG (pure JS, see
    // imageTranscode.ts) — the gateway accepts only jpeg/png/gif/webp, so
    // passing their raw bytes through would 400. The PNG re-enters the
    // ordinary pipeline (token budget / compression / metadata unchanged).
    let imagePath = resolvedFilePath
    let transcodeNote: string | null = null
    if (isTranscodeImageExtension(ext)) {
      // Cap the raw read at the transcode limit + 1: an over-limit file still
      // trips the transcoder's honest size error without ever materializing
      // hundreds of MB in memory.
      const raw = await getFsImplementation().readFileBytes(
        resolvedFilePath,
        TRANSCODE_IMAGE_MAX_BYTES + 1,
      )
      // 2026-07-04 审计 PR-10 — WithInfo 变体带回多页 TIFF 诚实标注，随
      // metadata 文本一并交给模型（此前只解首页且完全静默）。
      const transcoded = transcodeImageToPngWithInfo(raw, ext)
      transcodeNote = transcoded.note
      const png = transcoded.png
      const outDir = await mkdtemp(path.join(tmpdir(), 'crabcode-img-transcode-'))
      imagePath = path.join(
        outDir,
        `${path.basename(resolvedFilePath, path.extname(resolvedFilePath))}.png`,
      )
      await writeFileAsync(imagePath, png)
      logEvent('tengu_image_transcode', { success: true })
    }
    const data = await readImageWithTokenBudget(imagePath, maxTokens)
    context.nestedMemoryAttachmentTriggers?.add(fullFilePath)

    logFileOperation({
      operation: 'read',
      tool: 'FileReadTool',
      filePath: fullFilePath,
      content: data.file.base64,
    })

    const dimensionText = data.file.dimensions
      ? createImageMetadataText(data.file.dimensions)
      : null
    const metadataText =
      [dimensionText, transcodeNote].filter(Boolean).join('\n') || null

    // The model image is already dimension/size constrained. Persist those
    // exact preview bytes as an explicit Artifact candidate so the host can
    // freeze them into its HOME-scoped cache. Pointing ImageView at the source
    // path fails for otherwise valid `/tmp`, `/Volumes`, and non-HOME
    // workspaces because Tauri deliberately does not expose the whole disk.
    const artifacts = await persistReadImagePreviewArtifact(data, file_path)

    return {
      data,
      ...(artifacts ? { artifacts } : {}),
      ...(metadataText && {
        newMessages: [
          createUserMessage({ content: metadataText, isMeta: true }),
        ],
      }),
    }
  }

  // --- PDF ---
  if (isPDFExtension(ext)) {
    // W-DOC-VISION-QUALITY-REMEDIATION PR-3a (F1): a text-only main model
    // reading a PDF should get the PDF's TEXT layer, not a lossy sidecar
    // transcription of page images. Try pdftotext first; low text density
    // means a scanned/image-only PDF → fall through to the page-image chain
    // (the sidecar's remaining legitimate scenario). pdftotext missing
    // (vendored archives ship only pdftoppm/pdfinfo until PR-10) or failing
    // degrades silently to the existing chains — never a new error (裁决 D6).
    if (mainLoopModelImageModality() === 'text_only') {
      const parsedRange = pages ? parsePDFPageRange(pages) : null
      const textResult = await extractPDFText(
        resolvedFilePath,
        parsedRange ?? undefined,
      )
      if (textResult.success && pdfTextMeetsDensityGate(textResult.data)) {
        logEvent('tengu_pdf_text_extraction', {
          success: true,
          pageCount: textResult.data.pageCount,
          hasPageRange: Boolean(pages),
        })
        logFileOperation({
          operation: 'read',
          tool: 'FileReadTool',
          filePath: fullFilePath,
          content: `PDF text layer via pdftotext (${textResult.data.pageCount} page(s))`,
        })
        // Re-enter as plain text: offset/limit and the content-token gate all
        // apply as for any text file (same re-entry pattern as office → md).
        const outDir = await mkdtemp(path.join(tmpdir(), 'crabcode-pdf-text-'))
        const txtPath = path.join(
          outDir,
          `${path.basename(resolvedFilePath, path.extname(resolvedFilePath))}.txt`,
        )
        // J8（2026-07-04 审计 PR-10）:整体密度门通过的混合 PDF，稀疏页（整页
        // 扫描图）原位插入"[第 N 页为图像内容，未转写]"标注——不再静默丢页。
        await writeFileAsync(txtPath, annotateSparsePages(textResult.data), 'utf8')
        return callInner(
          file_path,
          fullFilePath,
          txtPath,
          'txt',
          offset,
          limit,
          undefined,
          maxSizeBytes,
          maxTokens,
          readFileState,
          context,
          messageId,
        )
      }
      if (textResult.success) {
        // Extraction ran but the text layer is too sparse — scanned PDF.
        logEvent('tengu_pdf_text_extraction', {
          success: false,
          lowDensity: true,
          pageCount: textResult.data.pageCount,
        })
      } else if (textResult.error.reason !== 'unavailable') {
        logEvent('tengu_pdf_text_extraction', {
          success: false,
          lowDensity: false,
          available: true,
        })
      }
    }

    if (pages) {
      const parsedRange = parsePDFPageRange(pages)
      const extractResult = await extractPDFPages(
        resolvedFilePath,
        parsedRange ?? undefined,
      )
      if (!extractResult.success) {
        throw new Error(extractResult.error.message)
      }
      logEvent('tengu_pdf_page_extraction', {
        success: true,
        pageCount: extractResult.data.file.count,
        fileSize: extractResult.data.file.originalSize,
        hasPageRange: true,
      })
      logFileOperation({
        operation: 'read',
        tool: 'FileReadTool',
        filePath: fullFilePath,
        content: `PDF pages ${pages}`,
      })
      const imageBlocks = await buildPDFPageImageBlocks(
        extractResult.data.file.outputDir,
      )
      return {
        data: extractResult.data,
        ...(imageBlocks.length > 0 && {
          newMessages: [
            createUserMessage({ content: imageBlocks, isMeta: true }),
          ],
        }),
      }
    }

    const pageCount = await getPDFPageCount(resolvedFilePath)
    if (pageCount !== null && pageCount > PDF_AT_MENTION_INLINE_THRESHOLD) {
      throw new Error(
        `This PDF has ${pageCount} pages, which is too many to read at once. ` +
          `Use the pages parameter to read specific page ranges (e.g., pages: "1-5"). ` +
          `Maximum ${PDF_MAX_PAGES_PER_READ} pages per request.`,
      )
    }

    const fs = getFsImplementation()
    const stats = await fs.stat(resolvedFilePath)
    const shouldExtractPages =
      !isPDFSupported() || stats.size > PDF_EXTRACT_SIZE_THRESHOLD

    // W-MULTIMODAL-INPUT P3: when extraction applies (large PDF, or a model
    // with no capability record), USE the extracted page images instead of
    // discarding them. The prior code logged the extraction then threw the
    // result away and sent the full base64 document block anyway — wasting a
    // poppler run and ignoring the documented "extract into page images
    // instead of base64 document blocks" design (apiLimits.ts
    // PDF_EXTRACT_SIZE_THRESHOLD). Page images are image blocks, so the
    // capability-aware media layer (queryModel) degrades them for text-only
    // models.
    if (shouldExtractPages) {
      const extractResult = await extractPDFPages(resolvedFilePath)
      if (extractResult.success) {
        logEvent('tengu_pdf_page_extraction', {
          success: true,
          pageCount: extractResult.data.file.count,
          fileSize: extractResult.data.file.originalSize,
        })
        const imageBlocks = await buildPDFPageImageBlocks(
          extractResult.data.file.outputDir,
        )
        if (imageBlocks.length > 0) {
          logFileOperation({
            operation: 'read',
            tool: 'FileReadTool',
            filePath: fullFilePath,
            content: `PDF extracted to ${imageBlocks.length} page image(s)`,
          })
          return {
            data: extractResult.data,
            newMessages: [
              createUserMessage({ content: imageBlocks, isMeta: true }),
            ],
          }
        }
        // Extraction succeeded but produced no page images — fall through to
        // the document-block path (or the not-supported throw below).
      } else {
        logEvent('tengu_pdf_page_extraction', {
          success: false,
          available: extractResult.error.reason !== 'unavailable',
          fileSize: stats.size,
        })
        // Extraction failed. A model with no capability record has no
        // document-block fallback → the throw below applies; a supported model
        // gracefully falls back to the document block.
      }
    }

    if (!isPDFSupported()) {
      // 文案即契约:terminalToolError.ts 签名 pdf_model_unsupported 按首句字符串判据
      // (真源 pdf.ts PDF_MODEL_UNSUPPORTED_MESSAGE_PREFIX),改文案必须同步
      throw new Error(
        `${PDF_MODEL_UNSUPPORTED_MESSAGE_PREFIX}. Use a newer model that supports PDFs, ` +
          `or use the pages parameter to read specific page ranges (e.g., pages: "1-5", maximum ${PDF_MAX_PAGES_PER_READ} pages per request). ` +
          'Page extraction requires poppler — install it from CrabCode settings → 多媒体 (or the first-launch 解析服务 step), or manually (`brew install poppler` on macOS or `apt-get install poppler-utils` on Debian/Ubuntu).',
      )
    }

    const readResult = await readPDF(resolvedFilePath)
    if (!readResult.success) {
      throw new Error(readResult.error.message)
    }
    const pdfData = readResult.data
    logFileOperation({
      operation: 'read',
      tool: 'FileReadTool',
      filePath: fullFilePath,
      content: pdfData.file.base64,
    })

    return {
      data: pdfData,
      newMessages: [
        createUserMessage({
          content: [
            {
              type: 'document',
              source: {
                type: 'base64',
                media_type: 'application/pdf',
                data: pdfData.file.base64,
              },
            },
          ],
          isMeta: true,
        }),
      ],
    }
  }

  // --- Text file (single async read via readFileInRange) ---
  // W-DOC-VISION-QUALITY-REMEDIATION PR-4 (F3): content sniff gate. Extension
  // allowlists can't cover every binary format (unknown extensions, UTF-16
  // files, renamed containers) — decoding those as UTF-8 silently feeds
  // mojibake to the model. Sniff the first 8KB of RAW bytes with the existing
  // isBinaryContent (files.ts; previously only git.ts used it) and refuse
  // loudly instead. UTF-16 text files are a KNOWN casualty (裁决 D7): they
  // were mojibake before too — an explicit error with an iconv hint strictly
  // beats silent garbage.
  {
    const rawPrefix = await readRawFilePrefix(resolvedFilePath)
    if (rawPrefix !== null && isBinaryContent(rawPrefix)) {
      throw new Error(
        `This file has a text extension but binary content and cannot be read as text. ` +
          `If it is UTF-16 encoded, convert it first (e.g. via Bash: iconv -f UTF-16 -t UTF-8 "${file_path}" > converted.txt); ` +
          `otherwise use a format-appropriate tool via Bash.`,
      )
    }
  }
  const lineOffset = offset === 0 ? 0 : offset - 1
  const { content, lineCount, totalLines, totalBytes, readBytes, mtimeMs } =
    await readFileInRange(
      resolvedFilePath,
      lineOffset,
      limit,
      limit === undefined ? maxSizeBytes : undefined,
      context.abortController.signal,
    )

  await validateContentTokens(content, ext, maxTokens)

  readFileState.set(fullFilePath, {
    content,
    timestamp: Math.floor(mtimeMs),
    offset,
    limit,
  })
  context.nestedMemoryAttachmentTriggers?.add(fullFilePath)

  // Snapshot before iterating — a listener that unsubscribes mid-callback
  // would splice the live array and skip the next listener.
  for (const listener of fileReadListeners.slice()) {
    listener(resolvedFilePath, content)
  }

  const data = {
    type: 'text' as const,
    file: {
      filePath: file_path,
      content,
      numLines: lineCount,
      startLine: offset,
      totalLines,
    },
  }
  if (isAutoMemFile(fullFilePath)) {
    memoryFileMtimes.set(data, mtimeMs)
  }

  logFileOperation({
    operation: 'read',
    tool: 'FileReadTool',
    filePath: fullFilePath,
    content,
  })

  const sessionFileType = detectSessionFileType(fullFilePath)
  const analyticsExt = getFileExtensionForAnalytics(fullFilePath)
  logEvent('tengu_session_file_read', {
    totalLines,
    readLines: lineCount,
    totalBytes,
    readBytes,
    offset,
    ...(limit !== undefined && { limit }),
    ...(analyticsExt !== undefined && { ext: analyticsExt }),
    ...(messageId !== undefined && {
      messageID:
        messageId as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    }),
    is_session_memory: sessionFileType === 'session_memory',
    is_session_transcript: sessionFileType === 'session_transcript',
  })

  return { data }
}

/**
 * Reads an image file and applies token-based compression if needed.
 * Reads the file ONCE, then applies standard resize. If the result exceeds
 * the token limit, applies aggressive compression from the same buffer.
 *
 * @param filePath - Path to the image file
 * @param maxTokens - Maximum token budget for the image
 * @returns Image data with appropriate compression applied
 */
export async function readImageWithTokenBudget(
  filePath: string,
  maxTokens: number = getDefaultFileReadingLimits().maxTokens,
  maxBytes?: number,
): Promise<ImageResult> {
  // Read file ONCE — capped to maxBytes to avoid OOM on huge files
  const imageBuffer = await getFsImplementation().readFileBytes(
    filePath,
    maxBytes,
  )
  const originalSize = imageBuffer.length

  if (originalSize === 0) {
    throw new Error(`Image file is empty: ${filePath}`)
  }

  const detectedMediaType = detectImageFormatFromBuffer(imageBuffer)
  const detectedFormat = detectedMediaType.split('/')[1] || 'png'

  // Try standard resize
  let result: ImageResult
  try {
    const resized = await maybeResizeAndDownsampleImageBuffer(
      imageBuffer,
      originalSize,
      detectedFormat,
    )
    result = createImageResponse(
      resized.buffer,
      resized.mediaType,
      originalSize,
      resized.dimensions,
    )
  } catch (e) {
    if (e instanceof ImageResizeError) throw e
    logError(e)
    result = createImageResponse(imageBuffer, detectedFormat, originalSize)
  }

  // Check if it fits in token budget
  const estimatedTokens = Math.ceil(result.file.base64.length * 0.125)
  if (estimatedTokens > maxTokens) {
    // Aggressive compression from the SAME buffer (no re-read)
    try {
      const compressed = await compressImageBufferWithTokenLimit(
        imageBuffer,
        maxTokens,
        detectedMediaType,
      )
      return {
        type: 'image',
        file: {
          base64: compressed.base64,
          type: compressed.mediaType,
          originalSize,
        },
      }
    } catch (e) {
      logError(e)
      // Fallback: heavily compressed version from the SAME buffer
      try {
        const sharpModule = await import('sharp')
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const sharp = ((sharpModule as any).default || sharpModule) as any

        const fallbackBuffer = await sharp(imageBuffer)
          .resize(400, 400, {
            fit: 'inside',
            withoutEnlargement: true,
          })
          .jpeg({ quality: 20 })
          .toBuffer()

        // PR-D 不变量（imageResizer.ts::usableOutput 同款）：处理器静默返回
        // 零字节时诚实透传原图，绝不返回 base64:'' 的"成功"结果。
        if (fallbackBuffer.length === 0) {
          return createImageResponse(imageBuffer, detectedFormat, originalSize)
        }
        return createImageResponse(fallbackBuffer, 'jpeg', originalSize)
      } catch (error) {
        logError(error)
        return createImageResponse(imageBuffer, detectedFormat, originalSize)
      }
    }
  }

  return result
}
