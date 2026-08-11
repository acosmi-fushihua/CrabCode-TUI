import { createHash } from 'node:crypto'
import {
  lstatSync,
  readFileSync,
  realpathSync,
  statSync,
} from 'node:fs'
import { isAbsolute, relative, resolve, sep } from 'node:path'

const SOURCE_BINDING_DOMAIN = 'crabcode-tui-source-graph-v1'
const ARTIFACT_BINDING_DOMAIN = 'crabcode-tui-artifact-v1'
const BUILD_BINDING_DOMAIN = 'crabcode-tui-build-v2'
const ACCOUNT_BRIDGE_CONFIGURATION_BINDING_DOMAIN =
  'crabcode-tui-account-bridge-public-configuration-v1'
const RUNTIME_ENTRY_POINT = 'src/entrypoints/tuiRuntime.ts'
const RUNTIME_OUTPUT = 'dist/tui-runtime/index.js'
const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const ACCOUNT_BRIDGE_CONFIGURATION_FIELDS = Object.freeze([
  'controlPlaneEndpoint',
  'eligibilityPublicKeyBase64url',
  'connectorPolicyPublicKeyBase64url',
  'artifactPublicKeyBase64url',
])

function portable(path) {
  return path.split(sep).join('/')
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function assertExactObject(value, keys, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${label} is not an object`)
  }
  const actual = Object.keys(value).sort()
  const expected = [...keys].sort()
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(
      `${label} fields are invalid: expected ${expected.join(',')}, got ${actual.join(',')}`,
    )
  }
}

function pathEscaped(root, candidate) {
  const fromRoot = relative(root, candidate)
  return (
    fromRoot === '..' ||
    fromRoot.startsWith(`..${sep}`) ||
    isAbsolute(fromRoot)
  )
}

function assertSafeInputPath(root, rawPath) {
  if (
    typeof rawPath !== 'string' ||
    rawPath.length === 0 ||
    rawPath.includes('\0') ||
    isAbsolute(rawPath)
  ) {
    throw new Error(`unsafe TUI runtime input path: ${String(rawPath)}`)
  }
  const absolute = resolve(root, rawPath)
  const lexicalRelative = relative(root, absolute)
  if (pathEscaped(root, absolute)) {
    throw new Error(`TUI runtime input escaped the repository: ${rawPath}`)
  }
  const info = lstatSync(absolute)
  if (!info.isFile() || info.isSymbolicLink()) {
    throw new Error(`TUI runtime input is not a regular file: ${rawPath}`)
  }
  const physical = realpathSync(absolute)
  if (pathEscaped(root, physical)) {
    throw new Error(
      `TUI runtime input escaped the repository through a symbolic-link directory: ${rawPath}`,
    )
  }
  return { absolute, path: portable(lexicalRelative) }
}

export function bindTuiRuntimeInputs(root, inputPaths) {
  const repositoryRoot = realpathSync(root)
  if (!Array.isArray(inputPaths) || inputPaths.length === 0) {
    throw new Error('TUI runtime source graph must contain at least one input')
  }
  const normalized = inputPaths.map(path =>
    assertSafeInputPath(repositoryRoot, path),
  )
  if (new Set(normalized.map(input => input.path)).size !== normalized.length) {
    throw new Error('TUI runtime source graph contains aliased duplicate paths')
  }
  const records = normalized
    .sort((left, right) =>
      left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
    )
    .map(({ absolute, path }) => {
      const bytes = readFileSync(absolute)
      return { path, size: bytes.length, sha256: sha256(bytes) }
    })
  const canonical = JSON.stringify({ domain: SOURCE_BINDING_DOMAIN, records })
  return {
    scheme: 'sha256',
    domain: SOURCE_BINDING_DOMAIN,
    inputCount: records.length,
    digest: sha256(canonical),
  }
}

export function bindTuiRuntimeArtifact(path) {
  const lexicalInfo = lstatSync(path)
  if (lexicalInfo.isSymbolicLink()) {
    throw new Error(`TUI runtime artifact must not be a symbolic link: ${path}`)
  }
  const info = statSync(path)
  if (!info.isFile() || info.size === 0) {
    throw new Error(`TUI runtime artifact is not a non-empty file: ${path}`)
  }
  const bytes = readFileSync(path)
  const contentSha256 = sha256(bytes)
  const canonical = JSON.stringify({
    domain: ARTIFACT_BINDING_DOMAIN,
    size: bytes.length,
    contentSha256,
  })
  return {
    scheme: 'sha256',
    domain: ARTIFACT_BINDING_DOMAIN,
    size: bytes.length,
    contentSha256,
    digest: sha256(canonical),
  }
}

function normalizeAccountBridgeTrustRoot(value, label, required) {
  if (typeof value !== 'string') {
    throw new Error(`${label} must be a string`)
  }
  const normalized = value.trim()
  if (normalized === '') {
    if (required) throw new Error(`release build requires ${label}`)
    return ''
  }
  if (!/^[A-Za-z0-9_-]{43}$/u.test(normalized)) {
    throw new Error(`${label} must be canonical 32-byte base64url`)
  }
  const decoded = Buffer.from(normalized, 'base64url')
  const canonical =
    decoded.length === 32 &&
    Buffer.from(decoded).toString('base64url') === normalized
  decoded.fill(0)
  if (!canonical) {
    throw new Error(`${label} must be canonical 32-byte base64url`)
  }
  return normalized
}

function normalizeAccountBridgeEndpoint(value, required) {
  const label = 'ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT'
  if (typeof value !== 'string') {
    throw new Error(`${label} must be a string`)
  }
  const normalized = value.trim()
  if (normalized === '') {
    if (required) throw new Error(`release build requires ${label}`)
    return ''
  }
  let parsed
  try {
    parsed = new URL(normalized)
  } catch {
    throw new Error(`${label} must be a canonical first-party HTTPS URL`)
  }
  const firstPartyHost =
    parsed.hostname === 'acosmi.com' || parsed.hostname.endsWith('.acosmi.com')
  if (
    parsed.protocol !== 'https:' ||
    !firstPartyHost ||
    parsed.username !== '' ||
    parsed.password !== '' ||
    parsed.search !== '' ||
    parsed.hash !== '' ||
    parsed.pathname === '/' ||
    parsed.toString() !== normalized
  ) {
    throw new Error(`${label} must be a canonical first-party HTTPS URL`)
  }
  return normalized
}

export function normalizeTuiRuntimeAccountBridgeConfiguration(
  environment,
  { required = false } = {},
) {
  if (!environment || typeof environment !== 'object') {
    throw new Error('Account Bridge build environment is invalid')
  }
  const configuration = {
    controlPlaneEndpoint: normalizeAccountBridgeEndpoint(
      environment.ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT ?? '',
      required,
    ),
    eligibilityPublicKeyBase64url: normalizeAccountBridgeTrustRoot(
      environment.ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL ?? '',
      'ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL',
      required,
    ),
    connectorPolicyPublicKeyBase64url: normalizeAccountBridgeTrustRoot(
      environment.ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL ?? '',
      'ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL',
      required,
    ),
    artifactPublicKeyBase64url: normalizeAccountBridgeTrustRoot(
      environment.ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL ?? '',
      'ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL',
      required,
    ),
  }
  const configuredTrustRoots = [
    configuration.eligibilityPublicKeyBase64url,
    configuration.connectorPolicyPublicKeyBase64url,
    configuration.artifactPublicKeyBase64url,
  ].filter(Boolean)
  if (new Set(configuredTrustRoots).size !== configuredTrustRoots.length) {
    throw new Error('Account Bridge public verification keys must be independent')
  }
  return configuration
}

export function bindTuiRuntimeAccountBridgeConfiguration(configuration) {
  assertExactObject(
    configuration,
    ACCOUNT_BRIDGE_CONFIGURATION_FIELDS,
    'Account Bridge public configuration',
  )
  for (const field of ACCOUNT_BRIDGE_CONFIGURATION_FIELDS) {
    if (typeof configuration[field] !== 'string') {
      throw new Error(`Account Bridge public configuration field ${field} is invalid`)
    }
  }
  const records = ACCOUNT_BRIDGE_CONFIGURATION_FIELDS.map(name => ({
    name,
    value: configuration[name],
  }))
  const canonical = JSON.stringify({
    domain: ACCOUNT_BRIDGE_CONFIGURATION_BINDING_DOMAIN,
    records,
  })
  return {
    scheme: 'sha256',
    domain: ACCOUNT_BRIDGE_CONFIGURATION_BINDING_DOMAIN,
    fieldCount: records.length,
    digest: sha256(canonical),
  }
}

export function createTuiRuntimeBuildConfiguration({
  profile,
  minify,
  nodeEnv,
  accountBridgeConfiguration,
}) {
  const configuration = {
    profile,
    minify,
    nodeEnv,
    accountBridgeBinding: bindTuiRuntimeAccountBridgeConfiguration(
      accountBridgeConfiguration,
    ),
  }
  assertBuildConfigurationShape(configuration)
  return configuration
}

export function bindTuiRuntimeBuild({
  schemaVersion,
  entryPoint,
  output,
  version,
  buildId,
  imageProcessorNapi,
  buildConfiguration,
  sourceBinding,
  artifactBinding,
}) {
  const canonical = JSON.stringify({
    domain: BUILD_BINDING_DOMAIN,
    schemaVersion,
    entryPoint,
    output,
    version,
    buildId,
    imageProcessorNapi,
    buildConfiguration,
    sourceBinding,
    artifactBinding,
  })
  return {
    scheme: 'sha256',
    domain: BUILD_BINDING_DOMAIN,
    digest: sha256(canonical),
  }
}

function exactBinding(actual, expected, label) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(
      `${label} is stale: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
    )
  }
}

function assertSourceBindingShape(binding) {
  assertExactObject(
    binding,
    ['scheme', 'domain', 'inputCount', 'digest'],
    'TUI runtime source binding',
  )
  if (
    binding.scheme !== 'sha256' ||
    binding.domain !== SOURCE_BINDING_DOMAIN ||
    !Number.isSafeInteger(binding.inputCount) ||
    binding.inputCount <= 0 ||
    !SHA256_PATTERN.test(binding.digest)
  ) {
    throw new Error('TUI runtime source binding metadata is invalid')
  }
}

function assertArtifactBindingShape(binding) {
  assertExactObject(
    binding,
    ['scheme', 'domain', 'size', 'contentSha256', 'digest'],
    'TUI runtime artifact binding',
  )
  if (
    binding.scheme !== 'sha256' ||
    binding.domain !== ARTIFACT_BINDING_DOMAIN ||
    !Number.isSafeInteger(binding.size) ||
    binding.size <= 0 ||
    !SHA256_PATTERN.test(binding.contentSha256) ||
    !SHA256_PATTERN.test(binding.digest)
  ) {
    throw new Error('TUI runtime artifact binding metadata is invalid')
  }
}

function assertAccountBridgeBindingShape(binding) {
  assertExactObject(
    binding,
    ['scheme', 'domain', 'fieldCount', 'digest'],
    'Account Bridge public configuration binding',
  )
  if (
    binding.scheme !== 'sha256' ||
    binding.domain !== ACCOUNT_BRIDGE_CONFIGURATION_BINDING_DOMAIN ||
    binding.fieldCount !== ACCOUNT_BRIDGE_CONFIGURATION_FIELDS.length ||
    !SHA256_PATTERN.test(binding.digest)
  ) {
    throw new Error('Account Bridge public configuration binding is invalid')
  }
}

function assertBuildConfigurationShape(configuration) {
  assertExactObject(
    configuration,
    ['profile', 'minify', 'nodeEnv', 'accountBridgeBinding'],
    'TUI runtime build configuration',
  )
  if (
    !['development', 'release'].includes(configuration.profile) ||
    typeof configuration.minify !== 'boolean' ||
    !['development', 'production'].includes(configuration.nodeEnv)
  ) {
    throw new Error('TUI runtime build configuration is invalid')
  }
  assertAccountBridgeBindingShape(configuration.accountBridgeBinding)
}

export function verifyTuiRuntimeBuildBinding(metafile, expectedIdentity = {}) {
  const build = metafile?.crabcodeTuiBuild
  assertExactObject(
    build,
    [
      'schemaVersion',
      'entryPoint',
      'output',
      'version',
      'buildId',
      'imageProcessorNapi',
      'buildConfiguration',
      'sourceBinding',
      'artifactBinding',
      'buildBinding',
    ],
    'TUI runtime build metadata',
  )
  if (
    build.schemaVersion !== 3 ||
    build.entryPoint !== RUNTIME_ENTRY_POINT ||
    build.output !== RUNTIME_OUTPUT ||
    typeof build.version !== 'string' ||
    !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u.test(build.version) ||
    typeof build.buildId !== 'string' ||
    build.buildId.length === 0 ||
    build.buildId.length > 256 ||
    /[\u0000-\u001f\u007f]/u.test(build.buildId) ||
    build.imageProcessorNapi !== false
  ) {
    throw new Error('TUI runtime build identity is invalid')
  }
  for (const [key, value] of Object.entries(expectedIdentity)) {
    if (!['entryPoint', 'output', 'version', 'buildId'].includes(key)) {
      throw new Error(`unknown expected TUI runtime identity field: ${key}`)
    }
    if (build[key] !== value) {
      throw new Error(
        `TUI runtime build identity mismatch for ${key}: expected ${JSON.stringify(value)}, got ${JSON.stringify(build[key])}`,
      )
    }
  }
  assertSourceBindingShape(build.sourceBinding)
  assertArtifactBindingShape(build.artifactBinding)
  assertBuildConfigurationShape(build.buildConfiguration)
  const expectedBuildBinding = bindTuiRuntimeBuild(build)
  exactBinding(
    expectedBuildBinding,
    build.buildBinding,
    'TUI runtime build binding',
  )
  return build
}

export function verifyTuiRuntimeReleaseBuildBinding(
  metafile,
  environment = process.env,
  expectedIdentity = {},
  expectedReleaseConfiguration = {},
) {
  const build = verifyTuiRuntimeBuildBinding(metafile, expectedIdentity)
  if (
    build.buildConfiguration.profile !== 'release' ||
    build.buildConfiguration.minify !== true ||
    build.buildConfiguration.nodeEnv !== 'production'
  ) {
    throw new Error(
      'TUI runtime artifact was not built with the required release profile',
    )
  }
  assertExactObject(
    expectedReleaseConfiguration,
    Object.hasOwn(
      expectedReleaseConfiguration,
      'accountBridgeArtifactPublicKeyBase64url',
    )
      ? ['accountBridgeArtifactPublicKeyBase64url']
      : [],
    'expected TUI runtime release configuration',
  )
  const normalizedAccountBridgeConfiguration =
    normalizeTuiRuntimeAccountBridgeConfiguration(environment, {
      required: true,
    })
  if (
    Object.hasOwn(
      expectedReleaseConfiguration,
      'accountBridgeArtifactPublicKeyBase64url',
    ) &&
    normalizedAccountBridgeConfiguration.artifactPublicKeyBase64url !==
      expectedReleaseConfiguration.accountBridgeArtifactPublicKeyBase64url
  ) {
    throw new Error(
      'TUI runtime Account Bridge artifact trust root does not match the packaged component provenance contract',
    )
  }
  const expectedAccountBridgeBinding =
    bindTuiRuntimeAccountBridgeConfiguration(
      normalizedAccountBridgeConfiguration,
    )
  exactBinding(
    expectedAccountBridgeBinding,
    build.buildConfiguration.accountBridgeBinding,
    'TUI runtime Account Bridge public configuration binding',
  )
  return build
}

export function verifyTuiRuntimeSourceBinding(root, metafile) {
  const build = verifyTuiRuntimeBuildBinding(metafile)
  if (!metafile.inputs || typeof metafile.inputs !== 'object') {
    throw new Error('TUI runtime metafile has no input graph')
  }
  if (!Object.hasOwn(metafile.inputs, build.entryPoint)) {
    throw new Error('TUI runtime source graph does not contain its entry point')
  }
  const outputs = Object.values(metafile.outputs ?? {})
  if (
    outputs.length !== 1 ||
    !outputs[0] ||
    typeof outputs[0] !== 'object' ||
    outputs[0].entryPoint !== build.entryPoint
  ) {
    throw new Error(
      'TUI runtime output graph is not the single bound entry-point output',
    )
  }
  const current = bindTuiRuntimeInputs(root, Object.keys(metafile.inputs))
  if (current.inputCount !== Object.keys(metafile.inputs).length) {
    throw new Error('TUI runtime source graph input count is ambiguous')
  }
  exactBinding(current, build.sourceBinding, 'TUI runtime source binding')
  return current
}

export function verifyTuiRuntimeArtifactBinding(path, metafile) {
  const build = verifyTuiRuntimeBuildBinding(metafile)
  const current = bindTuiRuntimeArtifact(path)
  exactBinding(
    current,
    build.artifactBinding,
    'TUI runtime artifact binding',
  )
  return current
}
