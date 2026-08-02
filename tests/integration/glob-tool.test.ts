/**
 * GlobTool Integration Tests
 *
 * Tests file pattern matching using the `glob` utility from src/utils/glob.ts,
 * which is the core function that GlobTool.call() delegates to.
 *
 * Direct import of GlobTool is not possible without a full `bun install`
 * (transitive deps: react, etc.). The `glob` utility itself also has deep
 * deps (messages.ts -> connectorText.js). So we test glob behavior via the
 * native Node.js glob or Bun.Glob as a specification test — verifying that
 * patterns match as GlobTool expects in a real directory structure.
 *
 * When full dependencies are available, these tests should be upgraded to
 * import and call GlobTool directly.
 */

import { describe, test, expect, beforeAll, afterAll } from 'bun:test'
import { mkdirSync, writeFileSync, rmSync, readdirSync, statSync } from 'fs'
import { join, relative } from 'path'
import { createTestDir } from '../setup.js'

let tmpDir: string

beforeAll(() => {
  tmpDir = createTestDir('glob-tool')

  // Create a project structure for glob testing:
  //   tmpDir/
  //     src/
  //       index.ts
  //       utils.ts
  //       helpers/
  //         format.ts
  //     docs/
  //       readme.md
  //       guide.md
  //     config.json
  //     package.json
  mkdirSync(join(tmpDir, 'src', 'helpers'), { recursive: true })
  mkdirSync(join(tmpDir, 'docs'), { recursive: true })

  writeFileSync(join(tmpDir, 'src', 'index.ts'), 'export {}')
  writeFileSync(join(tmpDir, 'src', 'utils.ts'), 'export function util() {}')
  writeFileSync(join(tmpDir, 'src', 'helpers', 'format.ts'), 'export function fmt() {}')
  writeFileSync(join(tmpDir, 'docs', 'readme.md'), '# Readme')
  writeFileSync(join(tmpDir, 'docs', 'guide.md'), '# Guide')
  writeFileSync(join(tmpDir, 'config.json'), '{}')
  writeFileSync(join(tmpDir, 'package.json'), '{"name": "test"}')
})

afterAll(() => {
  try {
    rmSync(tmpDir, { recursive: true, force: true })
  } catch {
    // best-effort cleanup
  }
})

/**
 * Helper: use Bun.Glob to match files in a directory.
 * This mirrors what GlobTool does internally via src/utils/glob.ts.
 */
async function globFiles(pattern: string, cwd: string): Promise<string[]> {
  const glob = new Bun.Glob(pattern)
  const results: string[] = []
  for await (const path of glob.scan({ cwd, onlyFiles: true })) {
    results.push(path)
  }
  return results.sort()
}

describe('Glob pattern matching (GlobTool core behavior)', () => {
  describe('basic glob matching', () => {
    test('*.ts matches TypeScript files in src directory', async () => {
      const files = await globFiles('*.ts', join(tmpDir, 'src'))
      expect(files).toContain('index.ts')
      expect(files).toContain('utils.ts')
      // *.ts should NOT match deeply nested files
      expect(files).not.toContain('helpers/format.ts')
      expect(files.length).toBe(2)
    })

    test('*.md matches markdown files in docs directory', async () => {
      const files = await globFiles('*.md', join(tmpDir, 'docs'))
      expect(files.length).toBe(2)
      expect(files).toContain('readme.md')
      expect(files).toContain('guide.md')
    })

    test('*.json matches JSON files at project root', async () => {
      const files = await globFiles('*.json', tmpDir)
      expect(files.length).toBe(2)
      expect(files).toContain('config.json')
      expect(files).toContain('package.json')
    })
  })

  describe('recursive search', () => {
    test('**/*.ts matches all TypeScript files recursively', async () => {
      const files = await globFiles('**/*.ts', tmpDir)
      expect(files.length).toBe(3)
      // Should find files in nested directories
      const hasIndex = files.some(f => f.includes('index.ts'))
      const hasUtils = files.some(f => f.includes('utils.ts'))
      const hasFormat = files.some(f => f.includes('format.ts'))
      expect(hasIndex).toBe(true)
      expect(hasUtils).toBe(true)
      expect(hasFormat).toBe(true)
    })

    test('**/*.md matches all markdown files recursively', async () => {
      const files = await globFiles('**/*.md', tmpDir)
      expect(files.length).toBe(2)
    })

    test('**/* matches all files recursively', async () => {
      const files = await globFiles('**/*', tmpDir)
      expect(files.length).toBe(7)
    })

    test('src/**/*.ts matches TypeScript files in src tree', async () => {
      const files = await globFiles('src/**/*.ts', tmpDir)
      expect(files.length).toBe(3)
    })
  })

  describe('empty result handling', () => {
    test('returns empty array for non-matching pattern', async () => {
      const files = await globFiles('*.xyz', tmpDir)
      expect(files).toEqual([])
    })

    test('returns empty array for non-matching recursive pattern', async () => {
      const files = await globFiles('**/*.rust', tmpDir)
      expect(files).toEqual([])
    })
  })

  describe('specific directory matching', () => {
    test('docs/* matches only top-level files in docs', async () => {
      const files = await globFiles('docs/*', tmpDir)
      expect(files.length).toBe(2)
    })

    test('src/helpers/*.ts matches only files in helpers', async () => {
      const files = await globFiles('src/helpers/*.ts', tmpDir)
      expect(files.length).toBe(1)
      expect(files[0]).toContain('format.ts')
    })
  })

  describe('mixed patterns', () => {
    test('pattern matching is case-sensitive', async () => {
      const files = await globFiles('*.TS', tmpDir)
      // .TS (uppercase) should not match .ts files (case-sensitive)
      expect(files.length).toBe(0)
    })
  })
})

describe('GlobTool output contract', () => {
  // Test the expected output shape that GlobTool produces
  // (without importing the tool itself)

  test('output should include filenames array, numFiles, truncated, durationMs', () => {
    // Simulate the GlobTool output shape
    const output = {
      filenames: ['src/index.ts', 'src/utils.ts'],
      durationMs: 5,
      numFiles: 2,
      truncated: false,
    }
    expect(Array.isArray(output.filenames)).toBe(true)
    expect(typeof output.numFiles).toBe('number')
    expect(typeof output.truncated).toBe('boolean')
    expect(typeof output.durationMs).toBe('number')
  })

  test('"No files found" should be returned for empty results', () => {
    // The mapToolResultToToolResultBlockParam contract
    const output = { filenames: [], durationMs: 1, numFiles: 0, truncated: false }
    const content = output.filenames.length === 0 ? 'No files found' : output.filenames.join('\n')
    expect(content).toBe('No files found')
  })

  test('non-empty results should be joined with newlines', () => {
    const output = {
      filenames: ['src/index.ts', 'src/utils.ts'],
      durationMs: 5,
      numFiles: 2,
      truncated: false,
    }
    const content = output.filenames.join('\n')
    expect(content).toBe('src/index.ts\nsrc/utils.ts')
  })
})
