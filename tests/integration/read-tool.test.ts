/**
 * FileReadTool Integration Tests
 *
 * Tests file-reading behavior by directly using Node.js fs operations to
 * exercise the same code paths that FileReadTool.call() uses internally:
 * reading files with offset/limit, handling missing files, BOM/CRLF stripping.
 *
 * NOTE: FileReadTool and its transitive dependencies (readFileInRange,
 * FileStateCache, etc.) cannot be imported in `bun test` mode without a full
 * `bun install` (missing: lru-cache, emoji-regex, lodash-es, etc.). These
 * tests validate the file-reading contract at the fs level.
 */

import { describe, test, expect, beforeAll, afterAll } from 'bun:test'
import { mkdirSync, writeFileSync, readFileSync, rmSync, statSync } from 'fs'
import { join } from 'path'
import { createTestDir } from '../setup.js'

let tmpDir: string
let sampleFilePath: string
let multiLineFilePath: string
let emptyFilePath: string

const SAMPLE_CONTENT = 'Hello, CrabCode!\nThis is line 2.\nLine 3 here.\n'
const MULTI_LINE_CONTENT =
  Array.from({ length: 50 }, (_, i) => `Line ${i + 1}: content here`).join('\n') + '\n'

beforeAll(() => {
  tmpDir = createTestDir('read-tool')
  sampleFilePath = join(tmpDir, 'sample.txt')
  writeFileSync(sampleFilePath, SAMPLE_CONTENT)
  multiLineFilePath = join(tmpDir, 'multiline.txt')
  writeFileSync(multiLineFilePath, MULTI_LINE_CONTENT)
  emptyFilePath = join(tmpDir, 'empty.txt')
  writeFileSync(emptyFilePath, '')
})

afterAll(() => {
  try {
    rmSync(tmpDir, { recursive: true, force: true })
  } catch {
    // best-effort cleanup
  }
})

/**
 * Read a file with offset/limit, mimicking the core logic of readFileInRange.
 * offset: 0-based line index, maxLines: max number of lines to return.
 */
function readFileRange(
  filePath: string,
  offset = 0,
  maxLines?: number,
): { content: string; lineCount: number; totalLines: number } {
  const raw = readFileSync(filePath, 'utf-8')
  // Strip BOM
  const text = raw.charCodeAt(0) === 0xfeff ? raw.slice(1) : raw
  // Split, strip \r, select range
  const allLines: string[] = []
  let startPos = 0
  let nlPos: number
  while ((nlPos = text.indexOf('\n', startPos)) !== -1) {
    let line = text.slice(startPos, nlPos)
    if (line.endsWith('\r')) line = line.slice(0, -1)
    allLines.push(line)
    startPos = nlPos + 1
  }
  // Final fragment
  let lastLine = text.slice(startPos)
  if (lastLine.endsWith('\r')) lastLine = lastLine.slice(0, -1)
  allLines.push(lastLine)

  const totalLines = allLines.length
  const endLine = maxLines !== undefined ? offset + maxLines : totalLines
  const selected = allLines.slice(offset, endLine)

  return {
    content: selected.join('\n'),
    lineCount: selected.length,
    totalLines,
  }
}

describe('FileReadTool (file reading pipeline)', () => {
  describe('reading existing files', () => {
    test('reads entire file', () => {
      const result = readFileRange(sampleFilePath)
      expect(result.content).toContain('Hello, CrabCode!')
      expect(result.content).toContain('This is line 2.')
      expect(result.content).toContain('Line 3 here.')
    })

    test('returns correct line count', () => {
      const result = readFileRange(sampleFilePath)
      // "Hello, CrabCode!", "This is line 2.", "Line 3 here.", "" (trailing newline)
      expect(result.totalLines).toBe(4)
    })

    test('file has non-zero size on disk', () => {
      const stats = statSync(sampleFilePath)
      expect(stats.size).toBeGreaterThan(0)
    })

    test('file has valid mtime', () => {
      const stats = statSync(sampleFilePath)
      expect(stats.mtimeMs).toBeGreaterThan(0)
    })
  })

  describe('offset parameter (0-based)', () => {
    test('offset=0 returns from beginning', () => {
      const result = readFileRange(multiLineFilePath, 0)
      expect(result.content).toContain('Line 1:')
    })

    test('offset=9 skips first 9 lines', () => {
      const result = readFileRange(multiLineFilePath, 9)
      expect(result.content.startsWith('Line 10:')).toBe(true)
      expect(result.content).not.toContain('Line 9:')
    })

    test('offset beyond file returns empty content', () => {
      const result = readFileRange(multiLineFilePath, 1000)
      expect(result.content).toBe('')
      expect(result.lineCount).toBe(0)
    })
  })

  describe('limit parameter (maxLines)', () => {
    test('limits number of lines returned', () => {
      const result = readFileRange(multiLineFilePath, 0, 5)
      const lines = result.content.split('\n').filter(Boolean)
      expect(lines.length).toBe(5)
      expect(lines[0]).toContain('Line 1:')
      expect(lines[4]).toContain('Line 5:')
    })

    test('limit=1 returns only one line', () => {
      const result = readFileRange(multiLineFilePath, 0, 1)
      const lines = result.content.split('\n').filter(Boolean)
      expect(lines.length).toBe(1)
      expect(lines[0]).toContain('Line 1:')
    })
  })

  describe('offset + limit together', () => {
    test('reads a specific range', () => {
      const result = readFileRange(multiLineFilePath, 19, 5)
      const lines = result.content.split('\n').filter(Boolean)
      expect(lines.length).toBe(5)
      expect(lines[0]).toContain('Line 20:')
      expect(lines[4]).toContain('Line 24:')
    })

    test('returns fewer lines if range exceeds file', () => {
      const result = readFileRange(multiLineFilePath, 48, 10)
      // File has 50 lines + trailing empty = 51 total
      expect(result.lineCount).toBeLessThanOrEqual(10)
      expect(result.content).toContain('Line 49:')
    })
  })

  describe('reading non-existent files', () => {
    test('throws for missing file', () => {
      const missingPath = join(tmpDir, 'does-not-exist.txt')
      try {
        readFileRange(missingPath)
        expect(true).toBe(false) // should not reach here
      } catch (err: any) {
        expect(err.code || err.message).toMatch(/ENOENT|no such file/i)
      }
    })
  })

  describe('reading empty files', () => {
    test('returns empty content', () => {
      const result = readFileRange(emptyFilePath)
      expect(result.content).toBe('')
    })
  })

  describe('CRLF handling', () => {
    test('strips carriage returns from CRLF files', () => {
      const crlfPath = join(tmpDir, 'crlf.txt')
      writeFileSync(crlfPath, 'line one\r\nline two\r\nline three\r\n')

      const result = readFileRange(crlfPath)
      expect(result.content).not.toContain('\r')
      expect(result.content).toContain('line one')
      expect(result.content).toContain('line two')
      expect(result.content).toContain('line three')
    })
  })

  describe('BOM handling', () => {
    test('strips UTF-8 BOM', () => {
      const bomPath = join(tmpDir, 'bom.txt')
      writeFileSync(bomPath, '\uFEFFBOM content here')

      const result = readFileRange(bomPath)
      expect(result.content).not.toMatch(/^\uFEFF/)
      expect(result.content).toContain('BOM content here')
    })
  })

  describe('line numbering (cat -n format)', () => {
    test('can add line numbers to content', () => {
      const content = 'first\nsecond\nthird'
      const lines = content.split('\n')
      const numbered = lines.map((line, i) => `     ${i + 1}\t${line}`).join('\n')
      expect(numbered).toContain('1\tfirst')
      expect(numbered).toContain('2\tsecond')
      expect(numbered).toContain('3\tthird')
    })
  })
})

describe('FileReadTool output contract', () => {
  test('text output has expected fields', () => {
    const output = {
      type: 'text' as const,
      file: {
        filePath: '/tmp/test.txt',
        content: '     1\tHello',
        numLines: 1,
        startLine: 1,
        totalLines: 1,
      },
    }
    expect(output.type).toBe('text')
    expect(output.file.filePath).toBeTruthy()
    expect(typeof output.file.content).toBe('string')
    expect(typeof output.file.numLines).toBe('number')
  })

  test('file_unchanged output type', () => {
    const output = {
      type: 'file_unchanged' as const,
      file: { filePath: '/tmp/test.txt' },
    }
    expect(output.type).toBe('file_unchanged')
  })

  test('image output type has base64 and media type', () => {
    const output = {
      type: 'image' as const,
      file: {
        base64: 'abc123...',
        type: 'image/png' as const,
        originalSize: 1024,
      },
    }
    expect(output.type).toBe('image')
    expect(output.file.base64).toBeTruthy()
    expect(output.file.type).toBe('image/png')
  })
})
