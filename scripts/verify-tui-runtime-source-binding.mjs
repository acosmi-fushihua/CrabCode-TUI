#!/usr/bin/env bun

import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import {
  verifyTuiRuntimeArtifactBinding,
  verifyTuiRuntimeBuildBinding,
  verifyTuiRuntimeSourceBinding,
} from './tui-runtime-source-binding.mjs'

const root = resolve(process.env.CRABCODE_TUI_BINDING_ROOT ?? join(import.meta.dir, '..'))
const metafilePath = join(root, 'dist/tui-runtime/metafile.json')
const artifactPath = join(root, 'dist/tui-runtime/index.js')
const packagePath = join(root, 'package.json')
if (
  !existsSync(metafilePath) ||
  !existsSync(artifactPath) ||
  !existsSync(packagePath)
) {
  throw new Error('TUI runtime binding check requires a fresh bun run build:ts')
}

const metafile = JSON.parse(readFileSync(metafilePath, 'utf8'))
const packageManifest = JSON.parse(readFileSync(packagePath, 'utf8'))
const version = packageManifest.version
let expectedBuildId = process.env.CRABCODE_BUILD_ID
if (!expectedBuildId || expectedBuildId.trim().length === 0) {
  try {
    const revision = execFileSync(
      'git',
      ['rev-parse', '--short=12', 'HEAD'],
      { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] },
    ).trim()
    expectedBuildId = /^[a-f0-9]{12,40}$/u.test(revision)
      ? `${version}+${revision}`
      : `${version}+unknown`
  } catch {
    expectedBuildId = `${version}+unknown`
  }
}
verifyTuiRuntimeBuildBinding(metafile, { version, buildId: expectedBuildId })
const source = verifyTuiRuntimeSourceBinding(root, metafile)
const artifact = verifyTuiRuntimeArtifactBinding(artifactPath, metafile)
console.log(
  `Verified TUI runtime source/artifact/build binding (${source.inputCount} inputs, ${artifact.size} bytes, build-id ${expectedBuildId})`,
)
