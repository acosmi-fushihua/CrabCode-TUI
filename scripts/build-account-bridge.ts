#!/usr/bin/env bun

import { mkdirSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dir, '..')
const component = join(root, 'components', 'oauthapi-llm')
const outputDirectory = join(root, 'dist', 'account-bridge')
const output = join(
  outputDirectory,
  process.platform === 'win32' ? 'oauthapi-llm.exe' : 'oauthapi-llm',
)

mkdirSync(outputDirectory, { recursive: true })
const child = Bun.spawn(
  ['go', 'build', '-trimpath', '-o', output, './cmd/oauthapi-llm'],
  {
    cwd: component,
    stdin: 'inherit',
    stdout: 'inherit',
    stderr: 'inherit',
  },
)
const exitCode = await child.exited
if (exitCode !== 0) process.exit(exitCode)
process.stdout.write(`Built ${output}\n`)
