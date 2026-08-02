#!/usr/bin/env bun

import { execFileSync, spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { basename, dirname, join, relative, resolve, sep } from 'node:path'
import { unzipSync, zipSync } from 'fflate'

const repositoryRoot = resolve(import.meta.dir, '..')
const sourceRepository = 'https://github.com/acosmi/CrabCode-TUI'
const bunVersion = '1.3.11'
const ripgrepVersion = '14.1.1'
const browserVersion = '0.28.0'
const accountBridgeRelease = 'v1.0.21'
const accountBridgeArtifactKey = '15MaLfvECwoagY8Oehclhk5nqsngGq0ECrKkRwOxDAQ'
const accountBridgeLockSha256 = '449fa63b0d3a276a99250b2e2158d8f5769b86571388475e8c88e72858e20c85'
const maximumDownloadBytes = 300 * 1024 * 1024
const maximumExpandedZipBytes = 600 * 1024 * 1024

const platforms = Object.freeze({
  'arm64-darwin': {
    rustTarget: 'aarch64-apple-darwin',
    executableExtension: '',
    archiveExtension: 'tar.gz',
    bunAsset: 'bun-darwin-aarch64.zip',
    bunRoot: 'bun-darwin-aarch64',
    bunSha256: '6f5a3467ed9caec4795bf78cd476507d9f870c7d57b86c945fcb338126772ffc',
    ripgrepAsset: 'ripgrep-14.1.1-aarch64-apple-darwin.tar.gz',
    ripgrepRoot: 'ripgrep-14.1.1-aarch64-apple-darwin',
    ripgrepSha256: '24ad76777745fbff131c8fbc466742b011f925bfa4fffa2ded6def23b5b937be',
    browserAsset: 'crabcode-browser-darwin-arm64',
    browserSha256: '0d84ab3253c63c25566e3c8998bbd205507c66dbf5e5108f1b359419f9d9b369',
    accountBridgeAsset: 'oauthapi-llm-arm64-darwin.zip',
    accountBridgeSha256: 'de446b62be95fd942b423cbcca118e3624fe67771ed556c159b51e78a33a5945',
    sharpKey: 'arm64-darwin',
  },
  'x64-darwin': {
    rustTarget: 'x86_64-apple-darwin',
    executableExtension: '',
    archiveExtension: 'tar.gz',
    bunAsset: 'bun-darwin-x64.zip',
    bunRoot: 'bun-darwin-x64',
    bunSha256: 'c4fe2b9247218b0295f24e895aaec8fee62e74452679a9026b67eacbd611a286',
    ripgrepAsset: 'ripgrep-14.1.1-x86_64-apple-darwin.tar.gz',
    ripgrepRoot: 'ripgrep-14.1.1-x86_64-apple-darwin',
    ripgrepSha256: 'fc87e78f7cb3fea12d69072e7ef3b21509754717b746368fd40d88963630e2b3',
    browserAsset: 'crabcode-browser-darwin-x64',
    browserSha256: '142cc952dccccdcd585c5e1d16468c98120f055aafb70c3cf20a6138122e7093',
    accountBridgeAsset: 'oauthapi-llm-x64-darwin.zip',
    accountBridgeSha256: 'ef01605dc95aab9ff268f1895feda4c5a3cacb029b5f9055ab14c65b38bb0123',
    sharpKey: 'x64-darwin',
  },
  'arm64-linux': {
    rustTarget: 'aarch64-unknown-linux-gnu',
    executableExtension: '',
    archiveExtension: 'tar.gz',
    bunAsset: 'bun-linux-aarch64.zip',
    bunRoot: 'bun-linux-aarch64',
    bunSha256: 'd13944da12a53ecc74bf6a720bd1d04c4555c038dfe422365356a7be47691fdf',
    ripgrepAsset: 'ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz',
    ripgrepRoot: 'ripgrep-14.1.1-aarch64-unknown-linux-gnu',
    ripgrepSha256: 'c827481c4ff4ea10c9dc7a4022c8de5db34a5737cb74484d62eb94a95841ab2f',
    browserAsset: 'crabcode-browser-linux-arm64',
    browserSha256: '2352cb7ca456d0e59fb278db142f823be1ae5fd19b8ad602b9bc2ff03e5e21a0',
    accountBridgeAsset: 'oauthapi-llm-arm64-linux.zip',
    accountBridgeSha256: '92a9e46c3cbee120b6b478f659face61ff2af269dc039dc1b60ccd4df67a3a67',
    sharpKey: 'arm64-linux',
  },
  'x64-linux': {
    rustTarget: 'x86_64-unknown-linux-gnu',
    executableExtension: '',
    archiveExtension: 'tar.gz',
    bunAsset: 'bun-linux-x64.zip',
    bunRoot: 'bun-linux-x64',
    bunSha256: '8611ba935af886f05a6f38740a15160326c15e5d5d07adef966130b4493607ed',
    ripgrepAsset: 'ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz',
    ripgrepRoot: 'ripgrep-14.1.1-x86_64-unknown-linux-musl',
    ripgrepSha256: '4cf9f2741e6c465ffdb7c26f38056a59e2a2544b51f7cc128ef28337eeae4d8e',
    browserAsset: 'crabcode-browser-linux-x64',
    browserSha256: '3fc7b6734dc161ef37efe8201518de08cb51bcef423802f114ff0b895bdb2899',
    accountBridgeAsset: 'oauthapi-llm-x64-linux.zip',
    accountBridgeSha256: '4b4d2a614186664c21943b2f7cc0125d61019d23fddb03e29799efc7316c477d',
    sharpKey: 'x64-linux',
  },
  'x64-win32': {
    rustTarget: 'x86_64-pc-windows-msvc',
    executableExtension: '.exe',
    archiveExtension: 'zip',
    bunAsset: 'bun-windows-x64.zip',
    bunRoot: 'bun-windows-x64',
    bunSha256: '066f8694f8b7d8df592452746d18f01710d4053e93030922dbc6e8c34a8c4b9f',
    ripgrepAsset: 'ripgrep-14.1.1-x86_64-pc-windows-msvc.zip',
    ripgrepRoot: 'ripgrep-14.1.1-x86_64-pc-windows-msvc',
    ripgrepSha256: 'd0f534024c42afd6cb4d38907c25cd2b249b79bbe6cc1dbee8e3e37c2b6e25a1',
    browserAsset: 'crabcode-browser-win32-x64.exe',
    browserSha256: 'e011561bb9f391cacb18a028a6763c5b469cebbf367bd6f81ec65df19a24bff0',
    accountBridgeAsset: 'oauthapi-llm-x64-win32.zip',
    accountBridgeSha256: '606147290ed777d3277872e7780c238a5f37cb3b7a37fffe28d60cd9c6a9ef37',
    sharpKey: 'x64-win32',
  },
})

const remoteLegalMaterials = Object.freeze([
  {
    id: 'bun-license',
    destination: `licenses/runtime/bun-${bunVersion}/LICENSE.md`,
    url: `https://raw.githubusercontent.com/oven-sh/bun/bun-v${bunVersion}/LICENSE.md`,
    sha256: '7068a9711ef8196d654e143447ed7976b3678ce21145b9da16e1f786528f15bb',
  },
  {
    id: 'browser-license',
    destination: `licenses/runtime/crabcode-browser-${browserVersion}/LICENSE`,
    url: `https://raw.githubusercontent.com/acosmi/agent-browser/v${browserVersion}/LICENSE`,
    sha256: '014bb31e83d5c2e76aea1cc6e82217346ab41362f32cb355ad0f5c10aa0aeaff',
  },
  {
    id: 'browser-notice',
    destination: `licenses/runtime/crabcode-browser-${browserVersion}/NOTICE`,
    url: `https://raw.githubusercontent.com/acosmi/agent-browser/v${browserVersion}/NOTICE`,
    sha256: '5023a4b335e82b1dfda5738df5df247916803f106a6a334e19874a154198a586',
  },
  {
    id: 'sharp-libvips-license',
    destination: 'licenses/runtime/sharp-libvips-1.3.2/LICENSE',
    url: 'https://raw.githubusercontent.com/lovell/sharp-libvips/v1.3.2/LICENSE',
    sha256: 'b40930bbcf80744c86c46a12bc9da056641d722716c378f5659b9e555ef833e1',
  },
  {
    id: 'sharp-libvips-notices',
    destination: 'licenses/runtime/sharp-libvips-1.3.2/THIRD-PARTY-NOTICES.md',
    url: 'https://raw.githubusercontent.com/lovell/sharp-libvips/v1.3.2/THIRD-PARTY-NOTICES.md',
    sha256: '25ffcfa69e28b1913ced27ec778b90f24911a1bb3021253577e8b0af55db0d49',
  },
])

function fail(message) {
  throw new Error(`release assembly failed: ${message}`)
}

function sha256Bytes(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function sha256File(path) {
  return sha256Bytes(readFileSync(path))
}

function jsonFile(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function portable(path) {
  return path.split(sep).join('/')
}

function compareText(left, right) {
  return left < right ? -1 : left > right ? 1 : 0
}

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true })
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`)
}

function parseArguments(argv) {
  const result = new Map()
  for (let index = 0; index < argv.length; index += 1) {
    const name = argv[index]
    if (name === '--help' || name === '-h') {
      return { help: true }
    }
    if (!name.startsWith('--') || index + 1 >= argv.length) {
      fail(`invalid argument near ${name}`)
    }
    if (result.has(name)) fail(`duplicate argument ${name}`)
    result.set(name, argv[index + 1])
    index += 1
  }
  const allowed = new Set(['--platform', '--version', '--output-dir'])
  for (const name of result.keys()) {
    if (!allowed.has(name)) fail(`unknown argument ${name}`)
  }
  return {
    help: false,
    platform: result.get('--platform'),
    version: result.get('--version'),
    outputDir: result.get('--output-dir') ?? 'dist/release',
  }
}

function printUsage() {
  console.log('usage: bun scripts/assemble-release.mjs --platform <arch-os> --version <semver> [--output-dir <dir>]')
  console.log(`platforms: ${Object.keys(platforms).join(', ')}`)
}

function hostPlatformToken() {
  const arch = process.arch === 'x64' ? 'x64' : process.arch === 'arm64' ? 'arm64' : null
  if (!arch || !['darwin', 'linux', 'win32'].includes(process.platform)) return null
  return `${arch}-${process.platform}`
}

function assertPortableArchivePath(path, expectedRoot) {
  if (
    path.includes('\\') ||
    path.startsWith('/') ||
    /^[A-Za-z]:/.test(path) ||
    path.includes('//') ||
    path.includes('\0') ||
    path.length > 1024
  ) {
    fail(`unsafe archive member path: ${path}`)
  }
  const stripped = path.endsWith('/') ? path.slice(0, -1) : path
  const segments = stripped.split('/')
  if (
    segments[0] !== expectedRoot ||
    segments.some(segment => segment === '' || segment === '.' || segment === '..')
  ) {
    fail(`archive member escapes ${expectedRoot}: ${path}`)
  }
}

function ensureRegularSource(path, label) {
  if (!existsSync(path)) fail(`${label} is missing: ${path}`)
  const info = lstatSync(path)
  if (!info.isFile() || info.isSymbolicLink() || info.size === 0) {
    fail(`${label} must be a non-empty regular file: ${path}`)
  }
}

function copyRegular(source, destination, executable = false) {
  ensureRegularSource(source, 'copy source')
  mkdirSync(dirname(destination), { recursive: true })
  copyFileSync(source, destination)
  if (executable && process.platform !== 'win32') chmodSync(destination, 0o755)
}

function writeRegular(destination, bytes, executable = false) {
  if (bytes.byteLength === 0) fail(`refusing to write empty release file: ${destination}`)
  mkdirSync(dirname(destination), { recursive: true })
  writeFileSync(destination, bytes)
  if (executable && process.platform !== 'win32') chmodSync(destination, 0o755)
}

async function downloadPinned(cacheDirectory, id, url, expectedSha256) {
  if (!url.startsWith('https://')) fail(`download URL is not HTTPS: ${url}`)
  if (!/^[a-f0-9]{64}$/.test(expectedSha256)) fail(`invalid pinned SHA-256 for ${id}`)
  mkdirSync(cacheDirectory, { recursive: true })
  const destination = join(cacheDirectory, id)
  if (existsSync(destination) && sha256File(destination) === expectedSha256) {
    return destination
  }
  rmSync(destination, { force: true })
  let lastError
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const response = await fetch(url, {
        redirect: 'follow',
        signal: AbortSignal.timeout(180_000),
        headers: { 'user-agent': 'CrabCode-TUI-release-builder' },
      })
      if (!response.ok) fail(`${id} download returned HTTP ${response.status}`)
      const declaredLength = Number(response.headers.get('content-length') ?? '0')
      if (declaredLength > maximumDownloadBytes) fail(`${id} exceeds the download size ceiling`)
      const chunks = []
      let total = 0
      const reader = response.body?.getReader()
      if (!reader) fail(`${id} response has no body`)
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        total += value.byteLength
        if (total > maximumDownloadBytes) fail(`${id} exceeds the download size ceiling`)
        chunks.push(Buffer.from(value))
      }
      const bytes = Buffer.concat(chunks, total)
      const actual = sha256Bytes(bytes)
      if (actual !== expectedSha256) {
        fail(`${id} SHA-256 mismatch: expected ${expectedSha256}, got ${actual}`)
      }
      writeFileSync(destination, bytes)
      return destination
    } catch (error) {
      lastError = error
      if (attempt < 3) await Bun.sleep(2 ** attempt * 1000)
    }
  }
  fail(`${id} could not be downloaded after three attempts: ${lastError}`)
}

function unzipEntries(path, expectedRoot) {
  const entries = unzipSync(new Uint8Array(readFileSync(path)))
  let total = 0
  const names = Object.keys(entries).sort()
  if (names.length === 0 || names.length > 4096) fail(`${basename(path)} has an invalid member count`)
  for (const name of names) {
    assertPortableArchivePath(name, expectedRoot)
    total += entries[name].byteLength
    if (total > maximumExpandedZipBytes) fail(`${basename(path)} exceeds expanded size ceiling`)
  }
  return entries
}

function extractZipFile(path, expectedRoot, relativePath) {
  const entries = unzipEntries(path, expectedRoot)
  const member = `${expectedRoot}/${relativePath}`
  const bytes = entries[member]
  if (!bytes || bytes.byteLength === 0) fail(`ZIP is missing ${member}`)
  return bytes
}

function extractTarFile(path, expectedRoot, relativePath) {
  const listing = spawnSync('tar', ['-tzf', path], {
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
  })
  if (listing.status !== 0) fail(`cannot inspect ${basename(path)}: ${listing.stderr}`)
  const names = listing.stdout.split(/\r?\n/u).filter(Boolean)
  for (const name of names) assertPortableArchivePath(name, expectedRoot)
  const member = `${expectedRoot}/${relativePath}`
  if (names.filter(name => name === member).length !== 1) fail(`tar archive must contain exactly one ${member}`)
  const extracted = spawnSync('tar', ['-xzOf', path, member], {
    encoding: null,
    maxBuffer: 256 * 1024 * 1024,
  })
  if (extracted.status !== 0 || !extracted.stdout?.length) {
    fail(`cannot extract ${member}: ${extracted.stderr?.toString() ?? ''}`)
  }
  return Buffer.from(extracted.stdout)
}

function walkFiles(root) {
  const result = []
  const visit = (directory) => {
    for (const name of readdirSync(directory).sort()) {
      const absolute = join(directory, name)
      const info = lstatSync(absolute)
      if (info.isSymbolicLink()) fail(`symlink is forbidden in release package: ${absolute}`)
      if (info.isDirectory()) visit(absolute)
      else if (info.isFile() && info.size > 0) result.push(absolute)
      else fail(`special or empty file is forbidden in release package: ${absolute}`)
    }
  }
  visit(root)
  return result
}

function stageAccountBridge(archive, packageDirectory, platformToken) {
  const entries = unzipEntries(archive, platformToken)
  let copied = 0
  for (const name of Object.keys(entries).sort()) {
    if (name.endsWith('/')) continue
    const relativeName = name.slice(platformToken.length + 1)
    if (/\.(?:go|c|cc|cpp|h|hpp|rs|ts)$/iu.test(relativeName) || /^(?:go\.mod|go\.sum)$/u.test(relativeName)) {
      fail(`Account Bridge source leaked into component archive: ${relativeName}`)
    }
    const destination = relativeName.startsWith('bin/')
      ? join(packageDirectory, relativeName)
      : join(packageDirectory, 'bin', 'account-bridge', relativeName)
    writeRegular(destination, entries[name], relativeName.startsWith('bin/'))
    copied += 1
  }
  if (copied < 10) fail('Account Bridge archive has an implausibly small file inventory')
}

function stageSharp(packageDirectory, platformKey) {
  const manifest = jsonFile(join(repositoryRoot, 'third_party/sharp-native/file-hashes.json'))
  if (manifest.schemaVersion !== 1 && manifest.schemaVersion !== undefined) {
    fail('unsupported Sharp native file-hash manifest')
  }
  const packages = manifest.packages?.[platformKey]
  if (!Array.isArray(packages) || packages.length === 0) fail(`Sharp native manifest has no ${platformKey} entry`)
  const summary = []
  for (const packageEntry of packages) {
    if (!/^@img\/[a-z0-9._-]+$/u.test(packageEntry.name) || !Array.isArray(packageEntry.files)) {
      fail(`invalid Sharp package entry for ${platformKey}`)
    }
    const sourceDirectory = join(repositoryRoot, 'node_modules', ...packageEntry.name.split('/'))
    for (const file of packageEntry.files) {
      if (!/^[a-zA-Z0-9._/+@-]+$/u.test(file.path) || file.path.split('/').some(segment => segment === '..')) {
        fail(`invalid Sharp file path: ${file.path}`)
      }
      const source = join(sourceDirectory, ...file.path.split('/'))
      ensureRegularSource(source, `${packageEntry.name}/${file.path}`)
      if (statSync(source).size !== file.size || sha256File(source) !== file.sha256) {
        fail(`Sharp native resource differs from its reviewed hash: ${packageEntry.name}/${file.path}`)
      }
      copyRegular(source, join(packageDirectory, 'node_modules', ...packageEntry.name.split('/'), ...file.path.split('/')))
    }
    summary.push({
      name: packageEntry.name,
      version: packageEntry.version,
      license: packageEntry.license,
      files: packageEntry.files.length,
    })
  }
  return summary
}

function legalFileNames(directory) {
  return readdirSync(directory)
    .filter(name => /^(?:licen[cs]e|copying|notice)(?:[-_.]|$)/iu.test(name))
    .filter(name => lstatSync(join(directory, name)).isFile())
    .sort()
}

function javascriptPackageRoot(inputPath) {
  const absolute = portable(resolve(repositoryRoot, inputPath))
  const marker = '/node_modules/'
  const index = absolute.lastIndexOf(marker)
  if (index < 0) return null
  const base = absolute.slice(0, index + marker.length)
  const segments = absolute.slice(index + marker.length).split('/')
  const count = segments[0]?.startsWith('@') ? 2 : 1
  if (segments.length < count) fail(`cannot resolve package root for metafile input ${inputPath}`)
  return resolve(base, ...segments.slice(0, count))
}

function licenseExpression(value) {
  if (typeof value === 'string') return value
  if (Array.isArray(value)) return value.map(licenseExpression).filter(Boolean).join(' OR ')
  if (value && typeof value === 'object') return JSON.stringify(value)
  return ''
}

function safePackageDirectoryName(name, version) {
  const value = `${name.replace(/^@/u, '').replaceAll('/', '__')}@${version}`
  if (!/^[A-Za-z0-9._+@-]+$/u.test(value)) fail(`unsafe package identity ${name}@${version}`)
  return value
}

function collectJavaScriptLicenses(packageDirectory) {
  const metafilePath = join(repositoryRoot, 'dist/tui-runtime/metafile.json')
  const metafile = jsonFile(metafilePath)
  const inputs = Object.keys(metafile.inputs ?? {})
  if (inputs.length < 100) fail('TUI runtime metafile has an implausibly small input graph')
  const supplementManifest = jsonFile(
    join(repositoryRoot, 'third_party/javascript-legal-supplements/manifest.json'),
  )
  const supplements = new Map(
    supplementManifest.packages.map(entry => [`${entry.packageName}@${entry.packageVersion}`, entry]),
  )
  const roots = new Set(inputs.map(javascriptPackageRoot).filter(Boolean).map(realpathSync))
  const packagesByIdentity = new Map()
  for (const packageRoot of roots) {
    const packageJsonPath = join(packageRoot, 'package.json')
    ensureRegularSource(packageJsonPath, 'bundled JavaScript package.json')
    const packageJsonRaw = readFileSync(packageJsonPath)
    const metadata = JSON.parse(packageJsonRaw.toString('utf8'))
    if (typeof metadata.name !== 'string' || typeof metadata.version !== 'string') {
      fail(`bundled JavaScript package has no canonical name/version: ${packageRoot}`)
    }
    const identity = `${metadata.name}@${metadata.version}`
    if (!packagesByIdentity.has(identity)) {
      packagesByIdentity.set(identity, { packageRoot, packageJsonRaw, metadata })
    }
  }

  const destinationRoot = join(packageDirectory, 'licenses', 'javascript')
  const summary = []
  for (const identity of [...packagesByIdentity.keys()].sort()) {
    const { packageRoot, packageJsonRaw, metadata } = packagesByIdentity.get(identity)
    const destination = join(destinationRoot, safePackageDirectoryName(metadata.name, metadata.version))
    mkdirSync(destination, { recursive: true })
    writeFileSync(join(destination, 'package.json'), packageJsonRaw)
    const copied = []
    for (const name of legalFileNames(packageRoot)) {
      copyRegular(join(packageRoot, name), join(destination, name))
      copied.push(name)
    }
    if (copied.length === 0) {
      const supplement = supplements.get(identity)
      if (!supplement || !Array.isArray(supplement.legalFiles) || supplement.legalFiles.length === 0) {
        fail(`bundled JavaScript package has no license material or reviewed supplement: ${identity}`)
      }
      if (
        sha256Bytes(packageJsonRaw) !== supplement.installedPackageJsonSha256 ||
        packageJsonRaw.length !== supplement.installedPackageJsonSize
      ) {
        fail(`reviewed JavaScript legal supplement no longer matches installed ${identity}`)
      }
      for (const legal of supplement.legalFiles) {
        const source = resolve(repositoryRoot, legal.sourcePath)
        if (!source.startsWith(join(repositoryRoot, 'third_party', 'javascript-legal-supplements'))) {
          fail(`legal supplement escapes reviewed corpus: ${legal.sourcePath}`)
        }
        ensureRegularSource(source, `${identity} legal supplement`)
        if (statSync(source).size !== legal.size || sha256File(source) !== legal.sha256) {
          fail(`legal supplement hash changed for ${identity}`)
        }
        const name = basename(legal.packageRelativePath)
        copyRegular(source, join(destination, name))
        copied.push(name)
      }
    }
    const expression = licenseExpression(metadata.license ?? metadata.licenses)
      || supplements.get(identity)?.declaredLicense
      || ''
    if (expression === '' || /UNLICENSED|SEE LICENSE IN$/iu.test(expression)) {
      fail(`bundled JavaScript package has no usable license declaration: ${identity}`)
    }
    summary.push({ name: metadata.name, version: metadata.version, license: expression, files: copied.sort() })
  }
  return { summary, metafile }
}

function cargoMetadata(manifestPath, target) {
  return JSON.parse(
    execFileSync(
      'cargo',
      ['metadata', '--format-version', '1', '--locked', '--filter-platform', target, '--manifest-path', manifestPath],
      { cwd: repositoryRoot, encoding: 'utf8', maxBuffer: 128 * 1024 * 1024 },
    ),
  )
}

function collectRustLicenses(packageDirectory, target) {
  const metadataDocuments = [
    cargoMetadata('crates/Cargo.toml', target),
    cargoMetadata('libs/acosmi-memory/Cargo.toml', target),
  ]
  const destinationRoot = join(packageDirectory, 'licenses', 'rust')
  const packages = new Map()
  for (const metadata of metadataDocuments) {
    const reachable = new Set(metadata.resolve?.nodes?.map(node => node.id) ?? [])
    for (const pkg of metadata.packages) {
      if (!reachable.has(pkg.id) || !pkg.source) continue
      const identity = `${pkg.name}@${pkg.version}`
      if (!packages.has(identity)) packages.set(identity, pkg)
    }
  }
  const summary = []
  for (const identity of [...packages.keys()].sort()) {
    const pkg = packages.get(identity)
    const sourceDirectory = dirname(pkg.manifest_path)
    const destination = join(destinationRoot, safePackageDirectoryName(pkg.name, pkg.version))
    mkdirSync(destination, { recursive: true })
    copyRegular(pkg.manifest_path, join(destination, 'Cargo.toml'))
    const copied = []
    for (const name of legalFileNames(sourceDirectory)) {
      copyRegular(join(sourceDirectory, name), join(destination, name))
      copied.push(name)
    }
    if (pkg.license_file) {
      const licenseFile = resolve(sourceDirectory, pkg.license_file)
      if (!licenseFile.startsWith(sourceDirectory)) fail(`Cargo license_file escapes ${identity}`)
      if (!copied.includes(basename(licenseFile))) {
        copyRegular(licenseFile, join(destination, basename(licenseFile)))
        copied.push(basename(licenseFile))
      }
    }
    if ((!pkg.license || pkg.license.trim() === '') && copied.length === 0) {
      fail(`Rust dependency has neither license expression nor legal file: ${identity}`)
    }
    writeFileSync(join(destination, 'LICENSE-EXPRESSION.txt'), `${pkg.license ?? 'SEE INCLUDED FILE'}\n`)
    summary.push({ name: pkg.name, version: pkg.version, license: pkg.license ?? null, files: copied.sort() })
  }
  copyRegular(join(repositoryRoot, 'LICENSE'), join(packageDirectory, 'licenses', 'common', 'MIT.txt'))
  copyRegular(
    join(repositoryRoot, 'crates/crabcode-markdown-core/LICENSE-Apache-2.0.txt'),
    join(packageDirectory, 'licenses', 'common', 'Apache-2.0.txt'),
  )
  copyRegular(
    join(repositoryRoot, 'third_party/release-licenses/BSL-1.0.txt'),
    join(packageDirectory, 'licenses', 'common', 'BSL-1.0.txt'),
  )
  return summary
}

function inventoryFiles(packageDirectory, excluded = new Set()) {
  return walkFiles(packageDirectory)
    .map(path => {
      const relativePath = portable(relative(packageDirectory, path))
      return {
        path: relativePath,
        sha256: sha256File(path),
        size: statSync(path).size,
      }
    })
    .filter(file => !excluded.has(file.path))
    .sort((left, right) => compareText(left.path, right.path))
}

function verifyManifest(packageDirectory) {
  const manifestPath = join(packageDirectory, 'release-manifest.json')
  const signaturePath = join(packageDirectory, 'release-manifest.sig')
  const manifest = jsonFile(manifestPath)
  const signature = jsonFile(signaturePath)
  if (
    signature.schemaVersion !== 1 ||
    signature.scheme !== 'sha256' ||
    signature.manifestSha256 !== sha256File(manifestPath)
  ) {
    fail('release-manifest.sig does not bind release-manifest.json')
  }
  const expected = manifest.files
  if (!Array.isArray(expected) || expected.length === 0) fail('release manifest has no files')
  const actual = inventoryFiles(
    packageDirectory,
    new Set(['release-manifest.json', 'release-manifest.sig']),
  )
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail('release package file inventory differs from release-manifest.json')
  }
}

function verifyPackageContract(packageDirectory, platformToken, version) {
  const extension = platforms[platformToken].executableExtension
  const required = [
    `crabcode${extension}`,
    `crabcode-tui${extension}`,
    `crabcode-cron${extension}`,
    `acosmi-memory-orchestrator${extension}`,
    `bun${extension}`,
    `crabcode-browser${extension}`,
    'dist/tui-runtime/index.js',
    'dist/tui-runtime/metafile.json',
    `dist/vendor/ripgrep/${platformToken}/rg${extension}`,
    `bin/oauthapi-llm${extension}`,
    'bin/account-bridge/provenance.json',
    'build-id',
    'release-materials.json',
    'release-manifest.json',
    'release-manifest.sig',
    'LICENSE',
    'OPEN_SOURCE.md',
    'OPEN_SOURCE.zh-CN.md',
    'THIRD_PARTY_NOTICES.md',
  ]
  for (const name of required) ensureRegularSource(join(packageDirectory, ...name.split('/')), `required release file ${name}`)
  const paths = walkFiles(packageDirectory).map(path => portable(relative(packageDirectory, path)))
  const forbidden = /(?:^|\/)(?:gui|app[-_]?server|frontend|archive|docs|claude\.md|agents\.md)(?:\/|$)/iu
  for (const path of paths) {
    if (forbidden.test(path)) fail(`non-TUI surface leaked into release: ${path}`)
    if (/\.(?:go|rs|ts|tsx|jsx|dmg|pkg|docx?|pdf|pptx?|xlsx?)$/iu.test(path)) {
      fail(`source/document artifact leaked into binary release: ${path}`)
    }
  }
  const buildId = readFileSync(join(packageDirectory, 'build-id'), 'utf8').trim()
  if (!new RegExp(`^${version.replaceAll('.', '\\.')}\\+[a-f0-9]{12}$`, 'u').test(buildId)) {
    fail(`invalid build-id ${buildId}`)
  }
  const metafile = jsonFile(join(packageDirectory, 'dist/tui-runtime/metafile.json'))
  if (
    metafile.crabcodeTuiBuild?.entryPoint !== 'src/entrypoints/tuiRuntime.ts' ||
    metafile.crabcodeTuiBuild?.output !== 'dist/tui-runtime/index.js' ||
    metafile.crabcodeTuiBuild?.version !== version ||
    metafile.crabcodeTuiBuild?.buildId !== buildId
  ) {
    fail('TUI runtime metafile identity does not match the release')
  }
  const output = Object.values(metafile.outputs ?? {})[0]
  if (Object.keys(metafile.outputs ?? {}).length !== 1 || (output?.imports?.length ?? 0) !== 0) {
    fail('TUI runtime bundle is not a closed single-output graph')
  }
  if (process.platform !== 'win32') {
    for (const name of [
      `crabcode${extension}`,
      `crabcode-tui${extension}`,
      `crabcode-cron${extension}`,
      `acosmi-memory-orchestrator${extension}`,
      `bun${extension}`,
      `crabcode-browser${extension}`,
    ]) {
      if ((statSync(join(packageDirectory, name)).mode & 0o111) === 0) fail(`required executable has no execute bit: ${name}`)
    }
  }
  verifyManifest(packageDirectory)
}

function createArchive(packageDirectory, outputDirectory, packageName, extension) {
  const archivePath = join(outputDirectory, `${packageName}.${extension}`)
  rmSync(archivePath, { force: true })
  if (extension === 'zip') {
    const input = {}
    for (const path of walkFiles(packageDirectory)) {
      input[`${packageName}/${portable(relative(packageDirectory, path))}`] = new Uint8Array(readFileSync(path))
    }
    writeFileSync(archivePath, zipSync(input, { level: 9 }))
  } else {
    const result = spawnSync('tar', ['-czf', archivePath, '-C', outputDirectory, packageName], {
      encoding: 'utf8',
    })
    if (result.status !== 0) fail(`tar creation failed: ${result.stderr}`)
  }
  ensureRegularSource(archivePath, 'final release archive')
  return archivePath
}

async function main() {
  const args = parseArguments(process.argv.slice(2))
  if (args.help) {
    printUsage()
    return
  }
  if (!args.platform || !platforms[args.platform]) fail('a supported --platform is required')
  if (!args.version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?$/u.test(args.version)) {
    fail('a canonical --version is required')
  }
  if (hostPlatformToken() !== args.platform) {
    fail(`native release assembly for ${args.platform} must run on that platform (host=${hostPlatformToken()})`)
  }
  const packageJson = jsonFile(join(repositoryRoot, 'package.json'))
  if (packageJson.version !== args.version) fail(`package.json version ${packageJson.version} != ${args.version}`)
  const gitStatus = execFileSync('git', ['status', '--porcelain', '--untracked-files=normal'], {
    cwd: repositoryRoot,
    encoding: 'utf8',
  }).trim()
  if (gitStatus !== '') fail('release source worktree is not clean')
  const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: repositoryRoot, encoding: 'utf8' }).trim()
  if (!/^[a-f0-9]{40}$/u.test(commit)) fail('cannot resolve release source commit')
  const expectedTag = `v${args.version}`
  const tags = execFileSync('git', ['tag', '--points-at', 'HEAD'], { cwd: repositoryRoot, encoding: 'utf8' })
    .split(/\r?\n/u)
    .filter(Boolean)
  if (!tags.includes(expectedTag)) fail(`release source commit is not tagged ${expectedTag}`)
  if (sha256File(join(repositoryRoot, 'components/oauthapi-llm/UPSTREAM.lock')) !== accountBridgeLockSha256) {
    fail('Account Bridge UPSTREAM.lock differs from the signed component release')
  }

  const info = platforms[args.platform]
  const outputDirectory = resolve(repositoryRoot, args.outputDir)
  if (outputDirectory === repositoryRoot || !outputDirectory.startsWith(`${repositoryRoot}${sep}`)) {
    fail('--output-dir must be a child of the repository')
  }
  mkdirSync(outputDirectory, { recursive: true })
  const packageName = `crabcode-${args.version}-${args.platform}`
  const packageDirectory = join(outputDirectory, packageName)
  rmSync(packageDirectory, { recursive: true, force: true })
  mkdirSync(packageDirectory, { recursive: true })
  const cacheDirectory = join(repositoryRoot, 'dist', 'release-cache')
  const extension = info.executableExtension
  const buildId = `${args.version}+${commit.slice(0, 12)}`

  const buildInputs = [
    ['crabcode launcher', join(repositoryRoot, 'crates/target/release', `crabcode-pure-tui-launcher${extension}`), `crabcode${extension}`],
    ['native TUI', join(repositoryRoot, 'crates/target/release', `crabcode-tui${extension}`), `crabcode-tui${extension}`],
    ['cron sidecar', join(repositoryRoot, 'crates/target/release', `crabcode-cron${extension}`), `crabcode-cron${extension}`],
    ['memory orchestrator', join(repositoryRoot, 'libs/acosmi-memory/target/release', `acosmi-memory-orchestrator${extension}`), `acosmi-memory-orchestrator${extension}`],
  ]
  for (const [label, source, destination] of buildInputs) {
    ensureRegularSource(source, label)
    copyRegular(source, join(packageDirectory, destination), true)
  }
  copyRegular(
    join(repositoryRoot, 'dist/tui-runtime/index.js'),
    join(packageDirectory, 'dist/tui-runtime/index.js'),
  )
  copyRegular(
    join(repositoryRoot, 'dist/tui-runtime/metafile.json'),
    join(packageDirectory, 'dist/tui-runtime/metafile.json'),
  )
  writeFileSync(join(packageDirectory, 'build-id'), `${buildId}\n`)

  const bunUrl = `https://github.com/oven-sh/bun/releases/download/bun-v${bunVersion}/${info.bunAsset}`
  const ripgrepUrl = `https://github.com/BurntSushi/ripgrep/releases/download/${ripgrepVersion}/${info.ripgrepAsset}`
  const browserUrl = `https://github.com/acosmi/agent-browser/releases/download/v${browserVersion}/${info.browserAsset}`
  const accountBridgeUrl = `https://github.com/acosmi/crabcode/releases/download/${accountBridgeRelease}/${info.accountBridgeAsset}`
  const [bunArchive, ripgrepArchive, browserBinary, accountBridgeArchive] = await Promise.all([
    downloadPinned(cacheDirectory, info.bunAsset, bunUrl, info.bunSha256),
    downloadPinned(cacheDirectory, info.ripgrepAsset, ripgrepUrl, info.ripgrepSha256),
    downloadPinned(cacheDirectory, info.browserAsset, browserUrl, info.browserSha256),
    downloadPinned(cacheDirectory, info.accountBridgeAsset, accountBridgeUrl, info.accountBridgeSha256),
  ])

  const bunName = `bun${extension}`
  writeRegular(
    join(packageDirectory, bunName),
    extractZipFile(bunArchive, info.bunRoot, bunName),
    true,
  )
  const ripgrepName = `rg${extension}`
  writeRegular(
    join(packageDirectory, 'dist/vendor/ripgrep', args.platform, ripgrepName),
    info.ripgrepAsset.endsWith('.zip')
      ? extractZipFile(ripgrepArchive, info.ripgrepRoot, ripgrepName)
      : extractTarFile(ripgrepArchive, info.ripgrepRoot, ripgrepName),
    true,
  )
  copyRegular(browserBinary, join(packageDirectory, `crabcode-browser${extension}`), true)
  stageAccountBridge(accountBridgeArchive, packageDirectory, args.platform)

  for (const [binary, version] of [
    [join(packageDirectory, bunName), bunVersion],
    [join(packageDirectory, 'dist/vendor/ripgrep', args.platform, ripgrepName), ripgrepVersion],
    [join(packageDirectory, `crabcode-browser${extension}`), browserVersion],
  ]) {
    const probe = spawnSync(binary, ['--version'], { encoding: 'utf8', timeout: 30_000 })
    const output = `${probe.stdout ?? ''}\n${probe.stderr ?? ''}`
    if (probe.status !== 0 || !output.includes(version)) {
      fail(`${basename(binary)} version probe failed for ${version}: ${output.trim()}`)
    }
  }

  const sharpSummary = stageSharp(packageDirectory, info.sharpKey)
  const { summary: javascriptSummary, metafile } = collectJavaScriptLicenses(packageDirectory)
  const rustSummary = collectRustLicenses(packageDirectory, info.rustTarget)

  for (const name of ['LICENSE', 'OPEN_SOURCE.md', 'OPEN_SOURCE.zh-CN.md', 'THIRD_PARTY_NOTICES.md']) {
    copyRegular(join(repositoryRoot, name), join(packageDirectory, name))
  }
  const legalMaterials = []
  for (const material of remoteLegalMaterials) {
    const source = await downloadPinned(cacheDirectory, material.id, material.url, material.sha256)
    copyRegular(source, join(packageDirectory, ...material.destination.split('/')))
    legalMaterials.push({ url: material.url, sha256: material.sha256, path: material.destination })
  }
  const ripgrepLicenseDirectory = join(packageDirectory, 'licenses/runtime', `ripgrep-${ripgrepVersion}`)
  for (const name of ['LICENSE-MIT', 'UNLICENSE']) {
    const bytes = info.ripgrepAsset.endsWith('.zip')
      ? extractZipFile(ripgrepArchive, info.ripgrepRoot, name)
      : extractTarFile(ripgrepArchive, info.ripgrepRoot, name)
    writeRegular(join(ripgrepLicenseDirectory, name), bytes)
  }

  const bridgeLock = jsonFile(join(repositoryRoot, 'components/oauthapi-llm/UPSTREAM.lock'))
  const { verifyPackagedArtifact } = await import('../src/services/accountBridge/runtimeManager.ts')
  const verifiedBridge = await verifyPackagedArtifact({
    binaryPath: join(packageDirectory, 'bin', `oauthapi-llm${extension}`),
    metadataDir: join(packageDirectory, 'bin', 'account-bridge'),
    expectedComponentVersion: bridgeLock.componentVersion,
    expectedPlatform: args.platform,
    expectedProtocolVersion: bridgeLock.protocolVersion,
    artifactPublicKeyBase64URL: accountBridgeArtifactKey,
  })

  const releaseMaterials = {
    schemaVersion: 1,
    product: 'CrabCode TUI',
    version: args.version,
    platform: args.platform,
    buildId,
    source: {
      repository: sourceRepository,
      commit,
      tag: expectedTag,
    },
    runtime: {
      bun: { version: bunVersion, url: bunUrl, sha256: info.bunSha256 },
      ripgrep: { version: ripgrepVersion, url: ripgrepUrl, sha256: info.ripgrepSha256 },
      browser: { version: browserVersion, url: browserUrl, sha256: info.browserSha256 },
      accountBridge: {
        componentVersion: bridgeLock.componentVersion,
        protocolVersion: bridgeLock.protocolVersion,
        url: accountBridgeUrl,
        sha256: info.accountBridgeSha256,
        provenanceSha256: sha256File(join(packageDirectory, 'bin/account-bridge/provenance.json')),
        signatureVerified: true,
        platformSignature: verifiedBridge.platformSignature ?? null,
      },
      sharp: sharpSummary,
    },
    dependencyLicenses: {
      javascriptPackages: javascriptSummary,
      rustPackages: rustSummary,
      remoteMaterials: legalMaterials,
      accountBridgeMaterials: 'bin/account-bridge/third-party-licenses.manifest.json',
    },
    bundle: {
      entryPoint: metafile.crabcodeTuiBuild.entryPoint,
      inputs: Object.keys(metafile.inputs).length,
      outputs: Object.keys(metafile.outputs).length,
      externalImports: Object.values(metafile.outputs).flatMap(output => output.imports ?? []).length,
    },
  }
  writeJson(join(packageDirectory, 'release-materials.json'), releaseMaterials)

  const manifest = {
    schemaVersion: 1,
    product: 'CrabCode TUI',
    version: args.version,
    platform: args.platform,
    buildId,
    sourceCommit: commit,
    files: inventoryFiles(packageDirectory),
  }
  const manifestPath = join(packageDirectory, 'release-manifest.json')
  writeJson(manifestPath, manifest)
  writeJson(join(packageDirectory, 'release-manifest.sig'), {
    schemaVersion: 1,
    scheme: 'sha256',
    manifestSha256: sha256File(manifestPath),
    trustAnchor: 'The installer authenticates the containing archive against checksums-sha256.txt from the same immutable GitHub Release tag.',
  })

  verifyPackageContract(packageDirectory, args.platform, args.version)
  const archivePath = createArchive(
    packageDirectory,
    outputDirectory,
    packageName,
    info.archiveExtension,
  )
  console.log(
    JSON.stringify(
      {
        package: portable(relative(repositoryRoot, archivePath)),
        bytes: statSync(archivePath).size,
        sha256: sha256File(archivePath),
        files: walkFiles(packageDirectory).length,
        javascriptPackages: javascriptSummary.length,
        rustPackages: rustSummary.length,
        accountBridgeSignatureVerified: true,
      },
      null,
      2,
    ),
  )
}

await main()
