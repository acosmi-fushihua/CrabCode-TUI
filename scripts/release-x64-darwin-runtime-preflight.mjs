#!/usr/bin/env bun

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { cpus, tmpdir } from 'node:os'
import { isAbsolute, join, resolve } from 'node:path'
import { unzipSync } from 'fflate'
import { x64DarwinBunRelease } from './release-bun-pins.mjs'
import { assertTuiRuntimeSmokeSuccess } from './tui-runtime-smoke-contract.mjs'

const repositoryRoot = resolve(import.meta.dir, '..')
const smokeScript = join(repositoryRoot, 'scripts', 'tui-runtime-smoke.mjs')
const maximumIterations = 100

function fail(message) {
  throw new Error(`x64 Darwin runtime preflight failed: ${message}`)
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function parseArguments(argv) {
  let iterations = 10
  for (let index = 0; index < argv.length; index += 1) {
    const name = argv[index]
    if (name === '--help' || name === '-h') return { help: true, iterations }
    if (name !== '--iterations' || index + 1 >= argv.length) {
      fail(`unsupported argument near ${name}`)
    }
    const value = Number(argv[index + 1])
    if (!Number.isSafeInteger(value) || value < 1 || value > maximumIterations) {
      fail(`--iterations must be an integer from 1 through ${maximumIterations}`)
    }
    iterations = value
    index += 1
  }
  return { help: false, iterations }
}

async function downloadArchive() {
  const override = process.env.CRABCODE_PREFLIGHT_BUN_ARCHIVE
  if (override) {
    if (!isAbsolute(override)) fail('CRABCODE_PREFLIGHT_BUN_ARCHIVE must be absolute')
    const info = lstatSync(override)
    if (!info.isFile() || info.isSymbolicLink()) {
      fail('CRABCODE_PREFLIGHT_BUN_ARCHIVE must be a regular file')
    }
    return readFileSync(override)
  }

  const response = await fetch(x64DarwinBunRelease.url, {
    redirect: 'follow',
    signal: AbortSignal.timeout(120_000),
  })
  if (!response.ok || !response.body) {
    fail(`download returned HTTP ${response.status}`)
  }
  const declaredLength = response.headers.get('content-length')
  if (declaredLength && Number(declaredLength) !== x64DarwinBunRelease.bytes) {
    fail(`download declared ${declaredLength} bytes instead of ${x64DarwinBunRelease.bytes}`)
  }

  const chunks = []
  let received = 0
  for await (const chunk of response.body) {
    received += chunk.byteLength
    if (received > x64DarwinBunRelease.bytes) {
      fail(`download exceeded the reviewed ${x64DarwinBunRelease.bytes}-byte boundary`)
    }
    chunks.push(Buffer.from(chunk))
  }
  return Buffer.concat(chunks)
}

function verifyArchive(bytes) {
  if (bytes.byteLength !== x64DarwinBunRelease.bytes) {
    fail(`archive size ${bytes.byteLength} differs from ${x64DarwinBunRelease.bytes}`)
  }
  const digest = sha256(bytes)
  if (digest !== x64DarwinBunRelease.sha256) {
    fail(`archive SHA-256 ${digest} differs from ${x64DarwinBunRelease.sha256}`)
  }
}

function extractPinnedExecutable(archive, destination) {
  const entries = unzipSync(new Uint8Array(archive))
  const expected = `${x64DarwinBunRelease.root}/bun`
  const names = Object.keys(entries).sort()
  if (
    names.length !== 2 ||
    names[0] !== `${x64DarwinBunRelease.root}/` ||
    names[1] !== expected
  ) {
    fail(`archive members differ from the reviewed Bun package: ${names.join(', ')}`)
  }
  const executable = entries[expected]
  if (
    executable.byteLength !== x64DarwinBunRelease.executableBytes ||
    sha256(executable) !== x64DarwinBunRelease.executableSha256
  ) {
    fail('extracted Bun executable differs from the reviewed binary')
  }
  writeFileSync(destination, executable)
  chmodSync(destination, 0o755)
}

function runChecked(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
    timeout: 75_000,
    ...options,
  })
  if (result.error || result.status !== 0) {
    fail(
      `${command} ${args.join(' ')} failed: ${
        result.error?.message ?? result.stderr?.trim() ?? `status ${result.status}`
      }`,
    )
  }
  return result
}

const args = parseArguments(process.argv.slice(2))
if (args.help) {
  console.log('usage: bun scripts/release-x64-darwin-runtime-preflight.mjs [--iterations 1..100]')
  process.exit(0)
}
if (process.platform !== 'darwin' || process.arch !== 'x64') {
  fail(`must execute as Darwin x64, received ${process.platform}/${process.arch}`)
}
const translationProbe = spawnSync(
  '/usr/sbin/sysctl',
  ['-in', 'sysctl.proc_translated'],
  { encoding: 'utf8', timeout: 5_000 },
)
const cpuModels = cpus()
  .map(cpu => cpu.model.trim())
  .filter(Boolean)
const isRosettaControl =
  (translationProbe.status === 0 && translationProbe.stdout.trim() === '1') ||
  cpuModels.some(model => model.startsWith('Apple '))
const isNativeIntel =
  !isRosettaControl && cpuModels.some(model => model.includes('Intel'))
if (
  isRosettaControl &&
  process.env.CRABCODE_PREFLIGHT_ALLOW_ROSETTA_CONTROL !== '1'
) {
  fail(
    'Rosetta is a supplemental control, not native Intel release authority; run on native x64 macOS',
  )
}
if (!isRosettaControl && !isNativeIntel) {
  fail('cannot prove that the x64 process is executing on native Intel hardware')
}

const sourceRuntime = join(repositoryRoot, 'dist', 'tui-runtime', 'index.js')
const sourceMetafile = join(repositoryRoot, 'dist', 'tui-runtime', 'metafile.json')
const packageManifestPath = join(repositoryRoot, 'package.json')
for (const source of [sourceRuntime, sourceMetafile, smokeScript]) {
  const info = lstatSync(source)
  if (!info.isFile() || info.isSymbolicLink() || info.size === 0) {
    fail(`required preflight input is not a non-empty regular file: ${source}`)
  }
}

const packageManifest = JSON.parse(readFileSync(packageManifestPath, 'utf8'))
const releaseVersion = packageManifest.version
const releaseBuildId = process.env.CRABCODE_BUILD_ID?.trim()
const runtimeMetafile = JSON.parse(readFileSync(sourceMetafile, 'utf8'))
if (
  typeof releaseVersion !== 'string' ||
  !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u.test(releaseVersion) ||
  !releaseBuildId
) {
  fail(
    'release version and CRABCODE_BUILD_ID must be established before preflight',
  )
}
if (
  runtimeMetafile.crabcodeTuiBuild?.version !== releaseVersion ||
  runtimeMetafile.crabcodeTuiBuild?.buildId !== releaseBuildId
) {
  fail('built runtime identity differs from the release version or build ID')
}

const scratch = mkdtempSync(join(tmpdir(), 'crabcode-x64-darwin-preflight-'))
try {
  const archive = await downloadArchive()
  verifyArchive(archive)

  const runtimeDirectory = join(scratch, 'dist', 'tui-runtime')
  mkdirSync(runtimeDirectory, { recursive: true })
  copyFileSync(sourceRuntime, join(runtimeDirectory, 'index.js'))
  copyFileSync(sourceMetafile, join(runtimeDirectory, 'metafile.json'))
  const candidate = join(scratch, 'bun')
  extractPinnedExecutable(archive, candidate)
  writeFileSync(
    join(scratch, 'release-materials.json'),
    `${JSON.stringify(
      {
        schemaVersion: 1,
        product: 'CrabCode TUI',
        version: releaseVersion,
        platform: 'x64-darwin',
        buildId: releaseBuildId,
        runtime: {
          bun: {
            version: x64DarwinBunRelease.version,
            url: x64DarwinBunRelease.url,
            sha256: x64DarwinBunRelease.sha256,
          },
        },
      },
      null,
      2,
    )}\n`,
  )

  const version = runChecked(candidate, ['--version']).stdout.trim()
  const revision = runChecked(candidate, ['--revision']).stdout.trim()
  if (version !== x64DarwinBunRelease.version || !revision.startsWith(`${version}+`)) {
    fail(`candidate identity is ${version}/${revision}`)
  }

  const stderrDispositions = new Set()
  for (let iteration = 1; iteration <= args.iterations; iteration += 1) {
    process.stderr.write(`x64 Darwin lifecycle ${iteration}/${args.iterations}\n`)
    const smoke = runChecked(process.execPath, [smokeScript], {
      cwd: repositoryRoot,
      env: {
        ...process.env,
        CRABCODE_SMOKE_PACKAGE_ROOT: scratch,
        CRABCODE_SMOKE_BUN: candidate,
        CRABCODE_SMOKE_CAPTURE_DARWIN_SAMPLE: '1',
      },
    })
    let report
    try {
      report = JSON.parse(smoke.stdout)
    } catch {
      fail(`iteration ${iteration} returned non-JSON output: ${smoke.stdout.slice(0, 2_000)}`)
    }
    try {
      assertTuiRuntimeSmokeSuccess(report)
    } catch {
      fail(`iteration ${iteration} violated the lifecycle contract: ${JSON.stringify(report)}`)
    }
    if (!new Set(['empty', 'classified-bun-baseline-no-avx-warning']).has(report.stderr)) {
      fail(`iteration ${iteration} returned unexpected stderr: ${JSON.stringify(report)}`)
    }
    stderrDispositions.add(report.stderr)
  }

  process.stdout.write(
    `${JSON.stringify(
      {
        platform: `${process.platform}/${process.arch}`,
        executionAuthority: isRosettaControl
          ? 'supplemental-rosetta-control'
          : 'native-intel-release-authority',
        bunVersion: version,
        bunRevision: revision,
        archiveSha256: x64DarwinBunRelease.sha256,
        iterations: `${args.iterations}/${args.iterations} success`,
        lifecycle: 'renderer context + initialize + 2 turns + end session',
        stderr: [...stderrDispositions].sort(),
      },
      null,
      2,
    )}\n`,
  )
} finally {
  rmSync(scratch, { recursive: true, force: true })
}
