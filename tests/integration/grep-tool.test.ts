/**
 * GrepTool Integration Tests
 *
 * Tests content search via ripgrep (rg), which is the backend that
 * GrepTool.call() delegates to via src/utils/ripgrep.ts.
 *
 * Direct import of GrepTool is not possible without a full `bun install`
 * (transitive deps: react, etc.). Instead, we test ripgrep behavior by
 * spawning `rg` directly, mirroring the args that GrepTool would build.
 *
 * PREREQUISITE: ripgrep (rg) must be available on PATH.
 * Tests that require rg are skipped if it is not found — reported as SKIP,
 * never as a silent pass. See the note on `rgAvailable` below: the detection
 * has to happen at collection time for `test.skipIf` to see it.
 */

import { describe, test, expect, beforeAll, afterAll } from 'bun:test'
import { mkdirSync, writeFileSync, rmSync, readFileSync } from 'fs'
import { join } from 'path'
import { createTestDir } from '../setup.js'

let tmpDir: string

/**
 * rg availability must be resolved at **collection** time, not in `beforeAll`.
 * `test.skipIf(cond)` evaluates `cond` when the test is registered, which is
 * before any hook has run — a `beforeAll`-assigned flag would always read its
 * initial value. Bun supports top-level await, so detect here.
 *
 * This used to live in `beforeAll`, and every rg-dependent case guarded itself
 * with a bare early return — which produced a **zero-assertion pass** rather
 * than a skip, so a machine with no rg reported 13 green tests that had not
 * tested anything. Report absence honestly instead.
 *
 * (Deliberately described, not quoted: embedding the offending snippet verbatim
 * would make this comment a false positive for any grep-based guard.)
 */
const rgAvailable: boolean = await (async () => {
  try {
    const proc = Bun.spawn(['rg', '--version'], { stdout: 'pipe', stderr: 'pipe' })
    return (await proc.exited) === 0
  } catch {
    return false
  }
})()

if (!rgAvailable) {
  console.warn(
    'ripgrep (rg) not found on PATH — rg-dependent GrepTool cases will report as SKIP (not pass)',
  )
}

beforeAll(async () => {
  tmpDir = createTestDir('grep-tool')

  // Create test files with known content for searching
  mkdirSync(join(tmpDir, 'src'), { recursive: true })
  mkdirSync(join(tmpDir, 'docs'), { recursive: true })

  writeFileSync(
    join(tmpDir, 'src', 'main.ts'),
    `import { helper } from './helper'

export function main() {
  console.log('Hello World')
  const result = helper()
  return result
}

export function secondary() {
  return 'secondary function'
}
`,
  )

  writeFileSync(
    join(tmpDir, 'src', 'helper.ts'),
    `export function helper() {
  return 'helper result'
}

export function helperTwo() {
  console.log('helper two called')
  return 42
}
`,
  )

  writeFileSync(
    join(tmpDir, 'src', 'utils.js'),
    `function formatDate(date) {
  return date.toISOString()
}

function parseJSON(str) {
  return JSON.parse(str)
}

module.exports = { formatDate, parseJSON }
`,
  )

  writeFileSync(
    join(tmpDir, 'docs', 'notes.md'),
    `# Project Notes

This is a HELLO WORLD example project.
It demonstrates various patterns.

## Functions
- main: entry point
- helper: utility function
- formatDate: date formatting
`,
  )
})

afterAll(() => {
  try {
    rmSync(tmpDir, { recursive: true, force: true })
  } catch {
    // best-effort cleanup
  }
})

/**
 * Helper: run ripgrep and return results as string array.
 * Mirrors the core of what GrepTool builds and executes.
 */
async function runRg(args: string[], cwd: string): Promise<{ lines: string[]; exitCode: number }> {
  const proc = Bun.spawn(['rg', ...args, cwd], {
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const stdout = await new Response(proc.stdout).text()
  const exitCode = await proc.exited
  const lines = stdout.trim().split('\n').filter(Boolean)
  return { lines, exitCode }
}

describe('GrepTool (ripgrep-based search)', () => {
  describe('ripgrep availability', () => {
    // Was an unconditionally-true assertion plus a console warning — a tautology
    // that passed whether or not rg existed. Assert the real precondition instead.
    test.skipIf(!rgAvailable)('ripgrep (rg) on PATH reports a usable version', async () => {
      const proc = Bun.spawn(['rg', '--version'], { stdout: 'pipe', stderr: 'pipe' })
      const stdout = await new Response(proc.stdout).text()
      expect(await proc.exited).toBe(0)
      expect(stdout).toMatch(/^ripgrep \d+\.\d+/)
    })
  })

  describe('basic regex matching', () => {
    test.skipIf(!rgAvailable)('finds files containing a pattern (-l mode)', async () => {
      const { lines } = await runRg(['--hidden', '-l', 'function'], tmpDir)
      // All source files contain "function"
      expect(lines.length).toBeGreaterThanOrEqual(3)
    })

    test.skipIf(!rgAvailable)('finds matching content lines', async () => {
      const { lines } = await runRg(['--hidden', 'Hello World'], tmpDir)
      const content = lines.join('\n')
      expect(content).toContain('Hello World')
    })

    test.skipIf(!rgAvailable)('regex patterns work (\\w+)', async () => {
      const { lines } = await runRg(
        ['--hidden', '-n', 'export function \\w+'],
        join(tmpDir, 'src'),
      )
      expect(lines.length).toBeGreaterThanOrEqual(3) // main, secondary, helper, helperTwo
      const content = lines.join('\n')
      expect(content).toContain('export function')
    })
  })

  describe('case insensitive search', () => {
    test.skipIf(!rgAvailable)('finds matches regardless of case with -i flag', async () => {
      const { lines } = await runRg(['--hidden', '-l', '-i', 'hello world'], tmpDir)
      // Should match "Hello World" in main.ts and "HELLO WORLD" in notes.md
      expect(lines.length).toBeGreaterThanOrEqual(2)
    })

    test.skipIf(!rgAvailable)('case-sensitive search does not match different case', async () => {
      const { lines } = await runRg(['--hidden', '-l', 'hello world'], tmpDir)
      // "hello world" (all lowercase) does not appear in any file
      expect(lines.length).toBe(0)
    })
  })

  describe('file type filter', () => {
    test.skipIf(!rgAvailable)('--type ts filters to TypeScript files only', async () => {
      const { lines } = await runRg(
        ['--hidden', '-l', '--type', 'ts', 'function'],
        tmpDir,
      )
      // Only .ts files should match
      for (const f of lines) {
        expect(f).toMatch(/\.ts$/)
      }
      expect(lines.length).toBeGreaterThanOrEqual(2) // main.ts, helper.ts
    })

    test.skipIf(!rgAvailable)('--type js filters to JavaScript files only', async () => {
      const { lines } = await runRg(
        ['--hidden', '-l', '--type', 'js', 'function'],
        tmpDir,
      )
      for (const f of lines) {
        expect(f).toMatch(/\.js$/)
      }
      expect(lines.length).toBeGreaterThanOrEqual(1) // utils.js
    })
  })

  describe('empty result handling', () => {
    test.skipIf(!rgAvailable)('returns no matches for non-matching pattern', async () => {
      const { lines, exitCode } = await runRg(
        ['--hidden', '-l', 'zzz_no_match_xyz'],
        tmpDir,
      )
      expect(lines.length).toBe(0)
      // rg exit code 1 = no matches found
      expect(exitCode).toBe(1)
    })
  })

  describe('count mode', () => {
    test.skipIf(!rgAvailable)('-c returns match counts per file', async () => {
      const { lines } = await runRg(
        ['--hidden', '-c', 'function'],
        join(tmpDir, 'src'),
      )
      // Each line is "filepath:count"
      let totalMatches = 0
      for (const line of lines) {
        const parts = line.split(':')
        const count = parseInt(parts[parts.length - 1]!, 10)
        if (!isNaN(count)) totalMatches += count
      }
      expect(totalMatches).toBeGreaterThanOrEqual(5)
    })
  })

  describe('glob filter', () => {
    test.skipIf(!rgAvailable)('--glob restricts to matching files', async () => {
      const { lines } = await runRg(
        ['--hidden', '-l', '--glob', '*.ts', 'function'],
        tmpDir,
      )
      // Only .ts files
      for (const f of lines) {
        expect(f).toMatch(/\.ts$/)
      }
    })
  })

  describe('context flags', () => {
    test.skipIf(!rgAvailable)('-B shows lines before match', async () => {
      const { lines } = await runRg(
        ['--hidden', '-n', '-B', '1', 'Hello World'],
        join(tmpDir, 'src'),
      )
      // Should include at least one context line before the match
      expect(lines.length).toBeGreaterThanOrEqual(2)
    })

    test.skipIf(!rgAvailable)('-A shows lines after match', async () => {
      const { lines } = await runRg(
        ['--hidden', '-n', '-A', '1', 'Hello World'],
        join(tmpDir, 'src'),
      )
      expect(lines.length).toBeGreaterThanOrEqual(2)
    })
  })
})

/**
 * 这些用例此前是拿自建字面量对自己做 `typeof` 断言 —— 无论产品的 outputSchema
 * 怎么改都恒绿，等于没测。现在改成对着产品 `outputSchema` 的**真实字段声明**校验。
 *
 * 之所以读源码文本而不是 import 真 schema：`src/tools/GrepTool/GrepTool.ts`
 * 经 `./UI.js` → … → `src/tools/GlobTool/UI.tsx` 回指自身构成 require 环，
 * 在测试里 import 会抛 `Cannot access 'GrepTool' before initialization`。
 */
describe('GrepTool output contract', () => {
  const productSource = readFileSync(
    join(import.meta.dir, '..', '..', 'src', 'tools', 'GrepTool', 'GrepTool.ts'),
    'utf8',
  )

  const schemaBody = /const outputSchema = lazySchema\(\(\) =>\s*z\.object\(\{([\s\S]*?)^\s*\}\),/m.exec(
    productSource,
  )?.[1]

  const fields = new Map<string, { optional: boolean }>()
  for (const line of (schemaBody ?? '').split('\n')) {
    const m = /^\s*(\w+):\s*(.+?),\s*(?:\/\/.*)?$/.exec(line)
    if (m) fields.set(m[1]!, { optional: m[2]!.includes('.optional()') })
  }
  const required = [...fields].filter(([, v]) => !v.optional).map(([k]) => k)

  test('产品 outputSchema 解析得到（闸门自身不得空转）', () => {
    expect(schemaBody).toBeDefined()
    expect(fields.size).toBeGreaterThan(0)
  })

  test('字段集与产品声明一致（产品增删字段即红）', () => {
    expect([...fields.keys()].sort()).toEqual([
      'appliedLimit',
      'appliedOffset',
      'content',
      'filenames',
      'mode',
      'numFiles',
      'numLines',
      'numMatches',
    ])
    expect(required.sort()).toEqual(['filenames', 'numFiles'])
  })

  for (const { name, output } of [
    {
      name: 'files_with_matches',
      output: {
        mode: 'files_with_matches',
        numFiles: 3,
        filenames: ['src/main.ts', 'src/helper.ts', 'src/utils.js'],
      },
    },
    {
      name: 'content',
      output: {
        mode: 'content',
        numFiles: 0,
        filenames: [],
        content: 'src/main.ts:4:  console.log("Hello World")',
        numLines: 1,
      },
    },
    {
      name: 'count',
      output: {
        mode: 'count',
        numFiles: 3,
        filenames: [],
        content: 'src/main.ts:2\nsrc/helper.ts:2\nsrc/utils.js:2',
        numMatches: 6,
      },
    },
  ] as { name: string; output: Record<string, unknown> }[]) {
    test(`${name} mode 样本只用产品声明过的字段，且必填字段齐全`, () => {
      const undeclared = Object.keys(output).filter(k => !fields.has(k))
      expect(undeclared).toEqual([])
      const missing = required.filter(k => !(k in output))
      expect(missing).toEqual([])
    })
  }
})
