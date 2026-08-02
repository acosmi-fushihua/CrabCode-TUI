#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import {
  existsSync,
  lstatSync,
  readFileSync,
  statSync,
} from 'node:fs'
import { dirname, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const failures = []
const portable = value => value.split(sep).join('/')
const fail = message => failures.push(message)

function run(command, args, cwd = root) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
  })
  if (result.status !== 0) {
    throw new Error(
      `${command} ${args.join(' ')} failed: ${result.stderr.trim() || `status ${result.status}`}`,
    )
  }
  return result.stdout
}

const tracked = run('git', [
  'ls-files',
  '-z',
  '--cached',
  '--others',
  '--exclude-standard',
])
  .split('\0')
  .filter(Boolean)
  .sort()
const trackedSet = new Set(tracked)

const requiredFiles = [
  'README.md',
  'README.zh-CN.md',
  'LICENSE',
  'OPEN_SOURCE.md',
  'OPEN_SOURCE.zh-CN.md',
  'CONTRIBUTING.md',
  'CONTRIBUTING.zh-CN.md',
  'SECURITY.md',
  'SECURITY.zh-CN.md',
  'THIRD_PARTY_NOTICES.md',
  'package.json',
  'bun.lock',
  'crates/Cargo.toml',
  'components/oauthapi-llm/go.mod',
  'src/entrypoints/tuiRuntime.ts',
  'crates/crabcode-tui/src/main.rs',
  'crates/crabcode-cli/src/pure_tui_launcher.rs',
]
for (const path of requiredFiles) {
  if (!trackedSet.has(path)) fail(`required open-source TUI file is not tracked: ${path}`)
}

const allowedTopLevelDirectories = new Set([
  '.github',
  'components',
  'crates',
  'libs',
  'scripts',
  'src',
  'tests',
  'third_party',
])
const allowedRootFiles = new Set([
  '.gitattributes',
  '.gitignore',
  'CHANGELOG.md',
  'CONTRIBUTING.md',
  'CONTRIBUTING.zh-CN.md',
  'LICENSE',
  'Makefile',
  'OPEN_SOURCE.md',
  'OPEN_SOURCE.zh-CN.md',
  'README.md',
  'README.zh-CN.md',
  'SECURITY.md',
  'SECURITY.zh-CN.md',
  'THIRD_PARTY_NOTICES.md',
  'biome.json',
  'bun.lock',
  'bunfig.toml',
  'package.json',
  'tsconfig.json',
  'tsconfig.tui-runtime.json',
])
const forbiddenPrefixes = [
  '.claude/',
  '.local-audit/',
  'apps/',
  'archive/',
  'ci/',
  'contracts/',
  'docs/',
  'e2e/',
  'frontend/',
  'src/appServer/',
  'src/bridge/',
  'src/cli/transports/',
  'src/remote/',
  'src/server/',
  'src/tools/DesktopAutomationTool/',
  'src/utils/chromeAutomation/',
  'src/utils/desktopAutomation/',
  'src/voice/',
  'src/components/',
  'src/ink/',
  'src/screens/',
]
const forbiddenExactPaths = new Set([
  'src/cli/remoteIO.ts',
  'src/utils/plugins/presentationAssets.ts',
])
const forbiddenPath = /(?:^|\/)(?:gui|tauri|app[-_]?server|ink)(?:\/|\.|$)/iu
const forbiddenRootDocument = /^(?:ACOSMI|AGENTS|AUDIT[^/]*|CLAUDE|CRABCODE|MIGRATION[^/]*|ONBOARDING|PLAN[^/]*)\.(?:md|json|ya?ml)$/iu
const forbiddenArtifact = /\.(?:7z|app|dll|dmg|docx?|exe|gz|pdf|pkg|pptx?|rar|so|tar|tgz|xlsx?|zip)$/iu

let trackedBytes = 0
for (const path of tracked) {
  const segments = path.split('/')
  if (segments.length === 1) {
    if (!allowedRootFiles.has(path)) fail(`unexpected root file: ${path}`)
    if (forbiddenRootDocument.test(path)) fail(`internal project document is forbidden: ${path}`)
  } else if (!allowedTopLevelDirectories.has(segments[0])) {
    fail(`unexpected top-level directory: ${segments[0]}/ (${path})`)
  }
  if (forbiddenPrefixes.some(prefix => path.startsWith(prefix))) {
    fail(`forbidden non-TUI path is tracked: ${path}`)
  }
  if (forbiddenExactPaths.has(path)) fail(`forbidden non-TUI file is tracked: ${path}`)
  if (forbiddenPath.test(path)) fail(`GUI/AppServer/Ink path is tracked: ${path}`)
  if (/(?:^|\/)(?:AGENTS|CLAUDE)\.md$/iu.test(path)) {
    fail(`agent-project instruction file is tracked: ${path}`)
  }
  if (forbiddenArtifact.test(path)) fail(`binary/archive/document artifact is tracked: ${path}`)
  const absolute = resolve(root, path)
  if (!existsSync(absolute)) {
    fail(`tracked path is missing from the working tree: ${path}`)
    continue
  }
  const info = lstatSync(absolute)
  if (!info.isFile() || info.isSymbolicLink()) {
    fail(`tracked path must be a regular file: ${path}`)
    continue
  }
  trackedBytes += info.size
}
if (tracked.length > 5_000) fail(`tracked file ceiling exceeded: ${tracked.length} > 5000`)
if (trackedBytes > 80 * 1024 * 1024) {
  fail(`tracked byte ceiling exceeded: ${trackedBytes} > 80 MiB`)
}

const exactScripts = new Set([
  'scripts/build-account-bridge.ts',
  'scripts/build-ts.ts',
  'scripts/repository-boundary.mjs',
  'scripts/run-bun-test.ts',
  'scripts/run-full-test-suite.ts',
  'scripts/tui-runtime-smoke.mjs',
])
for (const path of tracked.filter(path => path.startsWith('scripts/'))) {
  if (!exactScripts.has(path)) fail(`unexpected build/governance script: ${path}`)
}
const exactWorkflows = new Set([
  '.github/actionlint.yaml',
  '.github/workflows/ci.yml',
])
for (const path of tracked.filter(path => path.startsWith('.github/'))) {
  if (!exactWorkflows.has(path)) fail(`unexpected GitHub project file: ${path}`)
}

const exactCrates = new Set([
  'acosmi-config',
  'acosmi-cron-ledger',
  'acosmi-daemon-launcher',
  'acosmi-exec',
  'acosmi-executor',
  'acosmi-generation-lease',
  'acosmi-heartbeat',
  'acosmi-index',
  'acosmi-permission',
  'acosmi-sandbox',
  'acosmi-scheduler',
  'acosmi-shell-parser',
  'acosmi-supervisor',
  'acosmi-types',
  'acosmi-util-absolute-path',
  'crabcode-cli',
  'crabcode-cron',
  'crabcode-markdown',
  'crabcode-markdown-core',
  'crabcode-mermaid',
  'crabcode-pager-render',
  'crabcode-ratatui-inline',
  'crabcode-ratatui-textarea',
  'crabcode-tui',
])
for (const path of tracked.filter(path => path.startsWith('crates/'))) {
  const segment = path.split('/')[1]
  if (
    !new Set(['Cargo.toml', 'Cargo.lock', 'clippy.toml', 'rustfmt.toml']).has(segment) &&
    !exactCrates.has(segment)
  ) {
    fail(`Rust crate outside the TUI product closure is tracked: ${path}`)
  }
}

const exactThirdParty = new Set([
  'crossterm-0.28.1-patched',
  'dagre_rust',
  'graphlib_rust',
  'javascript-legal-supplements',
  'mermaid-to-svg',
  'ordered_hashmap',
  'permutation_iterator-0.1.2-patched',
  'ratatui-0.29.0-patched',
  'sharp-native',
])
for (const path of tracked.filter(path => path.startsWith('third_party/'))) {
  if (!exactThirdParty.has(path.split('/')[1])) {
    fail(`third-party tree outside the TUI runtime closure is tracked: ${path}`)
  }
}

function workspaceDirectories(manifest) {
  const metadata = JSON.parse(
    run('cargo', ['metadata', '--manifest-path', manifest, '--no-deps', '--format-version', '1']),
  )
  return new Set(
    metadata.workspace_members.map(id => {
      const pkg = metadata.packages.find(candidate => candidate.id === id)
      return portable(relative(resolve(root, dirname(manifest)), dirname(pkg.manifest_path)))
    }),
  )
}
function compareSet(label, actual, expected) {
  const left = [...actual].sort()
  const right = [...expected].sort()
  if (JSON.stringify(left) !== JSON.stringify(right)) {
    fail(`${label} differs from the exact TUI closure: ${JSON.stringify(left)}`)
  }
}
compareSet('crates workspace', workspaceDirectories('crates/Cargo.toml'), exactCrates)
compareSet(
  'memory workspace',
  workspaceDirectories('libs/acosmi-memory/Cargo.toml'),
  new Set([
    'acosmi-memory-adapter',
    'acosmi-memory-core',
    'acosmi-memory-journal',
    'acosmi-memory-orchestrator',
    'acosmi-memory-parse',
    'acosmi-memory-queue',
    'acosmi-memory-se',
    'acosmi-memory-session',
    'acosmi-memory-transaction',
    'acosmi-memory-vfs',
  ]),
)
compareSet(
  'search workspace',
  workspaceDirectories('libs/acosmi-se/Cargo.toml'),
  new Set([
    'acosmi-common',
    'acosmi-gpu',
    'acosmi-gridstore',
    'acosmi-macros',
    'acosmi-posting-list',
    'acosmi-quantization',
    'acosmi-segment',
    'acosmi-sparse',
  ]),
)

for (const manifest of tracked.filter(path => path.endsWith('Cargo.toml'))) {
  const text = readFileSync(resolve(root, manifest), 'utf8')
  for (const line of text.split(/\r?\n/u)) {
    if (line.trimStart().startsWith('#')) continue
    for (const match of line.matchAll(/\bpath\s*=\s*"([^"]+)"/gu)) {
      const target = resolve(root, dirname(manifest), match[1])
      if (!existsSync(target)) fail(`broken Cargo path dependency: ${manifest} -> ${match[1]}`)
    }
  }
}

const packageJson = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8'))
for (const name of ['ink', 'react', 'react-dom', 'react-reconciler', 'usehooks-ts']) {
  if (packageJson.dependencies?.[name] || packageJson.devDependencies?.[name]) {
    fail(`forbidden GUI dependency remains in package.json: ${name}`)
  }
}
if (packageJson.license !== 'MIT') fail('package.json license must be MIT')
if (packageJson.repository?.url !== 'https://github.com/acosmi/CrabCode-TUI.git') {
  fail('package.json repository must point at the public TUI repository')
}

const forbiddenSourceFragments = [
  ['internal audit/plan document reference', /docs\/(?:audit|audits|chonggou|deepseek|claude|log)(?:\/|\b)/iu],
  ['removed worker application reference', /apps\/agent-worker(?:\/|\b)/u],
  ['removed desktop frontend reference', /frontend\/tauri(?:\/|\b)/u],
  ['removed AppServer source reference', /src\/appServer(?:\/|\b)/u],
  ['external GUI Git-workflow surface', /surface\s*:\s*['"]external['"]/u],
  ['removed SDK event consumer latch', /\bsetSdkEventConsumerActive\b/u],
  ['removed GUI autocompaction switch', /\bCRABCODE_SUPPRESS_INLOOP_AUTOCOMPACT\b/u],
  ['removed desktop artifact state', /\bdesktopArtifactAccountEpochMs\b/u],
]
for (const path of tracked.filter(path => {
  const sourceRoot = /^(?:components|crates|libs|src)\//u.test(path)
  const sourceExtension = /\.(?:go|rs|[cm]?[jt]sx?)$/u.test(path)
  return sourceRoot && sourceExtension
})) {
  const absolute = resolve(root, path)
  if (!existsSync(absolute)) continue
  const source = readFileSync(absolute, 'utf8')
  for (const [label, pattern] of forbiddenSourceFragments) {
    if (pattern.test(source)) fail(`${label} remains in ${path}`)
  }
}

const metafilePath = resolve(root, 'dist/tui-runtime/metafile.json')
if (!existsSync(metafilePath)) {
  fail('build metafile is missing; run bun run build:ts first')
} else {
  const metafile = JSON.parse(readFileSync(metafilePath, 'utf8'))
  const outputs = Object.values(metafile.outputs ?? {})
  if (outputs.length !== 1) fail(`TUI build must have exactly one output; found ${outputs.length}`)
  const contributing = new Set(
    Object.entries(outputs[0]?.inputs ?? {})
      .filter(([, value]) => Number(value?.bytesInOutput) > 0)
      .map(([path]) => portable(path).replace(/^\.\//u, '')),
  )
  const runtimeInputs = new Set(
    Object.keys(metafile.inputs ?? {}).map(path => portable(path).replace(/^\.\//u, '')),
  )
  for (const path of contributing) {
    if (/^(?:apps|archive|contracts|docs|frontend)\//u.test(path)) {
      fail(`non-TUI input contributes executable bytes: ${path}`)
    }
    if (/^src\/(?:appServer|components|ink|screens)(?:\/|$)/u.test(path)) {
      fail(`GUI/AppServer input contributes executable bytes: ${path}`)
    }
    if (/node_modules\/(?:\.bun\/[^/]+\/node_modules\/)?(?:ink|react|react-dom|react-reconciler)(?:\/|$)/u.test(path)) {
      fail(`GUI dependency contributes executable bytes: ${path}`)
    }
  }

  const exactTypeOnlySources = new Set([
    'src/constants/querySource.ts',
    'src/entrypoints/sdk/controlTypes.ts',
    'src/entrypoints/sdk/sdkUtilityTypes.ts',
    'src/entrypoints/sdk/settingsTypes.generated.ts',
    'src/i18n/types.ts',
    'src/keybindings/types.ts',
    'src/query/transitions.ts',
    'src/services/api/types.ts',
    'src/services/lsp/types.ts',
    'src/services/mcp/agentServerInfo.ts',
    'src/services/search/types.ts',
    'src/services/skillSearch/signals.ts',
    'src/state/AppState.ts',
    'src/types/api-types.ts',
    'src/types/auth.ts',
    'src/types/bun-bundle.d.ts',
    'src/types/canUseTool.ts',
    'src/types/externalModules.d.ts',
    'src/types/fileSuggestion.ts',
    'src/types/ideSelection.ts',
    'src/types/image-codecs.d.ts',
    'src/types/macro.d.ts',
    'src/types/message.ts',
    'src/types/messageQueueTypes.ts',
    'src/types/notebook.ts',
    'src/types/notification.ts',
    'src/types/optional-packages.d.ts',
    'src/types/permissionDecisionSource.ts',
    'src/types/spinner.ts',
    'src/types/statusLine.ts',
    'src/types/terminalKey.ts',
    'src/types/terminalStyle.ts',
    'src/types/toolContracts.ts',
    'src/types/toolUseConfirm.ts',
    'src/types/tools.ts',
    'src/types/utils.ts',
    'src/utils/sandbox/types.ts',
    'src/utils/secureStorage/types.ts',
  ])
  const typeOnlySources = new Set(
    tracked.filter(
      path =>
        path.startsWith('src/') &&
        /\.[cm]?[jt]sx?$/u.test(path) &&
        !runtimeInputs.has(path),
    ),
  )
  compareSet('TypeScript files outside the executable TUI bundle', typeOnlySources, exactTypeOnlySources)

  const tsc = resolve(root, 'node_modules', '.bin', process.platform === 'win32' ? 'tsc.cmd' : 'tsc')
  if (!existsSync(tsc)) {
    fail('local TypeScript compiler is missing; run bun install')
  } else {
    const listed = new Set(
      run(tsc, ['-p', 'tsconfig.tui-runtime.json', '--listFilesOnly'])
        .split(/\r?\n/u)
        .map(path => path.trim())
        .filter(Boolean)
        .map(path => portable(relative(root, path)))
        .filter(path => !path.startsWith('../')),
    )
    const unreachable = tracked.filter(
      path =>
        path.startsWith('src/') &&
        /\.[cm]?[jt]sx?$/u.test(path) &&
        !listed.has(path) &&
        !contributing.has(path),
    )
    if (unreachable.length > 0) {
      fail(`tracked TypeScript outside the TUI compile graph: ${unreachable.join(', ')}`)
    }
  }
}

const license = readFileSync(resolve(root, 'LICENSE'), 'utf8')
if (!license.startsWith('MIT License\n')) fail('LICENSE is not the canonical MIT text')

if (failures.length > 0) {
  process.stderr.write(`repository boundary failed (${failures.length}):\n`)
  for (const message of failures) process.stderr.write(`  - ${message}\n`)
  process.exit(1)
}

process.stdout.write(
  `repository boundary passed: files=${tracked.length} bytes=${trackedBytes} pure_tui=true\n`,
)
