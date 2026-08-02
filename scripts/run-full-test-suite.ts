/** Run each TypeScript test file in an isolated Bun process. */

import { existsSync, statSync } from 'node:fs'
import { relative, resolve, sep } from 'node:path'

const REPO_ROOT = resolve(import.meta.dir, '..')
const TEST_FILE = /\.test\.tsx?$/
const LOCALHOST_HOSTS = ['127.0.0.1', 'localhost', '::1'] as const

function portable(path: string): string {
  return relative(REPO_ROOT, path).split(sep).join('/')
}

function mergeNoProxy(existing: string | undefined): string {
  const values = (existing ?? '')
    .split(',')
    .map(value => value.trim())
    .filter(Boolean)
  for (const host of LOCALHOST_HOSTS) {
    if (!values.some(value => value.toLowerCase() === host.toLowerCase())) {
      values.push(host)
    }
  }
  return values.join(',')
}

async function collect(inputs: string[]): Promise<string[]> {
  const files = new Set<string>()
  for (const input of inputs) {
    const absolute = resolve(input)
    if (!existsSync(absolute)) throw new Error(`test input is missing: ${input}`)
    if (!statSync(absolute).isDirectory()) {
      if (!TEST_FILE.test(absolute)) {
        throw new Error(`test input is not a test file: ${input}`)
      }
      files.add(absolute)
      continue
    }
    const glob = new Bun.Glob('**/*.test.{ts,tsx}')
    for await (const file of glob.scan({ cwd: absolute, absolute: true })) {
      files.add(file)
    }
  }
  return [...files].sort((left, right) => left.localeCompare(right))
}

async function runFile(file: string, env: Record<string, string>): Promise<boolean> {
  const child = Bun.spawn(['bun', 'test', file], {
    cwd: REPO_ROOT,
    env,
    stdin: 'ignore',
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (exitCode === 0) return true
  process.stderr.write(`\n[test] FAIL ${portable(file)}\n${stdout}${stderr}`)
  return false
}

const files = await collect(process.argv.slice(2).length > 0 ? process.argv.slice(2) : ['tests'])
if (files.length === 0) throw new Error('no test files found')

const env = { ...(process.env as Record<string, string>) }
const noProxy = mergeNoProxy(env.NO_PROXY ?? env.no_proxy)
env.NO_PROXY = noProxy
env.no_proxy = noProxy

const failed: string[] = []
for (const [index, file] of files.entries()) {
  if (!(await runFile(file, env))) failed.push(portable(file))
  if ((index + 1) % 20 === 0 || index + 1 === files.length) {
    process.stdout.write(
      `[test] completed=${index + 1}/${files.length} failed=${failed.length}\n`,
    )
  }
}

if (failed.length > 0) {
  process.stderr.write(`[test] failed files:\n${failed.map(file => `  - ${file}`).join('\n')}\n`)
  process.exit(1)
}

process.stdout.write(`[test] PASS files=${files.length}\n`)
