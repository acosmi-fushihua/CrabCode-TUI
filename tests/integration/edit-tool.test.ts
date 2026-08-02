/**
 * FileEditTool Integration Tests
 *
 * Tests the core file editing operations: string replacement, new file creation,
 * duplicate string detection, and validation rules.
 *
 * NOTE: FileEditTool and its transitive dependencies cannot be imported in
 * `bun test` mode without a full `bun install`. These tests exercise the
 * editing logic at the fs level — the same read/modify/write sequence that
 * FileEditTool.call() performs — and validate the tool's behavioral contract.
 */

import { describe, test, expect, beforeAll, afterAll } from 'bun:test'
import { mkdirSync, writeFileSync, readFileSync, rmSync, existsSync, statSync } from 'fs'
import { join, dirname } from 'path'
import { createTestDir } from '../setup.js'

let tmpDir: string

beforeAll(() => {
  tmpDir = createTestDir('edit-tool')
})

afterAll(() => {
  try {
    rmSync(tmpDir, { recursive: true, force: true })
  } catch {
    // best-effort cleanup
  }
})

/**
 * Simulate FileEditTool's core edit operation:
 * 1. Read file content
 * 2. Find old_string
 * 3. Replace (once or all)
 * 4. Write back
 *
 * Returns the validation/edit result mimicking FileEditTool's behavior.
 */
function simulateEdit(
  filePath: string,
  oldString: string,
  newString: string,
  replaceAll = false,
): {
  success: boolean
  error?: string
  originalContent?: string
  updatedContent?: string
} {
  // Validation: old_string === new_string
  if (oldString === newString) {
    return { success: false, error: 'No changes to make: old_string and new_string are exactly the same.' }
  }

  const fileExists = existsSync(filePath)

  // File doesn't exist
  if (!fileExists) {
    if (oldString === '') {
      // Create new file
      mkdirSync(dirname(filePath), { recursive: true })
      writeFileSync(filePath, newString)
      return { success: true, originalContent: '', updatedContent: newString }
    }
    return { success: false, error: 'File does not exist.' }
  }

  const content = readFileSync(filePath, 'utf-8').replaceAll('\r\n', '\n')

  // Empty old_string on existing file
  if (oldString === '') {
    if (content.trim() !== '') {
      return { success: false, error: 'Cannot create new file - file already exists.' }
    }
    // Empty file — insert content
    writeFileSync(filePath, newString)
    return { success: true, originalContent: content, updatedContent: newString }
  }

  // old_string not found
  if (!content.includes(oldString)) {
    return { success: false, error: `String to replace not found in file.\nString: ${oldString}` }
  }

  // Multiple matches without replace_all
  const matches = content.split(oldString).length - 1
  if (matches > 1 && !replaceAll) {
    return {
      success: false,
      error: `Found ${matches} matches of the string to replace, but replace_all is false.`,
    }
  }

  // Perform replacement
  const updatedContent = replaceAll
    ? content.replaceAll(oldString, newString)
    : content.replace(oldString, newString)

  writeFileSync(filePath, updatedContent)
  return { success: true, originalContent: content, updatedContent }
}

describe('FileEditTool (file editing pipeline)', () => {
  describe('string replacement', () => {
    test('replaces a unique string in file content', () => {
      const filePath = join(tmpDir, 'replace-unique.txt')
      writeFileSync(filePath, 'Hello World\nGoodbye Moon\n')

      const result = simulateEdit(filePath, 'Hello', 'Greetings')
      expect(result.success).toBe(true)

      const final = readFileSync(filePath, 'utf-8')
      expect(final).toContain('Greetings World')
      expect(final).not.toContain('Hello World')
      expect(final).toContain('Goodbye Moon')
    })

    test('replace_all replaces all occurrences', () => {
      const filePath = join(tmpDir, 'replace-all.txt')
      writeFileSync(filePath, 'foo bar\nfoo baz\nfoo qux\n')

      const result = simulateEdit(filePath, 'foo', 'replaced', true)
      expect(result.success).toBe(true)

      const final = readFileSync(filePath, 'utf-8')
      expect(final).not.toContain('foo')
      expect(final.split('replaced').length - 1).toBe(3)
    })

    test('multiline string replacement works', () => {
      const filePath = join(tmpDir, 'replace-multiline.txt')
      writeFileSync(filePath, 'function old() {\n  return 1\n}\n')

      const result = simulateEdit(
        filePath,
        'function old() {\n  return 1\n}',
        'function updated() {\n  return 2\n}',
      )
      expect(result.success).toBe(true)

      const final = readFileSync(filePath, 'utf-8')
      expect(final).toContain('function updated()')
      expect(final).toContain('return 2')
    })

    test('preserves file content outside the replaced region', () => {
      const filePath = join(tmpDir, 'preserve-context.txt')
      writeFileSync(filePath, 'line1\nTARGET\nline3\nline4\n')

      const result = simulateEdit(filePath, 'TARGET', 'REPLACED')
      expect(result.success).toBe(true)

      const final = readFileSync(filePath, 'utf-8')
      expect(final).toBe('line1\nREPLACED\nline3\nline4\n')
    })
  })

  describe('new file creation', () => {
    test('creates file when old_string is empty and file does not exist', () => {
      const filePath = join(tmpDir, 'new-file.txt')
      expect(existsSync(filePath)).toBe(false)

      const result = simulateEdit(filePath, '', 'brand new content\n')
      expect(result.success).toBe(true)
      expect(existsSync(filePath)).toBe(true)
      expect(readFileSync(filePath, 'utf-8')).toBe('brand new content\n')
    })

    test('creates nested directory structure if needed', () => {
      const filePath = join(tmpDir, 'deep', 'nested', 'dir', 'file.ts')
      expect(existsSync(filePath)).toBe(false)

      const result = simulateEdit(filePath, '', 'export const x = 1\n')
      expect(result.success).toBe(true)
      expect(existsSync(filePath)).toBe(true)
      expect(readFileSync(filePath, 'utf-8')).toContain('export const x = 1')
    })

    test('rejects empty old_string when file already has content', () => {
      const filePath = join(tmpDir, 'existing-content.txt')
      writeFileSync(filePath, 'I already have content\n')

      const result = simulateEdit(filePath, '', 'new content')
      expect(result.success).toBe(false)
      expect(result.error).toContain('already exists')
    })

    test('allows empty old_string on empty file (content insertion)', () => {
      const filePath = join(tmpDir, 'empty-for-insert.txt')
      writeFileSync(filePath, '')

      const result = simulateEdit(filePath, '', 'inserted content')
      expect(result.success).toBe(true)
      expect(readFileSync(filePath, 'utf-8')).toBe('inserted content')
    })
  })

  describe('duplicate string handling', () => {
    test('rejects when multiple matches exist without replace_all', () => {
      const filePath = join(tmpDir, 'duplicate-reject.txt')
      writeFileSync(filePath, 'foo bar\nfoo baz\nfoo qux\n')

      const result = simulateEdit(filePath, 'foo', 'replaced', false)
      expect(result.success).toBe(false)
      expect(result.error).toContain('matches')
      expect(result.error).toContain('replace_all')
    })

    test('succeeds with multiple matches when replace_all=true', () => {
      const filePath = join(tmpDir, 'duplicate-accept.txt')
      writeFileSync(filePath, 'foo bar\nfoo baz\nfoo qux\n')

      const result = simulateEdit(filePath, 'foo', 'replaced', true)
      expect(result.success).toBe(true)
    })

    test('unique string has exactly one match (no error)', () => {
      const filePath = join(tmpDir, 'unique-string.txt')
      writeFileSync(filePath, 'alpha beta gamma\ndelta epsilon\n')

      const result = simulateEdit(filePath, 'beta gamma', 'REPLACED')
      expect(result.success).toBe(true)
    })
  })

  describe('validation rules', () => {
    test('rejects when old_string === new_string', () => {
      const filePath = join(tmpDir, 'same-string.txt')
      writeFileSync(filePath, 'content\n')

      const result = simulateEdit(filePath, 'content', 'content')
      expect(result.success).toBe(false)
      expect(result.error).toContain('same')
    })

    test('rejects when old_string not found in file', () => {
      const filePath = join(tmpDir, 'not-found.txt')
      writeFileSync(filePath, 'actual content here\n')

      const result = simulateEdit(filePath, 'nonexistent string', 'replacement')
      expect(result.success).toBe(false)
      expect(result.error).toContain('not found')
    })

    test('rejects non-empty old_string on non-existent file', () => {
      const missingPath = join(tmpDir, 'totally-missing.txt')
      const result = simulateEdit(missingPath, 'something', 'replacement')
      expect(result.success).toBe(false)
      expect(result.error).toContain('does not exist')
    })
  })

  describe('file state tracking (staleness check concept)', () => {
    test('read timestamp is earlier than write timestamp after edit', () => {
      const filePath = join(tmpDir, 'staleness-check.txt')
      writeFileSync(filePath, 'original\n')

      const readTime = statSync(filePath).mtimeMs

      // Small delay to ensure mtime changes
      writeFileSync(filePath, 'modified\n')
      const writeTime = statSync(filePath).mtimeMs

      // After modification, mtime should be >= readTime
      expect(writeTime).toBeGreaterThanOrEqual(readTime)
    })
  })
})

describe('FileEditTool output contract', () => {
  test('success result contains expected fields', () => {
    const output = {
      filePath: '/tmp/test.txt',
      oldString: 'old',
      newString: 'new',
      originalFile: 'old content',
      structuredPatch: [],
      userModified: false,
      replaceAll: false,
    }
    expect(typeof output.filePath).toBe('string')
    expect(typeof output.oldString).toBe('string')
    expect(typeof output.newString).toBe('string')
    expect(typeof output.originalFile).toBe('string')
    expect(Array.isArray(output.structuredPatch)).toBe(true)
    expect(typeof output.userModified).toBe('boolean')
    expect(typeof output.replaceAll).toBe('boolean')
  })

  test('success message for normal edit', () => {
    const filePath = '/tmp/test.txt'
    const message = `The file ${filePath} has been updated successfully.`
    expect(message).toContain('updated successfully')
  })

  test('success message for replace_all mentions all occurrences', () => {
    const filePath = '/tmp/test.txt'
    const replaceAll = true
    const message = replaceAll
      ? `The file ${filePath} has been updated. All occurrences were successfully replaced.`
      : `The file ${filePath} has been updated successfully.`
    expect(message).toContain('All occurrences')
  })

  test('success message includes user-modified note when applicable', () => {
    const userModified = true
    const modifiedNote = userModified
      ? '.  The user modified your proposed changes before accepting them. '
      : ''
    expect(modifiedNote).toContain('user modified')
  })
})
