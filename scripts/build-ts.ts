#!/usr/bin/env bun
/**
 * 原生 TUI 直连后端运行时构建脚本
 *
 * 注入 MACRO.* 编译时常量并调用 bun build。
 * 用法:  bun scripts/build-ts.ts
 */

import { execSync } from 'child_process'
import { readFileSync, rmSync } from 'fs'
import { join, resolve } from 'path'
import { valid as validSemver } from 'semver'
import {
  bindTuiRuntimeArtifact,
  bindTuiRuntimeBuild,
  bindTuiRuntimeInputs,
  createTuiRuntimeBuildConfiguration,
} from './tui-runtime-source-binding.mjs'

const pkg = JSON.parse(readFileSync(join(import.meta.dir, '..', 'package.json'), 'utf8'))
const version: string = pkg.version ?? '0.1.0'
if (
  typeof version !== 'string' ||
  !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version) ||
  validSemver(version) !== version
) {
  throw new Error('package.json version is invalid')
}

// ── BUILD_TIME 确定性解析（可复现构建）─────────────────────────
// SOURCE_DATE_EPOCH（Reproducible Builds 标准 env，Unix 秒）优先；
// 否则使用当前 commit 的 committer date（同一 commit 恒定、归一化 UTC）；
// git 不可用且无 env 时才回退 wall-clock（本地无仓构建不要求可复现）。
function resolveBuildTime(): string {
  const epoch = process.env.SOURCE_DATE_EPOCH?.trim()
  if (epoch && /^\d+$/.test(epoch)) {
    return new Date(Number(epoch) * 1000).toISOString()
  }
  try {
    const iso = execSync('git log -1 --format=%cI', {
      cwd: join(import.meta.dir, '..'),
      stdio: ['ignore', 'pipe', 'ignore'],
    }).toString().trim()
    const d = new Date(iso)
    if (iso && !Number.isNaN(d.getTime())) return d.toISOString()
  } catch {
    // git 不可用 → wall-clock
  }
  return new Date().toISOString()
}
const now = resolveBuildTime()

// ── F1 daemon/产物代际握手 (2026-06-11)：BUILD_ID 计算 ────────
// 契约（与 Rust `crates/acosmi-daemon-launcher/build.rs` / `scripts/release.sh`
// 逐字对齐）：
// env `CRABCODE_BUILD_ID` 已设 → 逐字使用（release 链权威路径）；否则
// `${version}+$(git rev-parse --short=12 HEAD)`；git 不可用（release tarball
// 内构建）→ `${version}+unknown`（非权威 id，运行时比对永远视为一致）。
// 不带 dirty 后缀。
function resolveBuildId(pkgVersion: string): string {
  const explicit = process.env.CRABCODE_BUILD_ID
  if (explicit && explicit.trim().length > 0) return explicit
  try {
    const sha = execSync('git rev-parse --short=12 HEAD', {
      cwd: join(import.meta.dir, '..'),
      stdio: ['ignore', 'pipe', 'ignore'],
    }).toString().trim()
    if (/^[0-9a-f]{12,40}$/.test(sha)) return `${pkgVersion}+${sha}`
  } catch {
    // git 不可用 → 落 +unknown
  }
  return `${pkgVersion}+unknown`
}
const buildId = resolveBuildId(version)

const accountBridgeLock = JSON.parse(
  readFileSync(
    join(
      import.meta.dir,
      '..',
      'components',
      'oauthapi-llm',
      'UPSTREAM.lock',
    ),
    'utf8',
  ),
) as Record<string, unknown>
const accountBridgeComponentVersion = accountBridgeLock.componentVersion
const accountBridgeProtocolVersion = accountBridgeLock.protocolVersion
if (
  typeof accountBridgeComponentVersion !== 'string' ||
  !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(
    accountBridgeComponentVersion,
  ) ||
  validSemver(accountBridgeComponentVersion) !==
    accountBridgeComponentVersion ||
  !Number.isSafeInteger(accountBridgeProtocolVersion) ||
  Number(accountBridgeProtocolVersion) <= 0
) {
  throw new Error(
    'components/oauthapi-llm/UPSTREAM.lock has invalid component/protocol version',
  )
}

// The direct QueryEngine bundle now owns the same transport-neutral manager as
// the worker bundle. Compile in only public verification/configuration values;
// runtime management/inference/master keys are generated after launch and can
// never enter this define map.
function accountBridgeTrustRoot(name: string): string {
  const value = process.env[name]?.trim() ?? ''
  if (value === '') {
    if (process.env.CRABCODE_RELEASE_BUILD === '1') {
      throw new Error(`release root build requires ${name}`)
    }
    return ''
  }
  if (!/^[A-Za-z0-9_-]{43}$/.test(value)) {
    throw new Error(`${name} must be canonical 32-byte base64url`)
  }
  const decoded = Buffer.from(value, 'base64url')
  const canonical =
    decoded.length === 32 && Buffer.from(decoded).toString('base64url') === value
  decoded.fill(0)
  if (!canonical) {
    throw new Error(`${name} must be canonical 32-byte base64url`)
  }
  return value
}

function accountBridgeControlPlaneEndpoint(): string {
  const value = process.env.ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT?.trim() ?? ''
  if (value === '') {
    if (process.env.CRABCODE_RELEASE_BUILD === '1') {
      throw new Error(
        'release root build requires ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT',
      )
    }
    return ''
  }
  let parsed: URL
  try {
    parsed = new URL(value)
  } catch {
    throw new Error(
      'ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT must be an exact HTTPS URL',
    )
  }
  const firstPartyHost =
    parsed.hostname === 'acosmi.com' ||
    parsed.hostname.endsWith('.acosmi.com')
  if (
    parsed.protocol !== 'https:' ||
    !firstPartyHost ||
    parsed.username !== '' ||
    parsed.password !== '' ||
    parsed.search !== '' ||
    parsed.hash !== '' ||
    parsed.pathname === '/' ||
    parsed.toString() !== value
  ) {
    throw new Error(
      'ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT must be a canonical first-party Acosmi HTTPS endpoint without credentials/query/fragment',
    )
  }
  return value
}

const eligibilityTrustRoot = accountBridgeTrustRoot(
  'ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL',
)
const connectorPolicyTrustRoot = accountBridgeTrustRoot(
  'ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL',
)
const artifactTrustRoot = accountBridgeTrustRoot(
  'ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL',
)
const controlPlaneEndpoint = accountBridgeControlPlaneEndpoint()
const configuredTrustRoots = [
  eligibilityTrustRoot,
  connectorPolicyTrustRoot,
  artifactTrustRoot,
].filter(Boolean)
if (new Set(configuredTrustRoots).size !== configuredTrustRoots.length) {
  throw new Error('Account Bridge release trust roots must be independent keys')
}

if (process.env.CRABCODE_RELEASE_BUILD !== '1') {
  const missingAccountBridgeEnvs = [
    ['ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT', controlPlaneEndpoint],
    ['ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL', eligibilityTrustRoot],
    [
      'ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL',
      connectorPolicyTrustRoot,
    ],
    ['ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL', artifactTrustRoot],
  ].filter(([, value]) => value === '')
  if (missingAccountBridgeEnvs.length > 0) {
    console.warn(
      [
        '⚠️  [build-ts] Account Bridge 注入缺失，直连 TUI 的可选 Account Bridge 连接器入口不可用：',
        ...missingAccountBridgeEnvs.map(
          ([name]) => `    - ${name} 未设置`,
        ),
        '    运行时表现 = 仅 Account Bridge 入口明确报「此构建未配置地区检测端点（开发构建）」。',
        '    Acosmi SDK 直连 OAuth 不读取上述变量，不受此开发构建提示影响。',
        '    本地验证 Account Bridge 入口需注入上述 4 个非密配置值。',
      ].join('\n'),
    )
  }
}

// ── MACRO 编译时常量 ──────────────────────────────────────
const define: Record<string, string> = {
  'MACRO.VERSION':           JSON.stringify(version),
  'MACRO.BUILD_ID':          JSON.stringify(buildId),
  'MACRO.PACKAGE_URL':       JSON.stringify('crabcode'),
  'MACRO.BUILD_TIME':        JSON.stringify(now),
  'MACRO.ISSUES_EXPLAINER':  JSON.stringify('report the issue at https://github.com/acosmi/CrabCode-TUI/issues'),
  'MACRO.CHANNEL':           JSON.stringify('external'),
  'MACRO.BUILD_ENV':         JSON.stringify('production'),
  'MACRO.FEEDBACK_CHANNEL':  JSON.stringify('https://github.com/acosmi/CrabCode-TUI/issues'),
  'MACRO.NATIVE_PACKAGE_URL': 'undefined',
  'MACRO.VERSION_CHANGELOG':  'undefined',
  ACCOUNT_BRIDGE_COMPONENT_VERSION: JSON.stringify(
    accountBridgeComponentVersion,
  ),
  ACCOUNT_BRIDGE_PROTOCOL_VERSION: JSON.stringify(
    accountBridgeProtocolVersion,
  ),
  ACCOUNT_BRIDGE_CRABCODE_RELEASE_VERSION: JSON.stringify(version),
  ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL:
    JSON.stringify(eligibilityTrustRoot),
  ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL: JSON.stringify(
    connectorPolicyTrustRoot,
  ),
  ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL:
    JSON.stringify(artifactTrustRoot),
  ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT: JSON.stringify(controlPlaneEndpoint),
  // The direct runtime is executed by its packaged Bun and cannot satisfy
  // the compiled-single-file NAPI lookup contract.
  CRABCODE_COMPILED_IMAGE_PROCESSOR_NAPI: 'false',
}

// ── NODE_ENV 内联（2026-06-11 真机实证，三 build 脚本同步持有此块）──
// Bun.build 会把 `process.env.NODE_ENV` 点访问在构建期内联（构建机未设 →
// "development"），常量折叠 + 死代码消除会把安装类型焊死为开发构建，
// 导致公开分发包拒绝更新。
// 修：按 minify（= release 链信号，S-7）确定性 define —— release 出
// production、本地 dev 构建保持 development（repo dist 判 development 语义
// 本就正确）。**不读构建机 NODE_ENV**：pretest/测试 harness 若导出
// NODE_ENV=test 会把 `=== 'test'` 分支（TestingPermissionTool 等）焊进 dist。
const NODE_ENV_INLINE =
  process.env.CRABCODE_BUILD_MINIFY === '1' ? 'production' : 'development'
const BUILD_PROFILE =
  process.env.CRABCODE_RELEASE_BUILD === '1' ? 'release' : 'development'
define['process.env.NODE_ENV'] = JSON.stringify(NODE_ENV_INLINE)

// The dedicated TUI package must own every reachable JavaScript dependency.
// Only native modules that have no distributable implementation remain
// external. Sharp and the real MCPB/OTel/highlight implementations are
// bundled, with Sharp's native @img payload tracked as a platform resource by
// the package closure.
const tuiRuntimeExternals: string[] = [
  '@acosmi-ai/sandbox-runtime',
  'color-diff-napi',
  'modifiers-napi',
  'audio-capture-napi',
  'image-processor-napi',
]

const projectRoot = resolve(import.meta.dir, '..')

// ── 执行构建 ──────────────────────────────────────────────
// 发布链设 CRABCODE_BUILD_MINIFY=1；本地开发构建默认保留可读栈帧。
// 只开 whitespace+syntax，不开 identifiers：后端仍有按 error.name 等
// 标识工作的路径，改名风险大于收益。
const minify = process.env.CRABCODE_BUILD_MINIFY === '1'

// Reusing a dirty developer workspace must not accidentally retain artifacts
// from the retired alternate hosts.
for (const retiredOutput of [
  'dist/index.js',
  'dist/cli.js',
  'dist/worker',
  'dist/bridge-host',
  'dist/extension',
]) {
  rmSync(retiredOutput, { recursive: true, force: true })
}

// Process-owned StructuredIO runtime for the native CrabCode TUI. Keep the
// metafile beside the artifact so release verification can prove the graph
// excludes AppServer, GUI, React and Ink surfaces.
const tuiRuntimeResult = await Bun.build({
  entrypoints: ['src/entrypoints/tuiRuntime.ts'],
  outdir: 'dist/tui-runtime',
  target: 'bun',
  naming: 'index.js',
  define,
  external: tuiRuntimeExternals,
  metafile: true,
  minify: minify ? { whitespace: true, syntax: true, identifiers: false } : false,
})

if (!tuiRuntimeResult.success) {
  console.error('Native TUI runtime build failed:')
  for (const log of tuiRuntimeResult.logs) {
    console.error(log)
  }
  process.exit(1)
}

const sourceBinding = bindTuiRuntimeInputs(
  projectRoot,
  Object.keys(tuiRuntimeResult.metafile.inputs),
)
const artifactBinding = bindTuiRuntimeArtifact(
  resolve(projectRoot, 'dist/tui-runtime/index.js'),
)
const runtimeBuildIdentity = {
  entryPoint: 'src/entrypoints/tuiRuntime.ts',
  output: 'dist/tui-runtime/index.js',
  version,
  buildId,
}
const buildConfiguration = createTuiRuntimeBuildConfiguration({
  profile: BUILD_PROFILE,
  minify,
  nodeEnv: NODE_ENV_INLINE,
  accountBridgeConfiguration: {
    controlPlaneEndpoint,
    eligibilityPublicKeyBase64url: eligibilityTrustRoot,
    connectorPolicyPublicKeyBase64url: connectorPolicyTrustRoot,
    artifactPublicKeyBase64url: artifactTrustRoot,
  },
})
const boundBuild = {
  schemaVersion: 3,
  ...runtimeBuildIdentity,
  imageProcessorNapi: false,
  buildConfiguration,
  sourceBinding,
  artifactBinding,
}
const crabcodeTuiBuild = {
  ...boundBuild,
  buildBinding: bindTuiRuntimeBuild(boundBuild),
}
const boundTuiRuntimeMetafile = {
  ...tuiRuntimeResult.metafile,
  crabcodeTuiBuild,
}
await Bun.write(
  'dist/tui-runtime/metafile.json',
  `${JSON.stringify(boundTuiRuntimeMetafile, null, 2)}\n`,
)
console.log(
  `Built dist/tui-runtime/index.js + metafile.json (${tuiRuntimeResult.outputs.length} output(s), version ${version}, build-id ${buildId})`,
)
