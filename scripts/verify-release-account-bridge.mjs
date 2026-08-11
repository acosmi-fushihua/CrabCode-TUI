#!/usr/bin/env bun

import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { unzipSync } from 'fflate'
import {
  accountBridgeReleaseAssetUrl,
  accountBridgeReleasePins,
  publicAccountBridgePlatforms,
} from './release-account-bridge-pins.mjs'
import { verifyPackagedArtifact } from '../src/services/accountBridge/runtimeManager.ts'

const repositoryRoot = resolve(import.meta.dir, '..')
const maximumDownloadBytes = 64 * 1024 * 1024
const maximumExpandedBytes = 256 * 1024 * 1024

function fail(message) {
  throw new Error(`Account Bridge release preflight failed: ${message}`)
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function parseArguments(argv) {
  const result = { cacheDirectory: null, help: false }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--help' || argument === '-h') result.help = true
    else if (argument === '--cache-dir') {
      const value = argv[index + 1]
      if (!value) fail('--cache-dir requires a path')
      result.cacheDirectory = resolve(value)
      index += 1
    } else fail(`unknown argument ${argument}`)
  }
  return result
}

async function downloadPinned(asset, expectedSha256, cacheDirectory) {
  let bytes
  if (cacheDirectory) {
    bytes = await readFile(join(cacheDirectory, asset))
  } else {
    console.log(`downloading pinned Account Bridge input ${asset}`)
    let response
    try {
      response = await fetch(accountBridgeReleaseAssetUrl(asset), {
        redirect: 'follow',
        signal: AbortSignal.timeout(300_000),
      })
    } catch (error) {
      fail(`cannot download ${asset}: ${String(error)}`)
    }
    if (!response.ok || !response.body) fail(`cannot download ${asset}: HTTP ${response.status}`)
    const declaredLength = Number(response.headers.get('content-length') ?? 0)
    if (declaredLength > maximumDownloadBytes) fail(`${asset} exceeds the download ceiling`)
    const chunks = []
    let received = 0
    for await (const chunk of response.body) {
      received += chunk.byteLength
      if (received > maximumDownloadBytes) fail(`${asset} exceeds the download ceiling`)
      chunks.push(Buffer.from(chunk))
    }
    bytes = Buffer.concat(chunks)
  }
  if (bytes.length === 0 || sha256(bytes) !== expectedSha256) {
    fail(`${asset} differs from its reviewed SHA-256`)
  }
  if (!cacheDirectory) console.log(`downloaded and hashed ${asset}`)
  return bytes
}

function parseChecksums(raw) {
  const result = new Map()
  for (const line of raw.toString('utf8').trimEnd().split('\n')) {
    const match = /^([a-f0-9]{64})  (oauthapi-llm-[a-z0-9-]+\.zip)$/u.exec(line)
    if (!match || result.has(match[2])) fail('official checksum manifest is malformed')
    result.set(match[2], match[1])
  }
  const pins = Object.values(accountBridgeReleasePins.platforms)
  if (result.size !== pins.length) fail('official checksum manifest has an unexpected asset set')
  for (const pin of pins) {
    if (result.get(pin.asset) !== pin.sha256) {
      fail(`official checksum manifest differs for ${pin.asset}`)
    }
  }
}

function unzipReviewedArchive(archive, platform) {
  const expectedRoot = `${platform}/`
  let expandedBytes = 0
  let entries
  try {
    entries = unzipSync(archive, {
      filter(entry) {
        expandedBytes += entry.originalSize
        if (expandedBytes > maximumExpandedBytes) fail(`${platform} archive exceeds the expansion ceiling`)
        const segments = entry.name.split('/')
        if (!entry.name.startsWith(expectedRoot) || entry.name.includes('\\')
          || entry.name.includes('\0') || segments.some(segment => segment === '.' || segment === '..')) {
          fail(`${platform} archive contains an unsafe path`)
        }
        return true
      },
    })
  } catch (error) {
    if (String(error).includes('Account Bridge release preflight failed:')) throw error
    fail(`${platform} archive is not a valid reviewed ZIP: ${String(error)}`)
  }
  return entries
}

async function stageArchive(archive, platform, root) {
  const entries = unzipReviewedArchive(archive, platform)
  const packageRoot = join(root, platform)
  const metadataDirectory = join(packageRoot, 'bin', 'account-bridge')
  await mkdir(metadataDirectory, { recursive: true })
  let fileCount = 0
  for (const name of Object.keys(entries).sort()) {
    if (name.endsWith('/')) continue
    const relativeName = name.slice(platform.length + 1)
    const bytes = entries[name]
    if (relativeName === '' || bytes.length === 0) fail(`${platform} archive contains an empty file`)
    const destination = relativeName.startsWith('bin/')
      ? join(packageRoot, relativeName)
      : join(metadataDirectory, relativeName)
    await mkdir(dirname(destination), { recursive: true })
    await writeFile(destination, bytes)
    fileCount += 1
  }
  if (fileCount < 10) fail(`${platform} archive has an implausibly small inventory`)
  return { packageRoot, metadataDirectory }
}

async function main() {
  const args = parseArguments(process.argv.slice(2))
  if (args.help) {
    console.log('Usage: bun scripts/verify-release-account-bridge.mjs [--cache-dir PATH]')
    return
  }
  for (const [name, expected] of [
    ['ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL', accountBridgeReleasePins.artifactPublicKeyBase64URL],
    ['ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL', accountBridgeReleasePins.eligibilityPublicKeyBase64URL],
  ]) {
    if (process.env[name] !== expected) fail(`${name} differs from the reviewed release pin`)
  }

  const lockRaw = await readFile(join(repositoryRoot, 'components/oauthapi-llm/UPSTREAM.lock'))
  if (sha256(lockRaw) !== accountBridgeReleasePins.upstreamLockSha256) {
    fail('local UPSTREAM.lock differs from the reviewed release pin')
  }
  const lock = JSON.parse(lockRaw.toString('utf8'))
  if (lock.componentVersion !== accountBridgeReleasePins.componentVersion
    || lock.protocolVersion !== accountBridgeReleasePins.protocolVersion) {
    fail('local Account Bridge version/protocol differs from the reviewed release pin')
  }

  const checksums = await downloadPinned(
    accountBridgeReleasePins.checksums.asset,
    accountBridgeReleasePins.checksums.sha256,
    args.cacheDirectory,
  )
  parseChecksums(checksums)
  const archives = await Promise.all(publicAccountBridgePlatforms.map(async platform => {
    const pin = accountBridgeReleasePins.platforms[platform]
    return [platform, await downloadPinned(pin.asset, pin.sha256, args.cacheDirectory)]
  }))

  const stagingRoot = await mkdtemp(join(tmpdir(), 'crabcode-account-bridge-preflight-'))
  try {
    for (const [platform, archive] of archives) {
      const { packageRoot, metadataDirectory } = await stageArchive(archive, platform, stagingRoot)
      const packagedLock = await readFile(join(metadataDirectory, 'UPSTREAM.lock'))
      if (!packagedLock.equals(lockRaw)) fail(`${platform} UPSTREAM.lock differs from the repository`)
      const extension = platform.endsWith('win32') ? '.exe' : ''
      await verifyPackagedArtifact({
        binaryPath: join(packageRoot, 'bin', `oauthapi-llm${extension}`),
        metadataDir: metadataDirectory,
        expectedComponentVersion: accountBridgeReleasePins.componentVersion,
        expectedPlatform: platform,
        expectedProtocolVersion: accountBridgeReleasePins.protocolVersion,
        artifactPublicKeyBase64URL: accountBridgeReleasePins.artifactPublicKeyBase64URL,
        eligibilityPublicKeyBase64URL: accountBridgeReleasePins.eligibilityPublicKeyBase64URL,
      })
      console.log(`verified Account Bridge ${platform}`)
    }
  } finally {
    await rm(stagingRoot, { recursive: true, force: true })
  }
}

main().catch(error => {
  console.error(error instanceof Error ? error.message : String(error))
  process.exitCode = 1
})
