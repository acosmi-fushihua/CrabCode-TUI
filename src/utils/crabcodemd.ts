/**
 * Files are loaded in the following order:
 *
 * 1. Managed memory (eg. /etc/crabcode-code/CRABCODE.md) - Global instructions for all users
 * 2. User memory (~/.crabcode/CRABCODE.md) - Private global instructions for all projects
 * 3. Project memory (CRABCODE.md, .crabcode/CRABCODE.md, and .crabcode/rules/*.md in project roots) - Instructions checked into the codebase
 * 4. Local memory (CRABCODE.local.md in project roots) - Private project-specific instructions
 *
 * Files are loaded in reverse order of priority, i.e. the latest files are highest priority
 * with the model paying more attention to them.
 *
 * File discovery:
 * - User memory is loaded from the user's home directory
 * - Project and Local files are discovered by traversing from the current directory up to root
 * - Files closer to the current directory have higher priority (loaded later)
 * - CRABCODE.md, .crabcode/CRABCODE.md, and all .md files in .crabcode/rules/ are checked in each directory for Project memory
 *
 * Memory @include directive:
 * - Memory files can include other files using @ notation
 * - Syntax: @path, @./relative/path, @~/home/path, or @/absolute/path
 * - @path (without prefix) is treated as a relative path (same as @./path)
 * - Works in leaf text nodes only (not inside code blocks or code strings)
 * - Included files are added as separate entries before the including file
 * - Circular references are prevented by tracking processed files
 * - Non-existent files are silently ignored
 */

import { feature } from './featurePolyfill.js'
import ignore from 'ignore'
import memoize from 'lodash-es/memoize.js'
import { Lexer } from 'marked'
import {
  basename,
  dirname,
  extname,
  isAbsolute,
  join,
  parse,
  relative,
  sep,
} from 'path'
import picomatch from 'picomatch'
import { logEvent } from 'src/services/analytics/index.js'
import {
  getAdditionalDirectoriesForCrabcodeMd,
  getOriginalCwd,
} from '../bootstrap/state.js'
import { truncateEntrypointContent } from '../memdir/memdir.js'
import {
  getAutoMemEntrypoint,
  getGlobalMemEntrypoint,
  getKnowledgeDir,
  isAutoMemoryEnabled,
} from '../memdir/paths.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../services/analytics/growthbook.js'
import {
  getCurrentProjectConfig,
  getManagedCrabCodeRulesDir,
  getMemoryPath,
  getUserCrabCodeRulesDir,
} from './config.js'
import { logForDebugging } from './debug.js'
import { logForDiagnosticsNoPII } from './diagLogs.js'
import { getCrabCodeConfigHomeDir, isEnvTruthy } from './envUtils.js'
import { getErrnoCode } from './errors.js'
import { normalizePathForComparison } from './file.js'
import { cacheKeys, type FileStateCache } from './fileStateCache.js'
import {
  parseFrontmatter,
  splitPathInFrontmatter,
} from './frontmatterParser.js'
import { getFsImplementation, safeResolvePath } from './fsOperations.js'
import { findCanonicalGitRoot, findGitRoot } from './git.js'
import {
  executeInstructionsLoadedHooks,
  hasInstructionsLoadedHook,
  type InstructionsLoadReason,
  type InstructionsMemoryType,
} from './hooks.js'
import type { MemoryType } from './memory/types.js'
import { expandPath } from './path.js'
import { pathInWorkingPath } from './permissions/filesystem.js'
import { isSettingSourceEnabled } from './settings/constants.js'
import { getInitialSettings } from './settings/settings.js'

/* eslint-disable @typescript-eslint/no-require-imports */
const teamMemPaths = feature('TEAMMEM')
  ? (require('../memdir/teamMemPaths.js') as typeof import('../memdir/teamMemPaths.js'))
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

let hasLoggedInitialLoad = false

const MEMORY_INSTRUCTION_PROMPT =
  'Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.'
// Recommended max character count for a memory file
export const MAX_MEMORY_CHARACTER_COUNT = 40000

// File extensions that are allowed for @include directives
// This prevents binary files (images, PDFs, etc.) from being loaded into memory
const TEXT_FILE_EXTENSIONS = new Set([
  // Markdown and text
  '.md',
  '.txt',
  '.text',
  // Data formats
  '.json',
  '.yaml',
  '.yml',
  '.toml',
  '.xml',
  '.csv',
  // Web
  '.html',
  '.htm',
  '.css',
  '.scss',
  '.sass',
  '.less',
  // JavaScript/TypeScript
  '.js',
  '.ts',
  '.tsx',
  '.jsx',
  '.mjs',
  '.cjs',
  '.mts',
  '.cts',
  // Python
  '.py',
  '.pyi',
  '.pyw',
  // Ruby
  '.rb',
  '.erb',
  '.rake',
  // Go
  '.go',
  // Rust
  '.rs',
  // Java/Kotlin/Scala
  '.java',
  '.kt',
  '.kts',
  '.scala',
  // C/C++
  '.c',
  '.cpp',
  '.cc',
  '.cxx',
  '.h',
  '.hpp',
  '.hxx',
  // C#
  '.cs',
  // Swift
  '.swift',
  // Shell
  '.sh',
  '.bash',
  '.zsh',
  '.fish',
  '.ps1',
  '.bat',
  '.cmd',
  // Config
  '.env',
  '.ini',
  '.cfg',
  '.conf',
  '.config',
  '.properties',
  // Database
  '.sql',
  '.graphql',
  '.gql',
  // Protocol
  '.proto',
  // Frontend frameworks
  '.vue',
  '.svelte',
  '.astro',
  // Templating
  '.ejs',
  '.hbs',
  '.pug',
  '.jade',
  // Other languages
  '.php',
  '.pl',
  '.pm',
  '.lua',
  '.r',
  '.R',
  '.dart',
  '.ex',
  '.exs',
  '.erl',
  '.hrl',
  '.clj',
  '.cljs',
  '.cljc',
  '.edn',
  '.hs',
  '.lhs',
  '.elm',
  '.ml',
  '.mli',
  '.f',
  '.f90',
  '.f95',
  '.for',
  // Build files
  '.cmake',
  '.make',
  '.makefile',
  '.gradle',
  '.sbt',
  // Documentation
  '.rst',
  '.adoc',
  '.asciidoc',
  '.org',
  '.tex',
  '.latex',
  // Lock files (often text-based)
  '.lock',
  // Misc
  '.log',
  '.diff',
  '.patch',
])

export type MemoryFileInfo = {
  path: string
  type: MemoryType
  content: string
  parent?: string // Path of the file that included this one
  globs?: string[] // Glob patterns for file paths this rule applies to
  // True when auto-injection transformed `content` (stripped HTML comments,
  // stripped frontmatter, truncated MEMORY.md) such that it no longer matches
  // the bytes on disk. When set, `rawContent` holds the unmodified disk bytes
  // so callers can cache a `isPartialView` readFileState entry — presence in
  // cache provides dedup + change detection, but Edit/Write still require an
  // explicit Read before proceeding.
  contentDiffersFromDisk?: boolean
  rawContent?: string
}

function pathInOriginalCwd(path: string): boolean {
  return pathInWorkingPath(path, getOriginalCwd())
}

/**
 * Parses raw content to extract both content and glob patterns from frontmatter
 * @param rawContent Raw file content with frontmatter
 * @returns Object with content and globs (undefined if no paths or match-all pattern)
 */
function parseFrontmatterPaths(rawContent: string): {
  content: string
  paths?: string[]
} {
  const { frontmatter, content } = parseFrontmatter(rawContent)

  if (!frontmatter.paths) {
    return { content }
  }

  const patterns = splitPathInFrontmatter(frontmatter.paths)
    .map(pattern => {
      // Remove /** suffix - ignore library treats 'path' as matching both
      // the path itself and everything inside it
      return pattern.endsWith('/**') ? pattern.slice(0, -3) : pattern
    })
    .filter((p: string) => p.length > 0)

  // If all patterns are ** (match-all), treat as no globs (undefined)
  // This means the file applies to all paths
  if (patterns.length === 0 || patterns.every((p: string) => p === '**')) {
    return { content }
  }

  return { content, paths: patterns }
}

/**
 * Strip block-level HTML comments (<!-- ... -->) from markdown content.
 *
 * Uses the marked lexer to identify comments at the block level only, so
 * comments inside inline code spans and fenced code blocks are preserved.
 * Inline HTML comments inside a paragraph are also left intact; the intended
 * use case is authorial notes that occupy their own lines.
 *
 * Unclosed comments (`<!--` with no matching `-->`) are left in place so a
 * typo doesn't silently swallow the rest of the file.
 */
export function stripHtmlComments(content: string): {
  content: string
  stripped: boolean
} {
  if (!content.includes('<!--')) {
    return { content, stripped: false }
  }
  // gfm:false is fine here — html-block detection is a CommonMark rule.
  return stripHtmlCommentsFromTokens(new Lexer({ gfm: false }).lex(content))
}

function stripHtmlCommentsFromTokens(tokens: ReturnType<Lexer['lex']>): {
  content: string
  stripped: boolean
} {
  let result = ''
  let stripped = false

  // A well-formed HTML comment span. Non-greedy so multiple comments on the
  // same line are matched independently; [\s\S] to span newlines.
  const commentSpan = /<!--[\s\S]*?-->/g

  for (const token of tokens) {
    if (token.type === 'html') {
      const trimmed = token.raw.trimStart()
      if (trimmed.startsWith('<!--') && trimmed.includes('-->')) {
        // Per CommonMark, a type-2 HTML block ends at the *line* containing
        // `-->`, so text after `-->` on that line is part of this token.
        // Strip only the comment spans and keep any residual content.
        const residue = token.raw.replace(commentSpan, '')
        stripped = true
        if (residue.trim().length > 0) {
          // Residual content exists (e.g. `<!-- note --> Use bun`): keep it.
          result += residue
        }
        continue
      }
    }
    result += token.raw
  }

  return { content: result, stripped }
}

/**
 * Parses raw memory file content into a MemoryFileInfo. Pure function — no I/O.
 *
 * When includeBasePath is given, @include paths are resolved in the same lex
 * pass and returned alongside the parsed file (so processMemoryFile doesn't
 * need to lex the same content a second time).
 */
function parseMemoryFileContent(
  rawContent: string,
  filePath: string,
  type: MemoryType,
  includeBasePath?: string,
): { info: MemoryFileInfo | null; includePaths: string[] } {
  // Skip non-text files to prevent loading binary data (images, PDFs, etc.) into memory
  const ext = extname(filePath).toLowerCase()
  if (ext && !TEXT_FILE_EXTENSIONS.has(ext)) {
    logForDebugging(`Skipping non-text file in @include: ${filePath}`)
    return { info: null, includePaths: [] }
  }

  const { content: withoutFrontmatter, paths } =
    parseFrontmatterPaths(rawContent)

  // Lex once so strip and @include-extract share the same tokens. gfm:false
  // is required by extract (so ~/path doesn't tokenize as strikethrough) and
  // doesn't affect strip (html blocks are a CommonMark rule).
  const hasComment = withoutFrontmatter.includes('<!--')
  const tokens =
    hasComment || includeBasePath !== undefined
      ? new Lexer({ gfm: false }).lex(withoutFrontmatter)
      : undefined

  // Only rebuild via tokens when a comment actually needs stripping —
  // marked normalises \r\n during lex, so round-tripping a CRLF file
  // through token.raw would spuriously flip contentDiffersFromDisk.
  const strippedContent =
    hasComment && tokens
      ? stripHtmlCommentsFromTokens(tokens).content
      : withoutFrontmatter

  const includePaths =
    tokens && includeBasePath !== undefined
      ? extractIncludePathsFromTokens(tokens, includeBasePath)
      : []

  // Truncate MEMORY.md entrypoints to the line AND byte caps
  let finalContent = strippedContent
  if (type === 'AutoMem' || type === 'TeamMem') {
    finalContent = truncateEntrypointContent(strippedContent).content
  }

  // Covers frontmatter strip, HTML comment strip, and MEMORY.md truncation
  const contentDiffersFromDisk = finalContent !== rawContent
  return {
    info: {
      path: filePath,
      type,
      content: finalContent,
      globs: paths,
      contentDiffersFromDisk,
      rawContent: contentDiffersFromDisk ? rawContent : undefined,
    },
    includePaths,
  }
}

function handleMemoryFileReadError(error: unknown, filePath: string): void {
  const code = getErrnoCode(error)
  // ENOENT = file doesn't exist, EISDIR = is a directory — both expected
  if (code === 'ENOENT' || code === 'EISDIR') {
    return
  }
  // Log permission errors (EACCES) as they're actionable
  if (code === 'EACCES') {
    // Don't log the full file path to avoid PII/security issues
    logEvent('tengu_crabcode_md_permission_error', {
      is_access_error: 1,
      has_home_dir: filePath.includes(getCrabCodeConfigHomeDir()) ? 1 : 0,
    })
  }
}

/**
 * Used by processMemoryFile → getMemoryFiles so the event loop stays
 * responsive during the directory walk (many readFile attempts, most
 * ENOENT). When includeBasePath is given, @include paths are resolved in
 * the same lex pass and returned alongside the parsed file.
 */
async function safelyReadMemoryFileAsync(
  filePath: string,
  type: MemoryType,
  includeBasePath?: string,
): Promise<{ info: MemoryFileInfo | null; includePaths: string[] }> {
  try {
    const fs = getFsImplementation()
    const rawContent = await fs.readFile(filePath, { encoding: 'utf-8' })
    return parseMemoryFileContent(rawContent, filePath, type, includeBasePath)
  } catch (error) {
    handleMemoryFileReadError(error, filePath)
    return { info: null, includePaths: [] }
  }
}

// W-MEMORY-LIFECYCLE K9 (2026-07-09) — pinned knowledge injection caps.
// Knowledge entries opt into system-prompt injection via frontmatter
// `injection: always`; `auto` (the default) entries surface only through
// memory.search recall, `manual` only when the user references them
// explicitly. Hard caps keep the always-on injection bounded.
const KNOWLEDGE_PINNED_MAX_ENTRIES = 3
const KNOWLEDGE_PINNED_MAX_CHARS = 4096

/**
 * Scan the personal knowledge base root (`<base>/knowledge/*.md`, top level
 * only) for pinned entries: frontmatter `injection: always` and not disabled
 * (`enabled !== false`). Returns AutoMem-typed MemoryFileInfo entries capped
 * at KNOWLEDGE_PINNED_MAX_ENTRIES (newest mtime wins when over budget) with
 * each body capped at KNOWLEDGE_PINNED_MAX_CHARS. Fail-soft throughout: a
 * missing root or unreadable entry contributes nothing.
 */
async function getPinnedKnowledgeEntries(): Promise<MemoryFileInfo[]> {
  const fs = getFsImplementation()
  const knowledgeDir = getKnowledgeDir()

  let entries: import('fs').Dirent[]
  try {
    entries = await fs.readdir(knowledgeDir)
  } catch {
    // ENOENT / ENOTDIR / EACCES — no knowledge root, nothing pinned.
    return []
  }

  const candidates: {
    path: string
    mtimeMs: number
    content: string
    rawContent: string
  }[] = []
  for (const entry of entries) {
    if (!entry.isFile() || !entry.name.endsWith('.md')) continue
    const entryPath = join(knowledgeDir, entry.name)
    try {
      const rawContent = await fs.readFile(entryPath, { encoding: 'utf-8' })
      const { frontmatter, content } = parseFrontmatter(rawContent, entryPath)
      if (frontmatter.enabled === false) continue
      if (frontmatter.injection !== 'always') continue
      const st = await fs.stat(entryPath)
      candidates.push({
        path: entryPath,
        mtimeMs: st.mtimeMs,
        content,
        rawContent,
      })
    } catch (error) {
      handleMemoryFileReadError(error, entryPath)
    }
  }

  // Over-budget pinning keeps the freshest entries; path tiebreak keeps the
  // selection deterministic when mtimes collide.
  candidates.sort(
    (a, b) => b.mtimeMs - a.mtimeMs || a.path.localeCompare(b.path),
  )

  return candidates.slice(0, KNOWLEDGE_PINNED_MAX_ENTRIES).map(candidate => {
    const truncated = candidate.content.length > KNOWLEDGE_PINNED_MAX_CHARS
    const body = truncated
      ? candidate.content.slice(0, KNOWLEDGE_PINNED_MAX_CHARS) +
        `\n\n> [knowledge entry truncated at ${KNOWLEDGE_PINNED_MAX_CHARS} characters — read the full file at: ${candidate.path}]`
      : candidate.content
    // Frontmatter is always stripped for injection, so the injected body
    // differs from disk; keep the raw bytes for the isPartialView cache
    // contract (same as parseMemoryFileContent).
    const contentDiffersFromDisk = body !== candidate.rawContent
    return {
      path: candidate.path,
      type: 'AutoMem' as const,
      content: body,
      contentDiffersFromDisk,
      rawContent: contentDiffersFromDisk ? candidate.rawContent : undefined,
    }
  })
}

type MarkdownToken = {
  type: string
  text?: string
  href?: string
  tokens?: MarkdownToken[]
  raw?: string
  items?: MarkdownToken[]
}

// Extract @path include references from pre-lexed tokens and resolve to
// absolute paths. Skips html tokens so @paths inside block comments are
// ignored — the caller may pass pre-strip tokens.
function extractIncludePathsFromTokens(
  tokens: ReturnType<Lexer['lex']>,
  basePath: string,
): string[] {
  const absolutePaths = new Set<string>()

  // Extract @paths from a text string and add resolved paths to absolutePaths.
  function extractPathsFromText(textContent: string) {
    const includeRegex = /(?:^|\s)@((?:[^\s\\]|\\ )+)/g
    let match
    while ((match = includeRegex.exec(textContent)) !== null) {
      let path = match[1]
      if (!path) continue

      // Strip fragment identifiers (#heading, #section-name, etc.)
      const hashIndex = path.indexOf('#')
      if (hashIndex !== -1) {
        path = path.substring(0, hashIndex)
      }
      if (!path) continue

      // Unescape the spaces in the path
      path = path.replace(/\\ /g, ' ')

      // Accept @path, @./path, @~/path, or @/path
      if (path) {
        const isValidPath =
          path.startsWith('./') ||
          path.startsWith('~/') ||
          (path.startsWith('/') && path !== '/') ||
          (!path.startsWith('@') &&
            !path.match(/^[#%^&*()]+/) &&
            path.match(/^[a-zA-Z0-9._-]/))

        if (isValidPath) {
          const resolvedPath = expandPath(path, dirname(basePath))
          absolutePaths.add(resolvedPath)
        }
      }
    }
  }

  // Recursively process elements to find text nodes
  function processElements(elements: MarkdownToken[]) {
    for (const element of elements) {
      if (element.type === 'code' || element.type === 'codespan') {
        continue
      }

      // For html tokens that contain comments, strip the comment spans and
      // check the residual for @paths (e.g. `<!-- note --> @./file.md`).
      // Other html tokens (non-comment tags) are skipped entirely.
      if (element.type === 'html') {
        const raw = element.raw || ''
        const trimmed = raw.trimStart()
        if (trimmed.startsWith('<!--') && trimmed.includes('-->')) {
          const commentSpan = /<!--[\s\S]*?-->/g
          const residue = raw.replace(commentSpan, '')
          if (residue.trim().length > 0) {
            extractPathsFromText(residue)
          }
        }
        continue
      }

      // Process text nodes
      if (element.type === 'text') {
        extractPathsFromText(element.text || '')
      }

      // Recurse into children tokens
      if (element.tokens) {
        processElements(element.tokens)
      }

      // Special handling for list structures
      if (element.items) {
        processElements(element.items)
      }
    }
  }

  processElements(tokens as MarkdownToken[])
  return [...absolutePaths]
}

const MAX_INCLUDE_DEPTH = 5

/**
 * Checks whether a CRABCODE.md file path is excluded by the crabcodeMdExcludes setting.
 * Only applies to User, Project, and Local memory types.
 * Managed, AutoMem, and TeamMem types are never excluded.
 *
 * Matches both the original path and the realpath-resolved path to handle symlinks
 * (e.g., /tmp -> /private/tmp on macOS).
 */
export function isCrabcodeMdExcluded(filePath: string, type: MemoryType): boolean {
  if (type !== 'User' && type !== 'Project' && type !== 'Local') {
    return false
  }

  const patterns = getInitialSettings().crabcodeMdExcludes
  if (!patterns || patterns.length === 0) {
    return false
  }

  const matchOpts = { dot: true }
  const normalizedPath = filePath.replaceAll('\\', '/')

  // Build an expanded pattern list that includes realpath-resolved versions of
  // absolute patterns. This handles symlinks like /tmp -> /private/tmp on macOS:
  // the user writes "/tmp/project/CRABCODE.md" in their exclude, but the system
  // resolves the CWD to "/private/tmp/project/...", so the file path uses the
  // real path. By resolving the patterns too, both sides match.
  const expandedPatterns = resolveExcludePatterns(patterns).filter(
    p => p.length > 0,
  )
  if (expandedPatterns.length === 0) {
    return false
  }

  return picomatch.isMatch(normalizedPath, expandedPatterns, matchOpts)
}

/**
 * Expands exclude patterns by resolving symlinks in absolute path prefixes.
 * For each absolute pattern (starting with /), tries to resolve the longest
 * existing directory prefix via realpathSync and adds the resolved version.
 * Glob patterns (containing *) have their static prefix resolved.
 */
function resolveExcludePatterns(patterns: string[]): string[] {
  const fs = getFsImplementation()
  const expanded: string[] = patterns.map(p => p.replaceAll('\\', '/'))

  for (const normalized of expanded) {
    // Only resolve absolute patterns — glob-only patterns like "**/*.md" don't have
    // a filesystem prefix to resolve
    if (!normalized.startsWith('/')) {
      continue
    }

    // Find the static prefix before any glob characters
    const globStart = normalized.search(/[*?{[]/)
    const staticPrefix =
      globStart === -1 ? normalized : normalized.slice(0, globStart)
    const dirToResolve = dirname(staticPrefix)

    try {
      // sync IO: called from sync context (isCrabcodeMdExcluded -> processMemoryFile -> getMemoryFiles)
      const resolvedDir = fs.realpathSync(dirToResolve).replaceAll('\\', '/')
      if (resolvedDir !== dirToResolve) {
        const resolvedPattern =
          resolvedDir + normalized.slice(dirToResolve.length)
        expanded.push(resolvedPattern)
      }
    } catch {
      // Directory doesn't exist; skip resolution for this pattern
    }
  }

  return expanded
}

/**
 * Recursively processes a memory file and all its @include references
 * Returns an array of MemoryFileInfo objects with includes first, then main file
 */
export async function processMemoryFile(
  filePath: string,
  type: MemoryType,
  processedPaths: Set<string>,
  includeExternal: boolean,
  depth: number = 0,
  parent?: string,
): Promise<MemoryFileInfo[]> {
  // Skip if already processed or max depth exceeded.
  // Normalize paths for comparison to handle Windows drive letter casing
  // differences (e.g., C:\Users vs c:\Users).
  const normalizedPath = normalizePathForComparison(filePath)
  if (processedPaths.has(normalizedPath) || depth >= MAX_INCLUDE_DEPTH) {
    return []
  }

  // Skip if path is excluded by crabcodeMdExcludes setting
  if (isCrabcodeMdExcluded(filePath, type)) {
    return []
  }

  // Resolve symlink path early for @import resolution
  const { resolvedPath, isSymlink } = safeResolvePath(
    getFsImplementation(),
    filePath,
  )

  processedPaths.add(normalizedPath)
  if (isSymlink) {
    processedPaths.add(normalizePathForComparison(resolvedPath))
  }

  const { info: memoryFile, includePaths: resolvedIncludePaths } =
    await safelyReadMemoryFileAsync(filePath, type, resolvedPath)
  if (!memoryFile || !memoryFile.content.trim()) {
    return []
  }

  // Add parent information
  if (parent) {
    memoryFile.parent = parent
  }

  const result: MemoryFileInfo[] = []

  // Add the main file first (parent before children)
  result.push(memoryFile)

  for (const resolvedIncludePath of resolvedIncludePaths) {
    const isExternal = !pathInOriginalCwd(resolvedIncludePath)
    if (isExternal && !includeExternal) {
      continue
    }

    // Recursively process included files with this file as parent
    const includedFiles = await processMemoryFile(
      resolvedIncludePath,
      type,
      processedPaths,
      includeExternal,
      depth + 1,
      filePath, // Pass current file as parent
    )
    result.push(...includedFiles)
  }

  return result
}

/**
 * Processes all .md files in the .crabcode/rules/ directory and its subdirectories
 * @param rulesDir The path to the rules directory
 * @param type Type of memory file (User, Project, Local)
 * @param processedPaths Set of already processed file paths
 * @param includeExternal Whether to include external files
 * @param conditionalRule If true, only include files with frontmatter paths; if false, only include files without frontmatter paths
 * @param visitedDirs Set of already visited directory real paths (for cycle detection)
 * @returns Array of MemoryFileInfo objects
 */
export async function processMdRules({
  rulesDir,
  type,
  processedPaths,
  includeExternal,
  conditionalRule,
  visitedDirs = new Set(),
}: {
  rulesDir: string
  type: MemoryType
  processedPaths: Set<string>
  includeExternal: boolean
  conditionalRule: boolean
  visitedDirs?: Set<string>
}): Promise<MemoryFileInfo[]> {
  if (visitedDirs.has(rulesDir)) {
    return []
  }

  try {
    const fs = getFsImplementation()

    const { resolvedPath: resolvedRulesDir, isSymlink } = safeResolvePath(
      fs,
      rulesDir,
    )

    visitedDirs.add(rulesDir)
    if (isSymlink) {
      visitedDirs.add(resolvedRulesDir)
    }

    const result: MemoryFileInfo[] = []
    let entries: import('fs').Dirent[]
    try {
      entries = await fs.readdir(resolvedRulesDir)
    } catch (e: unknown) {
      const code = getErrnoCode(e)
      if (code === 'ENOENT' || code === 'EACCES' || code === 'ENOTDIR') {
        return []
      }
      throw e
    }

    for (const entry of entries) {
      const entryPath = join(rulesDir, entry.name)
      const { resolvedPath: resolvedEntryPath, isSymlink } = safeResolvePath(
        fs,
        entryPath,
      )

      // Use Dirent methods for non-symlinks to avoid extra stat calls.
      // For symlinks, we need stat to determine what the target is.
      const stats = isSymlink ? await fs.stat(resolvedEntryPath) : null
      const isDirectory = stats ? stats.isDirectory() : entry.isDirectory()
      const isFile = stats ? stats.isFile() : entry.isFile()

      if (isDirectory) {
        result.push(
          ...(await processMdRules({
            rulesDir: resolvedEntryPath,
            type,
            processedPaths,
            includeExternal,
            conditionalRule,
            visitedDirs,
          })),
        )
      } else if (isFile && entry.name.endsWith('.md')) {
        const files = await processMemoryFile(
          resolvedEntryPath,
          type,
          processedPaths,
          includeExternal,
        )
        result.push(
          ...files.filter(f => (conditionalRule ? f.globs : !f.globs)),
        )
      }
    }

    return result
  } catch (error) {
    if (error instanceof Error && error.message.includes('EACCES')) {
      logEvent('tengu_crabcode_rules_md_permission_error', {
        is_access_error: 1,
        has_home_dir: rulesDir.includes(getCrabCodeConfigHomeDir()) ? 1 : 0,
      })
    }
    return []
  }
}

// Internal memoized implementation. The public `getMemoryFiles` below
// wraps this with an mtime fingerprint guard (W-PROMPT-CACHE-WATCHER
// P1-11) so external editors (VS Code / vim / git pull) that modify
// memory files without going through FileEditTool / FileWriteTool also
// invalidate the project-rules cache stack on the next call.
const _getMemoryFilesImpl = memoize(
  async (forceIncludeExternal: boolean = false): Promise<MemoryFileInfo[]> => {
    const startTime = Date.now()
    logForDiagnosticsNoPII('info', 'memory_files_started')

    const result: MemoryFileInfo[] = []
    const processedPaths = new Set<string>()
    const config = getCurrentProjectConfig()
    const includeExternal =
      forceIncludeExternal ||
      config.hasCrabcodeMdExternalIncludesApproved ||
      false

    // Process Managed file first (always loaded - policy settings)
    const managedCrabcodeMd = getMemoryPath('Managed')
    result.push(
      ...(await processMemoryFile(
        managedCrabcodeMd,
        'Managed',
        processedPaths,
        includeExternal,
      )),
    )
    // Process Managed .crabcode/rules/*.md files
    const managedCrabCodeRulesDir = getManagedCrabCodeRulesDir()
    result.push(
      ...(await processMdRules({
        rulesDir: managedCrabCodeRulesDir,
        type: 'Managed',
        processedPaths,
        includeExternal,
        conditionalRule: false,
      })),
    )

    // Process User file (only if userSettings is enabled)
    if (isSettingSourceEnabled('userSettings')) {
      const userCrabcodeMd = getMemoryPath('User')
      result.push(
        ...(await processMemoryFile(
          userCrabcodeMd,
          'User',
          processedPaths,
          true, // User memory can always include external files
        )),
      )
      // Process User ~/.crabcode/rules/*.md files
      const userCrabCodeRulesDir = getUserCrabCodeRulesDir()
      result.push(
        ...(await processMdRules({
          rulesDir: userCrabCodeRulesDir,
          type: 'User',
          processedPaths,
          includeExternal: true,
          conditionalRule: false,
        })),
      )
    }

    // Then process Project and Local files
    const dirs: string[] = []
    const originalCwd = getOriginalCwd()
    let currentDir = originalCwd

    while (currentDir !== parse(currentDir).root) {
      dirs.push(currentDir)
      currentDir = dirname(currentDir)
    }

    // When running from a git worktree nested inside its main repo (e.g.,
    // .crabcode/worktrees/<name>/ from `crabcode -w`), the upward walk passes
    // through both the worktree root and the main repo root. Both contain
    // checked-in files like CRABCODE.md and .crabcode/rules/*.md, so the same
    // content gets loaded twice. Skip Project-type (checked-in) files from
    // directories above the worktree but within the main repo — the worktree
    // already has its own checkout. CRABCODE.local.md is gitignored so it only
    // exists in the main repo and is still loaded.
    // See: https://github.com/acosmi/crabcode/issues/29599
    const gitRoot = findGitRoot(originalCwd)
    const canonicalRoot = findCanonicalGitRoot(originalCwd)
    const isNestedWorktree =
      gitRoot !== null &&
      canonicalRoot !== null &&
      normalizePathForComparison(gitRoot) !==
        normalizePathForComparison(canonicalRoot) &&
      pathInWorkingPath(gitRoot, canonicalRoot)

    // Process from root downward to CWD
    for (const dir of dirs.reverse()) {
      // In a nested worktree, skip checked-in files from the main repo's
      // working tree (dirs inside canonicalRoot but outside the worktree).
      const skipProject =
        isNestedWorktree &&
        pathInWorkingPath(dir, canonicalRoot) &&
        !pathInWorkingPath(dir, gitRoot)

      // Try reading CRABCODE.md (Project) - only if projectSettings is enabled
      if (isSettingSourceEnabled('projectSettings') && !skipProject) {
        const projectPath = join(dir, 'CRABCODE.md')
        result.push(
          ...(await processMemoryFile(
            projectPath,
            'Project',
            processedPaths,
            includeExternal,
          )),
        )

        // Try reading .crabcode/CRABCODE.md (Project)
        const dotCrabCodePath = join(dir, '.crabcode', 'CRABCODE.md')
        result.push(
          ...(await processMemoryFile(
            dotCrabCodePath,
            'Project',
            processedPaths,
            includeExternal,
          )),
        )

        // Try reading .crabcode/rules/*.md files (Project)
        const rulesDir = join(dir, '.crabcode', 'rules')
        result.push(
          ...(await processMdRules({
            rulesDir,
            type: 'Project',
            processedPaths,
            includeExternal,
            conditionalRule: false,
          })),
        )
      }

      // Try reading CRABCODE.local.md (Local) - only if localSettings is enabled
      if (isSettingSourceEnabled('localSettings')) {
        const localPath = join(dir, 'CRABCODE.local.md')
        result.push(
          ...(await processMemoryFile(
            localPath,
            'Local',
            processedPaths,
            includeExternal,
          )),
        )
      }
    }

    // Process CRABCODE.md from additional directories (--add-dir) if env var is enabled
    // This is controlled by CRABCODE_ADDITIONAL_DIRECTORIES_MD and defaults to off
    // Note: we don't check isSettingSourceEnabled('projectSettings') here because --add-dir
    // is an explicit user action and the SDK defaults settingSources to [] when not specified
    if (isEnvTruthy(process.env.CRABCODE_ADDITIONAL_DIRECTORIES_MD)) {
      const additionalDirs = getAdditionalDirectoriesForCrabcodeMd()
      for (const dir of additionalDirs) {
        // Try reading CRABCODE.md from the additional directory
        const projectPath = join(dir, 'CRABCODE.md')
        result.push(
          ...(await processMemoryFile(
            projectPath,
            'Project',
            processedPaths,
            includeExternal,
          )),
        )

        // Try reading .crabcode/CRABCODE.md from the additional directory
        const dotCrabCodePath = join(dir, '.crabcode', 'CRABCODE.md')
        result.push(
          ...(await processMemoryFile(
            dotCrabCodePath,
            'Project',
            processedPaths,
            includeExternal,
          )),
        )

        // Try reading .crabcode/rules/*.md files from the additional directory
        const rulesDir = join(dir, '.crabcode', 'rules')
        result.push(
          ...(await processMdRules({
            rulesDir,
            type: 'Project',
            processedPaths,
            includeExternal,
            conditionalRule: false,
          })),
        )
      }
    }

    // Memdir entrypoint (memory.md) - only if feature is on and file exists
    if (isAutoMemoryEnabled()) {
      const { info: memdirEntry } = await safelyReadMemoryFileAsync(
        getAutoMemEntrypoint(),
        'AutoMem',
      )
      if (memdirEntry) {
        const normalizedPath = normalizePathForComparison(memdirEntry.path)
        if (!processedPaths.has(normalizedPath)) {
          processedPaths.add(normalizedPath)
          result.push(memdirEntry)
        }
      }
    }

    // User-global memory entrypoint (<base>/memory/MEMORY.md) — cross-session
    // memory visible regardless of the anchored project (W-MEMORY-LIFECYCLE
    // K4). Mirrors the per-project AutoMem block above: same AutoMem type,
    // same processedPaths dedup (a project whose memory dir resolves to the
    // global root is injected once), and a missing file is silently skipped
    // by safelyReadMemoryFileAsync.
    if (isAutoMemoryEnabled()) {
      const { info: globalMemEntry } = await safelyReadMemoryFileAsync(
        getGlobalMemEntrypoint(),
        'AutoMem',
      )
      if (globalMemEntry) {
        const normalizedPath = normalizePathForComparison(globalMemEntry.path)
        if (!processedPaths.has(normalizedPath)) {
          processedPaths.add(normalizedPath)
          result.push(globalMemEntry)
        }
      }
    }

    // Pinned knowledge entries (<base>/knowledge/*.md with frontmatter
    // `injection: always`) — the user's opt-in slice of the personal knowledge
    // base that rides along in every session (W-MEMORY-LIFECYCLE K9). Same
    // AutoMem type + processedPaths dedup as the memory entrypoints above;
    // `injection: auto` (default) entries surface via memory.search recall
    // instead, `manual` only on explicit user reference. Hard-capped at
    // KNOWLEDGE_PINNED_MAX_ENTRIES entries / KNOWLEDGE_PINNED_MAX_CHARS each.
    if (isAutoMemoryEnabled()) {
      for (const pinnedEntry of await getPinnedKnowledgeEntries()) {
        const normalizedPath = normalizePathForComparison(pinnedEntry.path)
        if (!processedPaths.has(normalizedPath)) {
          processedPaths.add(normalizedPath)
          result.push(pinnedEntry)
        }
      }
    }

    // Team memory entrypoint - only if feature is on and file exists
    if (feature('TEAMMEM') && teamMemPaths!.isTeamMemoryEnabled()) {
      const { info: teamMemEntry } = await safelyReadMemoryFileAsync(
        teamMemPaths!.getTeamMemEntrypoint(),
        'TeamMem',
      )
      if (teamMemEntry) {
        const normalizedPath = normalizePathForComparison(teamMemEntry.path)
        if (!processedPaths.has(normalizedPath)) {
          processedPaths.add(normalizedPath)
          result.push(teamMemEntry)
        }
      }
    }

    const totalContentLength = result.reduce(
      (sum, f) => sum + f.content.length,
      0,
    )

    logForDiagnosticsNoPII('info', 'memory_files_completed', {
      duration_ms: Date.now() - startTime,
      file_count: result.length,
      total_content_length: totalContentLength,
    })

    const typeCounts: Record<string, number> = {}
    for (const f of result) {
      typeCounts[f.type] = (typeCounts[f.type] ?? 0) + 1
    }

    if (!hasLoggedInitialLoad) {
      hasLoggedInitialLoad = true
      logEvent('tengu_crabcodemd__initial_load', {
        file_count: result.length,
        total_content_length: totalContentLength,
        user_count: typeCounts['User'] ?? 0,
        project_count: typeCounts['Project'] ?? 0,
        local_count: typeCounts['Local'] ?? 0,
        managed_count: typeCounts['Managed'] ?? 0,
        automem_count: typeCounts['AutoMem'] ?? 0,
        ...(feature('TEAMMEM')
          ? { teammem_count: typeCounts['TeamMem'] ?? 0 }
          : {}),
        duration_ms: Date.now() - startTime,
      })
    }

    // Fire InstructionsLoaded hook for each instruction file loaded
    // (fire-and-forget, audit/observability only).
    // AutoMem/TeamMem are intentionally excluded — they're a separate
    // memory system, not "instructions" in the CRABCODE.md/rules sense.
    // Gated on !forceIncludeExternal: the forceIncludeExternal=true variant
    // is only used by getExternalCrabcodeMdIncludes() for approval checks, not
    // for building context — firing the hook there would double-fire on startup.
    // The one-shot flag is consumed on every !forceIncludeExternal cache miss
    // (NOT gated on hasInstructionsLoadedHook) so the flag is released even
    // when no hook is configured — otherwise a mid-session hook registration
    // followed by a direct .cache.clear() would spuriously fire with a stale
    // 'session_start' reason.
    if (!forceIncludeExternal) {
      const eagerLoadReason = consumeNextEagerLoadReason()
      if (eagerLoadReason !== undefined && hasInstructionsLoadedHook()) {
        for (const file of result) {
          if (!isInstructionsMemoryType(file.type)) continue
          const loadReason = file.parent ? 'include' : eagerLoadReason
          void executeInstructionsLoadedHooks(
            file.path,
            file.type,
            loadReason,
            {
              globs: file.globs,
              parentFilePath: file.parent,
            },
          )
        }
      }
    }

    return result
  },
)

// ============================================================================
// W-PROMPT-CACHE-WATCHER (P1-11): mtime fingerprint guard for external edits
// ============================================================================
//
// Root cause this fixes: external editors (VS Code / vim / git pull) mutate
// CRABCODE.md / .crabcode/rules/*.md without going through FileEditTool /
// FileWriteTool / `memory` command, so nothing invalidates the lodash
// memoize cache — it keeps returning stale content until `/compact` or
// process restart.
//
// Strategy: at every `getMemoryFiles` call, take a cheap path-list +
// `mtimeMs` snapshot of all candidate memory file paths. If the snapshot
// differs from the last one (and a previous snapshot existed), clear the
// memoize cache before delegating to the underlying memoized impl. Cost ≈
// O(N) fs.stat calls where N is the count of memory candidates — typically
// 20-60 stat calls for a 4-deep project tree, negligible relative to the
// readFile directory walk the impl itself performs.
// The fingerprint guard only fires when nothing else has invalidated.

let _lastFingerprint: string | null = null

// Exported so the system-prompt section cache key (prompts.ts
// `projectRulesFingerprint`) can fold the same mtime snapshot into its
// segment name. Without this, external edits to CLAUDE.md / AGENTS.md /
// CRABCODE.md / .crabcode/rules/**.md invalidate the `getMemoryFiles`
// memoize cache but the **systemPromptSectionCache** keeps returning the
// stale `project_rules` section body (W-PROMPT-DEFAULT-AS-APPEND DRIFT-10).
export async function computeProjectRulesFingerprint(): Promise<string> {
  const fs = getFsImplementation()

  // Collect path candidates in the same order as `_getMemoryFilesImpl` walks
  // them, so the fingerprint is deterministic and stable across calls when
  // the file system hasn't changed.
  const paths: string[] = []

  // Managed
  paths.push(getMemoryPath('Managed'))
  const managedRulesDir = getManagedCrabCodeRulesDir()
  paths.push(...(await listMarkdownFilesDeep(fs, managedRulesDir)))

  // User
  if (isSettingSourceEnabled('userSettings')) {
    paths.push(getMemoryPath('User'))
    const userRulesDir = getUserCrabCodeRulesDir()
    paths.push(...(await listMarkdownFilesDeep(fs, userRulesDir)))
  }

  // Project / Local — walk upward from CWD to root
  const originalCwd = getOriginalCwd()
  let currentDir = originalCwd
  while (currentDir !== parse(currentDir).root) {
    paths.push(join(currentDir, 'CRABCODE.md'))
    paths.push(join(currentDir, '.crabcode', 'CRABCODE.md'))
    paths.push(join(currentDir, 'CRABCODE.local.md'))
    const rulesDir = join(currentDir, '.crabcode', 'rules')
    paths.push(...(await listMarkdownFilesDeep(fs, rulesDir)))
    currentDir = dirname(currentDir)
  }

  // Additional --add-dir CRABCODE.md
  if (isEnvTruthy(process.env.CRABCODE_ADDITIONAL_DIRECTORIES_MD)) {
    for (const dir of getAdditionalDirectoriesForCrabcodeMd()) {
      paths.push(join(dir, 'CRABCODE.md'))
      paths.push(join(dir, '.crabcode', 'CRABCODE.md'))
      const rulesDir = join(dir, '.crabcode', 'rules')
      paths.push(...(await listMarkdownFilesDeep(fs, rulesDir)))
    }
  }

  // AutoMem / TeamMem entrypoints
  if (isAutoMemoryEnabled()) {
    paths.push(getAutoMemEntrypoint())
    // W-MEMORY-LIFECYCLE K4/K9 (2026-07-09): the user-global memory
    // entrypoint and pinned-knowledge candidates are injected by
    // _getMemoryFilesImpl, so edits from OUTSIDE this process (typically the
    // another local process via knowledge CRUD / memory promote) must flip this
    // fingerprint too — otherwise a mid-session create/promote keeps serving
    // the stale memoized prompt until an unrelated rules file changes.
    // Knowledge fingerprinting reuses the capped deep walker (superset of the
    // reader's top-level-only scan — over-invalidation is safe, a miss is not).
    paths.push(getGlobalMemEntrypoint())
    paths.push(...(await listMarkdownFilesDeep(fs, getKnowledgeDir())))
  }
  if (feature('TEAMMEM') && teamMemPaths!.isTeamMemoryEnabled()) {
    paths.push(teamMemPaths!.getTeamMemEntrypoint())
  }

  // Sort to make fingerprint order-stable even if the upstream walk order
  // ever drifts. We also de-dup to keep the string compact.
  const uniqSorted = Array.from(new Set(paths)).sort()

  // Build `path:mtimeMs` segments; missing files contribute `path:0` so
  // creation/deletion both flip the fingerprint.
  const segments: string[] = []
  for (const p of uniqSorted) {
    let mtime = 0
    try {
      const st = await fs.stat(p)
      // Files only — directories are not memory content. (readdir helper
      // above already filtered to .md files, but defense in depth.)
      if (st.isFile()) {
        mtime = Math.floor(st.mtimeMs)
      }
    } catch {
      // ENOENT / EACCES / EISDIR all map to "no contribution" (mtime=0).
      // A later creation flips mtime > 0 → fingerprint changes.
    }
    segments.push(`${p}:${mtime}`)
  }

  return segments.join('|')
}

// W-PROMPT-DEFAULT-AS-APPEND DRIFT-11: fingerprint walker must share the
// reader's recursion shape (processMdRules at :697 walks unbounded depth),
// otherwise a deep edit under `.crabcode/rules/foo/bar/deep.md` (≥3 levels)
// changes content but does not flip the fingerprint → cache returns stale
// section body.
//
// Previously this was a 2-level cap (`Recurse one level deeper`) introduced
// purely as a cost-bound. The cap is replaced with two safer guards that
// keep cost bounded without leaking deep edits:
//   (1) Recursion depth ≤ FINGERPRINT_MAX_DEPTH (5) — defends against
//       symlink loops / pathologically deep trees. processMdRules itself
//       uses `visitedDirs` to break symlink cycles; we mirror that.
//   (2) Total file count ≤ FINGERPRINT_MAX_FILES (100) — defends against
//       a directory with thousands of .md files (a missed mtime on the
//       101st file is acceptable; the user can /memory to force-reload).
const FINGERPRINT_MAX_DEPTH = 5
const FINGERPRINT_MAX_FILES = 100

async function listMarkdownFilesDeep(
  fs: ReturnType<typeof getFsImplementation>,
  rulesDir: string,
): Promise<string[]> {
  const out: string[] = []
  const visitedDirs = new Set<string>()
  await walkMarkdownFiles(fs, rulesDir, 0, visitedDirs, out)
  return out
}

async function walkMarkdownFiles(
  fs: ReturnType<typeof getFsImplementation>,
  dir: string,
  depth: number,
  visitedDirs: Set<string>,
  out: string[],
): Promise<void> {
  if (depth >= FINGERPRINT_MAX_DEPTH) return
  if (out.length >= FINGERPRINT_MAX_FILES) return

  // Cheap symlink cycle defense: resolve once and short-circuit if seen.
  let resolvedDir: string
  try {
    const { resolvedPath } = safeResolvePath(fs, dir)
    resolvedDir = resolvedPath
  } catch {
    return
  }
  if (visitedDirs.has(resolvedDir)) return
  visitedDirs.add(resolvedDir)

  let entries: import('fs').Dirent[]
  try {
    entries = await fs.readdir(resolvedDir)
  } catch {
    // ENOENT / ENOTDIR / EACCES — no contribution.
    return
  }

  for (const entry of entries) {
    if (out.length >= FINGERPRINT_MAX_FILES) return
    const entryPath = join(resolvedDir, entry.name)
    if (entry.isFile() && entry.name.endsWith('.md')) {
      out.push(entryPath)
    } else if (entry.isDirectory()) {
      await walkMarkdownFiles(fs, entryPath, depth + 1, visitedDirs, out)
    } else if (entry.isSymbolicLink()) {
      // Resolve once to decide file vs dir; cycle protection above handles
      // symlink loops via visitedDirs.
      try {
        const stat = await fs.stat(entryPath)
        if (stat.isFile() && entry.name.endsWith('.md')) {
          out.push(entryPath)
        } else if (stat.isDirectory()) {
          await walkMarkdownFiles(fs, entryPath, depth + 1, visitedDirs, out)
        }
      } catch {
        // Broken symlink — skip silently.
      }
    }
  }
}

// Public entry point: fingerprint guard + delegate to memoized impl.
//
// The function is typed with an explicit `cache` property surface so the
// `Object.defineProperty` shim below is visible to callsites that read
// `getMemoryFiles.cache?.clear?.()` directly (clearMemoryFileCaches +
// existing tests).
export interface GetMemoryFilesWithCache {
  (forceIncludeExternal?: boolean): Promise<MemoryFileInfo[]>
  cache?: {
    clear?: () => void
    get?: (k: unknown) => unknown
    set?: (k: unknown, v: unknown) => void
    delete?: (k: unknown) => void
    size?: number
  }
}

export const getMemoryFiles: GetMemoryFilesWithCache = async function (
  forceIncludeExternal: boolean = false,
): Promise<MemoryFileInfo[]> {
  try {
    const fp = await computeProjectRulesFingerprint()
    if (_lastFingerprint !== null && fp !== _lastFingerprint) {
      // External-edit detected (mtime fingerprint changed since the last
      // call) — drop the getMemoryFiles memoize cache so this call reflects
      // the on-disk change instead of serving stale content.
      _getMemoryFilesImpl.cache?.clear?.()
    }
    _lastFingerprint = fp
  } catch {
    // Fingerprint computation should never throw, but if it does, treat
    // it as "unknown state" and let the memoized impl serve its cache.
    // The next successful fingerprint will re-establish the baseline.
  }

  return _getMemoryFilesImpl(forceIncludeExternal)
}

// `.cache` shim: several callsites read `getMemoryFiles.cache.clear?.()`
// directly. Proxy to the impl's cache so existing `clearMemoryFileCaches()`
// and tests keep working without rewiring every callsite.
Object.defineProperty(getMemoryFiles, 'cache', {
  get(): unknown {
    return _getMemoryFilesImpl.cache
  },
  configurable: true,
})

// Test-only: reset the fingerprint baseline. Exported so the unit test can
// simulate "first call after process start" without resorting to module
// re-import gymnastics. Not used outside tests.
export function _resetProjectRulesFingerprintForTesting(): void {
  _lastFingerprint = null
}

function isInstructionsMemoryType(
  type: MemoryType,
): type is InstructionsMemoryType {
  return (
    type === 'User' ||
    type === 'Project' ||
    type === 'Local' ||
    type === 'Managed'
  )
}

// Load reason to report for top-level (non-included) files on the next eager
// getMemoryFiles() pass. Set to 'compact' by resetGetMemoryFilesCache when
// compaction clears the cache, so the InstructionsLoaded hook reports the
// reload correctly instead of misreporting it as 'session_start'. One-shot:
// reset to 'session_start' after being read.
let nextEagerLoadReason: InstructionsLoadReason = 'session_start'

// Whether the InstructionsLoaded hook should fire on the next cache miss.
// true initially (for session_start), consumed after firing, re-enabled only
// by resetGetMemoryFilesCache(). Callers that only need cache invalidation
// for correctness (e.g. worktree enter/exit, settings sync, /memory dialog)
// should use clearMemoryFileCaches() instead to avoid spurious hook fires.
let shouldFireHook = true

function consumeNextEagerLoadReason(): InstructionsLoadReason | undefined {
  if (!shouldFireHook) return undefined
  shouldFireHook = false
  const reason = nextEagerLoadReason
  nextEagerLoadReason = 'session_start'
  return reason
}

/**
 * Clears the getMemoryFiles memoize cache
 * without firing the InstructionsLoaded hook.
 *
 * Use this for cache invalidation that is purely for correctness (e.g.
 * worktree enter/exit, settings sync, /memory dialog). For events that
 * represent instructions actually being reloaded into context (e.g.
 * compaction), use resetGetMemoryFilesCache() instead.
 */
export function clearMemoryFileCaches(): void {
  // ?.cache because tests spyOn this, which replaces the memoize wrapper.
  getMemoryFiles.cache?.clear?.()
}

export function resetGetMemoryFilesCache(
  reason: InstructionsLoadReason = 'session_start',
): void {
  nextEagerLoadReason = reason
  shouldFireHook = true
  clearMemoryFileCaches()
}

export function getLargeMemoryFiles(files: MemoryFileInfo[]): MemoryFileInfo[] {
  return files.filter(f => f.content.length > MAX_MEMORY_CHARACTER_COUNT)
}

/**
 * When tengu_moth_copse is on, the findRelevantMemories prefetch surfaces
 * memory files via attachments, so the MEMORY.md index is no longer injected
 * into the system prompt. Callsites that care about "what's actually in
 * context" (context builder, /context viz) should filter through this.
 */
export function filterInjectedMemoryFiles(
  files: MemoryFileInfo[],
): MemoryFileInfo[] {
  const skipMemoryIndex = getFeatureValue_CACHED_MAY_BE_STALE(
    'tengu_moth_copse',
    false,
  )
  if (!skipMemoryIndex) return files
  return files.filter(f => f.type !== 'AutoMem' && f.type !== 'TeamMem')
}

export const getCrabcodeMds = (
  memoryFiles: MemoryFileInfo[],
  filter?: (type: MemoryType) => boolean,
): string => {
  const memories: string[] = []
  const skipProjectLevel = getFeatureValue_CACHED_MAY_BE_STALE(
    'tengu_paper_halyard',
    false,
  )

  for (const file of memoryFiles) {
    if (filter && !filter(file.type)) continue
    if (skipProjectLevel && (file.type === 'Project' || file.type === 'Local'))
      continue
    if (file.content) {
      const description =
        file.type === 'Project'
          ? ' (project instructions, checked into the codebase)'
          : file.type === 'Local'
            ? " (user's private project instructions, not checked in)"
            : feature('TEAMMEM') && file.type === 'TeamMem'
              ? ' (shared team memory, synced across the organization)'
              : file.type === 'AutoMem'
                ? " (user's auto-memory, persists across conversations)"
                : " (user's private global instructions for all projects)"

      const content = file.content.trim()
      if (feature('TEAMMEM') && file.type === 'TeamMem') {
        memories.push(
          `Contents of ${file.path}${description}:\n\n<team-memory-content source="shared">\n${content}\n</team-memory-content>`,
        )
      } else {
        memories.push(`Contents of ${file.path}${description}:\n\n${content}`)
      }
    }
  }

  if (memories.length === 0) {
    return ''
  }

  return `${MEMORY_INSTRUCTION_PROMPT}\n\n${memories.join('\n\n')}`
}

/**
 * Gets managed and user conditional rules that match the target path.
 * This is the first phase of nested memory loading.
 *
 * @param targetPath The target file path to match against glob patterns
 * @param processedPaths Set of already processed file paths (will be mutated)
 * @returns Array of MemoryFileInfo objects for matching conditional rules
 */
export async function getManagedAndUserConditionalRules(
  targetPath: string,
  processedPaths: Set<string>,
): Promise<MemoryFileInfo[]> {
  const result: MemoryFileInfo[] = []

  // Process Managed conditional .crabcode/rules/*.md files
  const managedCrabCodeRulesDir = getManagedCrabCodeRulesDir()
  result.push(
    ...(await processConditionedMdRules(
      targetPath,
      managedCrabCodeRulesDir,
      'Managed',
      processedPaths,
      false,
    )),
  )

  if (isSettingSourceEnabled('userSettings')) {
    // Process User conditional .crabcode/rules/*.md files
    const userCrabCodeRulesDir = getUserCrabCodeRulesDir()
    result.push(
      ...(await processConditionedMdRules(
        targetPath,
        userCrabCodeRulesDir,
        'User',
        processedPaths,
        true,
      )),
    )
  }

  return result
}

/**
 * Gets memory files for a single nested directory (between CWD and target).
 * Loads CRABCODE.md, unconditional rules, and conditional rules for that directory.
 *
 * @param dir The directory to process
 * @param targetPath The target file path (for conditional rule matching)
 * @param processedPaths Set of already processed file paths (will be mutated)
 * @returns Array of MemoryFileInfo objects
 */
export async function getMemoryFilesForNestedDirectory(
  dir: string,
  targetPath: string,
  processedPaths: Set<string>,
): Promise<MemoryFileInfo[]> {
  const result: MemoryFileInfo[] = []

  // Process project memory files (CRABCODE.md and .crabcode/CRABCODE.md)
  if (isSettingSourceEnabled('projectSettings')) {
    const projectPath = join(dir, 'CRABCODE.md')
    result.push(
      ...(await processMemoryFile(
        projectPath,
        'Project',
        processedPaths,
        false,
      )),
    )
    const dotCrabCodePath = join(dir, '.crabcode', 'CRABCODE.md')
    result.push(
      ...(await processMemoryFile(
        dotCrabCodePath,
        'Project',
        processedPaths,
        false,
      )),
    )
  }

  // Process local memory file (CRABCODE.local.md)
  if (isSettingSourceEnabled('localSettings')) {
    const localPath = join(dir, 'CRABCODE.local.md')
    result.push(
      ...(await processMemoryFile(localPath, 'Local', processedPaths, false)),
    )
  }

  const rulesDir = join(dir, '.crabcode', 'rules')

  // Process project unconditional .crabcode/rules/*.md files, which were not eagerly loaded
  // Use a separate processedPaths set to avoid marking conditional rule files as processed
  const unconditionalProcessedPaths = new Set(processedPaths)
  result.push(
    ...(await processMdRules({
      rulesDir,
      type: 'Project',
      processedPaths: unconditionalProcessedPaths,
      includeExternal: false,
      conditionalRule: false,
    })),
  )

  // Process project conditional .crabcode/rules/*.md files
  result.push(
    ...(await processConditionedMdRules(
      targetPath,
      rulesDir,
      'Project',
      processedPaths,
      false,
    )),
  )

  // processedPaths must be seeded with unconditional paths for subsequent directories
  for (const path of unconditionalProcessedPaths) {
    processedPaths.add(path)
  }

  return result
}

/**
 * Gets conditional rules for a CWD-level directory (from root up to CWD).
 * Only processes conditional rules since unconditional rules are already loaded eagerly.
 *
 * @param dir The directory to process
 * @param targetPath The target file path (for conditional rule matching)
 * @param processedPaths Set of already processed file paths (will be mutated)
 * @returns Array of MemoryFileInfo objects
 */
export async function getConditionalRulesForCwdLevelDirectory(
  dir: string,
  targetPath: string,
  processedPaths: Set<string>,
): Promise<MemoryFileInfo[]> {
  const rulesDir = join(dir, '.crabcode', 'rules')
  return processConditionedMdRules(
    targetPath,
    rulesDir,
    'Project',
    processedPaths,
    false,
  )
}

/**
 * Processes all .md files in the .crabcode/rules/ directory and its subdirectories,
 * filtering to only include files with frontmatter paths that match the target path
 * @param targetPath The file path to match against frontmatter glob patterns
 * @param rulesDir The path to the rules directory
 * @param type Type of memory file (User, Project, Local)
 * @param processedPaths Set of already processed file paths
 * @param includeExternal Whether to include external files
 * @returns Array of MemoryFileInfo objects that match the target path
 */
export async function processConditionedMdRules(
  targetPath: string,
  rulesDir: string,
  type: MemoryType,
  processedPaths: Set<string>,
  includeExternal: boolean,
): Promise<MemoryFileInfo[]> {
  const conditionedRuleMdFiles = await processMdRules({
    rulesDir,
    type,
    processedPaths,
    includeExternal,
    conditionalRule: true,
  })

  // Filter to only include files whose globs patterns match the targetPath
  return conditionedRuleMdFiles.filter(file => {
    if (!file.globs || file.globs.length === 0) {
      return false
    }

    // For Project rules: glob patterns are relative to the directory containing .crabcode
    // For Managed/User rules: glob patterns are relative to the original CWD
    const baseDir =
      type === 'Project'
        ? dirname(dirname(rulesDir)) // Parent of .crabcode
        : getOriginalCwd() // Project root for managed/user rules

    const relativePath = isAbsolute(targetPath)
      ? relative(baseDir, targetPath)
      : targetPath
    // ignore() throws on empty strings, paths escaping the base (../),
    // and absolute paths (Windows cross-drive relative() returns absolute).
    // Files outside baseDir can't match baseDir-relative globs anyway.
    if (
      !relativePath ||
      relativePath.startsWith('..') ||
      isAbsolute(relativePath)
    ) {
      return false
    }
    return ignore().add(file.globs).ignores(relativePath)
  })
}

export type ExternalCrabcodeMdInclude = {
  path: string
  parent: string
}

export function getExternalCrabcodeMdIncludes(
  files: MemoryFileInfo[],
): ExternalCrabcodeMdInclude[] {
  const externals: ExternalCrabcodeMdInclude[] = []
  for (const file of files) {
    if (file.type !== 'User' && file.parent && !pathInOriginalCwd(file.path)) {
      externals.push({ path: file.path, parent: file.parent })
    }
  }
  return externals
}

export function hasExternalCrabcodeMdIncludes(files: MemoryFileInfo[]): boolean {
  return getExternalCrabcodeMdIncludes(files).length > 0
}

export async function shouldShowCrabcodeMdExternalIncludesWarning(): Promise<boolean> {
  const config = getCurrentProjectConfig()
  if (
    config.hasCrabcodeMdExternalIncludesApproved ||
    config.hasCrabcodeMdExternalIncludesWarningShown
  ) {
    return false
  }

  return hasExternalCrabcodeMdIncludes(await getMemoryFiles(true))
}

/**
 * Check if a file path is a memory file (CRABCODE.md, CRABCODE.local.md,
 * or .crabcode/rules/*.md)
 */
export function isMemoryFilePath(filePath: string): boolean {
  const name = basename(filePath)

  // CRABCODE.md or CRABCODE.local.md anywhere
  if (name === 'CRABCODE.md' || name === 'CRABCODE.local.md') {
    return true
  }

  // .md files in .crabcode/rules/ directories
  if (
    name.endsWith('.md') &&
    filePath.includes(`${sep}.crabcode${sep}rules${sep}`)
  ) {
    return true
  }

  return false
}

/**
 * Get all memory file paths from both standard discovery and readFileState.
 * Combines:
 * - getMemoryFiles() paths (CWD upward to root)
 * - readFileState paths matching memory patterns (includes child directories)
 */
export function getAllMemoryFilePaths(
  files: MemoryFileInfo[],
  readFileState: FileStateCache,
): string[] {
  const paths = new Set<string>()
  for (const file of files) {
    if (file.content.trim().length > 0) {
      paths.add(file.path)
    }
  }

  // Add memory files from readFileState (includes child directories)
  for (const filePath of cacheKeys(readFileState)) {
    if (isMemoryFilePath(filePath)) {
      paths.add(filePath)
    }
  }

  return Array.from(paths)
}
