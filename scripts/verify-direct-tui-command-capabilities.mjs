#!/usr/bin/env bun

import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { isAbsolute, relative, resolve, sep } from 'node:path'

const root = resolve(import.meta.dir, '..')
const contractPath = resolve(
  root,
  'contracts/direct-tui-command-capabilities/v1/command-capabilities.json',
)
const rustPath = resolve(root, 'crates/crabcode-tui/src/tui_app.rs')
const contract = JSON.parse(readFileSync(contractPath, 'utf8'))
const rust = readFileSync(rustPath, 'utf8')

function fail(message) {
  throw new Error(`direct TUI command capability contract: ${message}`)
}

function sortedUnique(label, values) {
  if (!Array.isArray(values)) fail(`${label} must be an array`)
  if (
    values.some(
      value =>
        typeof value !== 'string' ||
        value.length === 0 ||
        value.startsWith('/') ||
        /\s/u.test(value),
    )
  ) {
    fail(`${label} contains an invalid invocation token`)
  }
  const sorted = [...values].sort()
  if (new Set(values).size !== values.length) {
    fail(`${label} contains duplicate invocation tokens`)
  }
  if (JSON.stringify(sorted) !== JSON.stringify(values)) {
    fail(`${label} must remain byte-stably sorted`)
  }
  return values
}

function assertSameSet(label, actualValues, expectedValues) {
  const actual = [...new Set(actualValues)].sort()
  const expected = [...new Set(expectedValues)].sort()
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    const actualSet = new Set(actual)
    const expectedSet = new Set(expected)
    fail(
      `${label} drifted; missing=${JSON.stringify(expected.filter(value => !actualSet.has(value)))} ` +
        `unexpected=${JSON.stringify(actual.filter(value => !expectedSet.has(value)))}`,
    )
  }
}

function sha256(value) {
  return createHash('sha256').update(value).digest('hex')
}

function assertExactKeys(label, value, expectedKeys) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    fail(`${label} must be an object`)
  }
  assertSameSet(`${label} keys`, Object.keys(value), expectedKeys)
}

function rustArrayBody(name) {
  const match = rust.match(
    new RegExp(`const ${name}[^=]*=\\s*&\\[(.*?)\\n\\];`, 's'),
  )
  if (!match) fail(`could not locate Rust array ${name}`)
  return match[1]
}

function rustStringArray(name) {
  return [...rustArrayBody(name).matchAll(/"([^"]+)"/g)].map(
    match => match[1],
  )
}

function rustFixedCompletionNames() {
  return [
    ...rustArrayBody('FIXED_LOCAL_COMMAND_COMPLETIONS').matchAll(
      /\(\s*"([^"]+)"\s*,\s*"[^"]*"\s*,/g,
    ),
  ].map(match => match[1])
}

if (contract.schemaVersion !== 1) fail('unsupported schemaVersion')

const reference = sortedUnique(
  'reference.invocationTokens',
  contract.reference?.invocationTokens,
)
const rendererLocal = sortedUnique(
  'owners.rendererLocal.invocationTokens',
  contract.owners?.rendererLocal?.invocationTokens,
)
const runtimeCatalog = sortedUnique(
  'owners.runtimeCatalog.invocationTokens',
  contract.owners?.runtimeCatalog?.invocationTokens,
)
const rendererExtensions = sortedUnique(
  'nonReferenceKnownTokens.rendererLocal',
  contract.nonReferenceKnownTokens?.rendererLocal,
)
const runtimeExtensions = sortedUnique(
  'nonReferenceKnownTokens.runtimeCatalog',
  contract.nonReferenceKnownTokens?.runtimeCatalog,
)
const failClosedExtensions = sortedUnique(
  'nonReferenceKnownTokens.failClosed',
  contract.nonReferenceKnownTokens?.failClosed,
)

const failClosedGroups = contract.failClosedGroups
if (!Array.isArray(failClosedGroups) || failClosedGroups.length === 0) {
  fail('failClosedGroups must be a non-empty array')
}
const groupIds = new Set()
const failClosedOnly = []
for (const [index, group] of failClosedGroups.entries()) {
  if (
    typeof group?.id !== 'string' ||
    group.id.length === 0 ||
    groupIds.has(group.id) ||
    typeof group.reason !== 'string' ||
    group.reason.length < 20
  ) {
    fail(`failClosedGroups[${index}] has an invalid id or reason`)
  }
  groupIds.add(group.id)
  failClosedOnly.push(
    ...sortedUnique(
      `failClosedGroups[${index}].invocationTokens`,
      group.invocationTokens,
    ),
  )
}

assertSameSet(
  'reference owner partition',
  [...rendererLocal, ...runtimeCatalog, ...failClosedOnly],
  reference,
)
if (
  rendererLocal.length !== contract.invariants.rendererLocalReferenceCount ||
  runtimeCatalog.length !== contract.invariants.runtimeCatalogReferenceCount ||
  failClosedOnly.length !== contract.invariants.failClosedOnlyReferenceCount ||
  reference.length !== contract.invariants.referenceInvocationCount
) {
  fail('declared command counts do not match their reviewed token arrays')
}
if (
  contract.invariants.runtimeCatalogMissingBehavior !==
  'fail_closed_without_backend_send'
) {
  fail('runtime-catalog absence must remain explicitly fail-closed')
}

// Force the reviewed default external feature profile before importing the
// TypeScript command registry. A developer's ~/.crabcode/features.json must
// never change this release contract.
process.env.CRABCODE_FEATURE_PROACTIVE = '0'
process.env.CRABCODE_FEATURE_KAIROS = '0'
const { getDirectTuiBuiltInCommandNames } = await import(
  '../src/cli/headlessCommands.ts'
)
assertSameSet(
  'TypeScript direct runtime registry',
  [...getDirectTuiBuiltInCommandNames()],
  [...runtimeCatalog, ...runtimeExtensions],
)

assertSameSet(
  'Rust fixed local command registry',
  rustFixedCompletionNames(),
  [...rendererLocal, ...rendererExtensions],
)

// Every reference token not owned locally must be rejected locally whenever
// the runtime does not advertise it. This prevents feature/auth/catalog drift
// from degrading into a backend "unknown skill" send.
assertSameSet(
  'Rust known-unavailable fallback registry',
  rustStringArray('UNAVAILABLE_LOCAL_COMMAND_TOKENS'),
  [
    ...runtimeCatalog,
    ...runtimeExtensions,
    ...failClosedOnly,
    ...failClosedExtensions,
  ],
)

if (
  sha256(JSON.stringify(reference)) !==
  contract.reference.invocationTokensSha256
) {
  fail('reference invocation-token snapshot hash drifted')
}

function verifyAvailableReferenceRepository() {
  const requireLiveReference =
    process.env.CRABCODE_COMMAND_REFERENCE_REQUIRED === '1'
  const testReferenceRepository =
    process.env.NODE_ENV === 'test'
      ? process.env.CRABCODE_COMMAND_REFERENCE_TEST_REPOSITORY
      : undefined
  // Production callers cannot redirect trust to an arbitrary repository;
  // the override only lets hermetic tests exercise optional/required modes.
  const referenceRepository = testReferenceRepository
    ? resolve(testReferenceRepository)
    : resolve(root, contract.reference.repository)
  if (!existsSync(referenceRepository)) {
    if (requireLiveReference) {
      fail(`required reference repository is absent: ${referenceRepository}`)
    }
    return 'snapshot_only'
  }

  const [sourcePath, exportName] = contract.reference.source.split('::')
  if (!sourcePath || !exportName) {
    fail('reference.source must use path::exportName syntax')
  }

  // The reference repository is optional for ordinary local/package checks.
  // When it happens to contain the pinned object, validate the immutable Git
  // blob instead of coupling this repository to the neighbour's current HEAD
  // or dirty worktree. Dynamic execution remains an explicit live-audit mode.
  const pinnedSource = spawnSync(
    'git',
    ['show', `${contract.reference.revision}:${sourcePath}`],
    {
      cwd: referenceRepository,
      encoding: 'utf8',
    },
  )
  if (pinnedSource.status !== 0) {
    if (requireLiveReference) {
      fail(
        `required pinned reference source is unavailable: ${pinnedSource.stderr.trim()}`,
      )
    }
    return 'snapshot_only'
  }
  if (sha256(pinnedSource.stdout) !== contract.reference.sourceSha256) {
    fail('reference command source hash drifted at the pinned revision')
  }
  if (!requireLiveReference) return 'snapshot_only'

  const revision = spawnSync('git', ['rev-parse', 'HEAD'], {
    cwd: referenceRepository,
    encoding: 'utf8',
  })
  if (revision.status !== 0) {
    fail(`could not read reference revision: ${revision.stderr.trim()}`)
  }
  if (revision.stdout.trim() !== contract.reference.revision) {
    fail(
      `reference revision drifted; expected=${contract.reference.revision} actual=${revision.stdout.trim()}`,
    )
  }

  const worktreeStatus = spawnSync('git', ['status', '--porcelain'], {
    cwd: referenceRepository,
    encoding: 'utf8',
  })
  if (worktreeStatus.status !== 0) {
    fail(`could not inspect reference worktree: ${worktreeStatus.stderr.trim()}`)
  }
  if (worktreeStatus.stdout.trim() !== '') {
    fail('required reference repository has worktree changes')
  }

  const absoluteSourcePath = resolve(referenceRepository, sourcePath)
  if (!existsSync(absoluteSourcePath)) {
    fail(`reference source is absent: ${absoluteSourcePath}`)
  }
  if (
    sha256(readFileSync(absoluteSourcePath)) !== contract.reference.sourceSha256
  ) {
    fail('reference command source hash drifted at the pinned revision')
  }

  const probe = [
    "globalThis.MACRO = new Proxy({ CHANNEL: 'external', VERSION: 'audit', BUILD_TIME: 'audit' }, { get: (target, property) => target[property] ?? false })",
    `import('./${sourcePath}').then(module => process.stdout.write(JSON.stringify([...module.${exportName}()].sort())))`,
  ].join('; ')
  const extracted = spawnSync(process.execPath, ['-e', probe], {
    cwd: referenceRepository,
    encoding: 'utf8',
    timeout: 120_000,
    env: {
      PATH: process.env.PATH,
      TMPDIR: process.env.TMPDIR,
      NODE_ENV: 'test',
      USER_TYPE: 'external',
      ACOSMI_API_KEY: 'fixture',
      CRABCODE_FEATURE_PROACTIVE: '0',
      CRABCODE_FEATURE_KAIROS: '0',
      CRABCODE_FEATURE_KAIROS_BRIEF: '0',
      CRABCODE_FEATURE_BRIDGE_MODE: '1',
      CRABCODE_FEATURE_DAEMON: '1',
      CRABCODE_FEATURE_VOICE_MODE: '0',
      CRABCODE_FEATURE_HISTORY_SNIP: '0',
      CRABCODE_FEATURE_FORK_SUBAGENT: '0',
      CRABCODE_FEATURE_CCR_REMOTE_SETUP: '0',
      CRABCODE_FEATURE_EXPERIMENTAL_SKILL_SEARCH: '0',
      CRABCODE_FEATURE_KAIROS_GITHUB_WEBHOOKS: '0',
      CRABCODE_FEATURE_ULTRAPLAN: '0',
      CRABCODE_FEATURE_WORKFLOW_SCRIPTS: '0',
      CRABCODE_FEATURE_TORCH: '0',
      CRABCODE_FEATURE_UDS_INBOX: '0',
      CRABCODE_FEATURE_BUDDY: '0',
      CRABCODE_FEATURE_MCP_SKILLS: '0',
    },
  })
  if (extracted.error) {
    fail(`reference command extraction could not complete: ${extracted.error.message}`)
  }
  if (extracted.status !== 0) {
    fail(`reference command extraction failed: ${extracted.stderr.trim()}`)
  }
  let extractedTokens
  try {
    extractedTokens = JSON.parse(extracted.stdout.trim())
  } catch (error) {
    fail(`reference command extraction was not JSON: ${error.message}`)
  }
  assertSameSet('live pinned reference command registry', extractedTokens, reference)
  return 'live_verified'
}

const referenceStatus = verifyAvailableReferenceRepository()

const audit = contract.lifecycleAudit
if (audit?.schemaVersion !== 1) {
  fail('unsupported lifecycleAudit.schemaVersion')
}
const expectedDimensions = [
  'ownership',
  'discovery',
  'parsingDispatch',
  'success',
  'failure',
  'cancellation',
  'terminalState',
  'rendering',
  'persistenceRecovery',
]
if (JSON.stringify(audit.dimensions) !== JSON.stringify(expectedDimensions)) {
  fail('lifecycle dimensions or their stable order drifted')
}
const allowedStatuses = Object.keys(audit.statusDefinitions ?? {})
assertSameSet('lifecycle status definitions', allowedStatuses, [
  'verified',
  'shared_path_only',
  'not_applicable',
  'unverified',
])

const expectedLifecycleScope = {
  ownerBoundary:
    'Lifecycle statuses cover the direct TUI owner boundary: token discovery, exact dispatch, correlated completion truth, renderer input, and TUI transcript recovery.',
  externalAuthorityBoundary:
    'Network services, operating-system integrations, and command-specific business mutations behind an admitted handler are separate authorities; this audit neither invokes them nor labels their internal effects as renderer success.',
  hermeticRuntimeSeam:
    'Production smoke loads every real command and executes each actual handler with mocks only at external authority seams. The separate runtime matrix verifies the shared slash-dispatch, terminal projection, renderer-input, and physical transcript machinery without pretending its injected handler is command-specific business evidence.',
}
assertExactKeys(
  'lifecycleAudit.scope',
  audit.scope,
  Object.keys(expectedLifecycleScope),
)
if (JSON.stringify(audit.scope) !== JSON.stringify(expectedLifecycleScope)) {
  fail('lifecycle owner and external-authority boundary drifted')
}

const evidence = audit.evidence
if (evidence === null || typeof evidence !== 'object' || Array.isArray(evidence)) {
  fail('lifecycleAudit.evidence must be an object')
}
for (const [id, item] of Object.entries(evidence)) {
  assertExactKeys(
    `lifecycle evidence ${id}`,
    item,
    item.kind === 'bun_test'
      ? ['kind', 'path', 'markers', 'testNames']
      : ['kind', 'path', 'markers'],
  )
  if (!['rust_test', 'bun_test'].includes(item.kind)) {
    fail(`lifecycle evidence ${id} has unsupported kind`)
  }
  if (typeof item.path !== 'string' || item.path.length === 0) {
    fail(`lifecycle evidence ${id} has an unsafe path`)
  }
  const evidencePath = resolve(root, item.path)
  const relativeEvidencePath = relative(root, evidencePath)
  if (
    relativeEvidencePath === '' ||
    relativeEvidencePath === '..' ||
    relativeEvidencePath.startsWith(`..${sep}`) ||
    isAbsolute(relativeEvidencePath)
  ) {
    fail(`lifecycle evidence ${id} has an unsafe path`)
  }
  if (!existsSync(evidencePath)) {
    fail(`lifecycle evidence ${id} is absent: ${item.path}`)
  }
  if (
    !Array.isArray(item.markers) ||
    item.markers.length === 0 ||
    item.markers.some(marker => typeof marker !== 'string' || marker.length < 8)
  ) {
    fail(`lifecycle evidence ${id} must declare non-empty stable markers`)
  }
  const body = readFileSync(evidencePath, 'utf8')
  for (const marker of item.markers) {
    if (!body.includes(marker)) {
      fail(`lifecycle evidence ${id} lost marker ${JSON.stringify(marker)}`)
    }
  }
  if (item.kind === 'bun_test') {
    if (
      !Array.isArray(item.testNames) ||
      item.testNames.length === 0 ||
      item.testNames.some(
        testName => typeof testName !== 'string' || testName.length < 8,
      ) ||
      new Set(item.testNames).size !== item.testNames.length
    ) {
      fail(
        `lifecycle evidence ${id} must declare unique executable Bun testNames`,
      )
    }
  }
}

const rendererKnown = [...rendererLocal, ...rendererExtensions]
const runtimeKnown = [...runtimeCatalog, ...runtimeExtensions]
const failClosedKnown = [...failClosedOnly, ...failClosedExtensions]
const allKnown = [...rendererKnown, ...runtimeKnown, ...failClosedKnown]
const ownerTokens = {
  rendererLocal: [],
  runtimeCatalog: [],
  failClosed: [],
}
const profileIds = new Set()
const tokenProfiles = new Map()
const coverageCounts = Object.fromEntries(
  allowedStatuses.map(status => [status, 0]),
)
const incompleteProfiles = []
const unverifiedProfiles = []
const requiredEvidenceIds = new Set()

if (!Array.isArray(audit.profiles) || audit.profiles.length === 0) {
  fail('lifecycleAudit.profiles must be a non-empty array')
}
for (const [profileIndex, profile] of audit.profiles.entries()) {
  assertExactKeys(`lifecycle profile ${profileIndex}`, profile, [
    'coverage',
    'id',
    'invocationTokens',
    'ownerClass',
  ])
  if (
    typeof profile.id !== 'string' ||
    profile.id.length === 0 ||
    profileIds.has(profile.id)
  ) {
    fail(`lifecycle profile ${profileIndex} has an invalid or duplicate id`)
  }
  profileIds.add(profile.id)
  if (!Object.hasOwn(ownerTokens, profile.ownerClass)) {
    fail(`lifecycle profile ${profile.id} has an invalid ownerClass`)
  }
  const tokens = sortedUnique(
    `lifecycle profile ${profile.id}.invocationTokens`,
    profile.invocationTokens,
  )
  ownerTokens[profile.ownerClass].push(...tokens)
  for (const token of tokens) {
    if (tokenProfiles.has(token)) {
      fail(
        `lifecycle token ${token} is assigned to both ${tokenProfiles.get(token).id} and ${profile.id}`,
      )
    }
    tokenProfiles.set(token, profile)
  }

  assertExactKeys(
    `lifecycle profile ${profile.id}.coverage`,
    profile.coverage,
    expectedDimensions,
  )
  const incompleteDimensions = []
  const unverifiedDimensions = []
  for (const dimension of expectedDimensions) {
    const cell = profile.coverage[dimension]
    const allowedCellKeys = ['evidence', 'status']
    if (cell?.note !== undefined) allowedCellKeys.push('note')
    assertExactKeys(
      `lifecycle profile ${profile.id}.${dimension}`,
      cell,
      allowedCellKeys,
    )
    if (!allowedStatuses.includes(cell.status)) {
      fail(
        `lifecycle profile ${profile.id}.${dimension} has unknown status ${cell.status}`,
      )
    }
    if (
      !Array.isArray(cell.evidence) ||
      cell.evidence.some(id => typeof id !== 'string' || !Object.hasOwn(evidence, id))
    ) {
      fail(`lifecycle profile ${profile.id}.${dimension} has invalid evidence`)
    }
    if (
      ['verified', 'shared_path_only'].includes(cell.status) &&
      cell.evidence.length === 0
    ) {
      fail(
        `lifecycle profile ${profile.id}.${dimension} requires machine evidence`,
      )
    }
    if (
      ['shared_path_only', 'not_applicable', 'unverified'].includes(
        cell.status,
      ) &&
      (typeof cell.note !== 'string' || cell.note.length < 20)
    ) {
      fail(
        `lifecycle profile ${profile.id}.${dimension} requires an explanatory note`,
      )
    }
    coverageCounts[cell.status] += tokens.length
    if (cell.status === 'verified') {
      for (const evidenceId of cell.evidence) {
        requiredEvidenceIds.add(evidenceId)
      }
    }
    if (['shared_path_only', 'unverified'].includes(cell.status)) {
      incompleteDimensions.push(dimension)
    }
    if (cell.status === 'unverified') {
      unverifiedDimensions.push(dimension)
    }
  }
  if (incompleteDimensions.length > 0) {
    incompleteProfiles.push({
      id: profile.id,
      tokens: tokens.length,
      dimensions: incompleteDimensions,
    })
  }
  if (unverifiedDimensions.length > 0) {
    unverifiedProfiles.push({
      id: profile.id,
      tokens: tokens.length,
      dimensions: unverifiedDimensions,
    })
  }
}

const unusedEvidenceIds = Object.keys(evidence)
  .filter(id => !requiredEvidenceIds.has(id))
  .sort()
if (unusedEvidenceIds.length > 0) {
  fail(
    `lifecycle evidence is not referenced by verified coverage: ${unusedEvidenceIds.join(', ')}`,
  )
}

assertSameSet('lifecycle known-token partition', [...tokenProfiles.keys()], allKnown)
assertSameSet('lifecycle renderer owner partition', ownerTokens.rendererLocal, rendererKnown)
assertSameSet('lifecycle runtime owner partition', ownerTokens.runtimeCatalog, runtimeKnown)
assertSameSet('lifecycle fail-closed owner partition', ownerTokens.failClosed, failClosedKnown)

const precedence = audit.ownershipPrecedence
const reservedRenderer = sortedUnique(
  'lifecycleAudit.ownershipPrecedence.reservedRendererPrivate',
  precedence.reservedRendererPrivate,
)
const runtimeCollisionRenderer = sortedUnique(
  'lifecycleAudit.ownershipPrecedence.runtimeCollisionWinsForRendererFallback',
  precedence.runtimeCollisionWinsForRendererFallback,
)
assertSameSet(
  'renderer reserved/fallback ownership partition',
  [...reservedRenderer, ...runtimeCollisionRenderer],
  rendererKnown,
)
if (
  JSON.stringify(precedence.rules) !==
  JSON.stringify([
    'reserved_renderer_private',
    'advertised_runtime_catalog',
    'renderer_local_fallback',
    'known_unavailable_fail_closed',
    'unknown_backend_resolver',
  ])
) {
  fail('ownership precedence order drifted')
}
if (
  precedence.unknownTokenPolicy !==
  'Preserve the existing backend slash resolver; the renderer must not invent either success or failure.'
) {
  fail('unknown-token terminal-truth policy drifted')
}
const reservedFunction = rust.slice(
  rust.indexOf('fn reserved_private_local_command'),
  rust.indexOf('fn fixed_local_command_description'),
)
assertSameSet(
  'Rust reserved renderer-private owner registry',
  [...reservedFunction.matchAll(/"([^"/]+)"/g)].map(match => match[1]),
  reservedRenderer,
)

const discovery = audit.discovery
const rendererVisible = sortedUnique(
  'lifecycleAudit.discovery.rendererLocalVisible',
  discovery.rendererLocalVisible,
)
const runtimeStaticHidden = sortedUnique(
  'lifecycleAudit.discovery.runtimeStaticHidden',
  discovery.runtimeStaticHidden,
)
const runtimeDynamicHidden = sortedUnique(
  'lifecycleAudit.discovery.runtimeDynamicHidden',
  discovery.runtimeDynamicHidden,
)
const runtimeVisible = sortedUnique(
  'lifecycleAudit.discovery.runtimeVisibleWhenAdvertised',
  discovery.runtimeVisibleWhenAdvertised,
)
assertSameSet('renderer discovery partition', rendererVisible, rendererKnown)
assertSameSet(
  'runtime discovery partition',
  [...runtimeStaticHidden, ...runtimeDynamicHidden, ...runtimeVisible],
  runtimeKnown,
)

const detailedReport = process.argv.includes('--report')
const requireComplete = process.argv.includes('--require-complete')
// Mutation-test seam: this can only make the audit stricter and cannot select
// a different contract or suppress a real finding. It exercises the same
// default fail-closed branch used by the package check.
if (process.argv.includes('--test-inject-unverified')) {
  unverifiedProfiles.push({
    id: 'verifier_fail_closed_self_test',
    tokens: 0,
    dimensions: ['failure'],
  })
}
if (requireComplete && incompleteProfiles.length > 0) {
  fail(
    `lifecycle audit is not complete: ${incompleteProfiles.map(profile => profile.id).join(', ')}`,
  )
}

function executeLifecycleEvidence() {
  const requiredItems = [...requiredEvidenceIds]
    .sort()
    .map(id => ({ id, ...evidence[id] }))
  const rustItems = requiredItems.filter(item => item.kind === 'rust_test')
  const bunItems = requiredItems.filter(item => item.kind === 'bun_test')
  const bunTestsByPath = new Map()
  for (const item of bunItems) {
    const names = bunTestsByPath.get(item.path) ?? new Set()
    for (const testName of item.testNames) names.add(testName)
    bunTestsByPath.set(item.path, names)
  }
  const bunPaths = [...bunTestsByPath.keys()].sort()
  const evidenceEnvironment = {
    ...process.env,
    NODE_ENV: 'test',
    ACOSMI_API_KEY: process.env.ACOSMI_API_KEY ?? 'lifecycle-fixture',
    CRABCODE_DISABLE_TELEMETRY: '1',
    DISABLE_BACKGROUND_TASKS: '1',
  }

  if (rustItems.length > 0) {
    const tuiAppRustPath = 'crates/crabcode-tui/src/tui_app.rs'
    const terminalRustPath = 'crates/crabcode-tui/src/terminal.rs'
    const unsupportedRustPaths = [
      ...new Set(
        rustItems
          .map(item => item.path)
          .filter(
            path => path !== tuiAppRustPath && path !== terminalRustPath,
          ),
      ),
    ]
    if (unsupportedRustPaths.length > 0) {
      fail(
        `strict evidence has no exact Rust runner for ${unsupportedRustPaths.join(', ')}`,
      )
    }
    const aggregateName =
      'direct_tui_command_lifecycle_executable_evidence_suite'
    const aggregatePath = resolve(root, tuiAppRustPath)
    const aggregateSource = readFileSync(aggregatePath, 'utf8')
    const aggregateDeclaration = `fn ${aggregateName}() {`
    const aggregateOffset = aggregateSource.indexOf(aggregateDeclaration)
    if (aggregateOffset < 0) {
      fail(`Rust lifecycle aggregate ${aggregateName} is absent`)
    }
    let aggregateTail = aggregateSource.slice(
      aggregateOffset + aggregateDeclaration.length,
    )
    const referencedRustFunctions = new Set(
      rustItems
        .filter(item => item.path === tuiAppRustPath)
        .flatMap(item =>
          item.markers.flatMap(marker => {
            const match = /^fn ([a-zA-Z_][a-zA-Z0-9_]*)\(\)$/.exec(marker)
            return match && match[1] !== aggregateName ? [match[1]] : []
          }),
        ),
    )
    if (process.argv.includes('--test-inject-missing-rust-member')) {
      const injected = [...referencedRustFunctions].sort()[0]
      if (injected) {
        aggregateTail = aggregateTail.replace(`${injected}();`, '')
      }
    }
    for (const functionName of [...referencedRustFunctions].sort()) {
      const invocation = new RegExp(
        `^\\s*${functionName}\\(\\);\\s*$`,
        'm',
      )
      if (!invocation.test(aggregateTail)) {
        fail(
          `Rust lifecycle aggregate ${aggregateName} does not execute referenced marker ${functionName}`,
        )
      }
    }
    const exactAggregateTestName = process.argv.includes(
      '--test-inject-missing-rust-aggregate-test',
    )
      ? `tui_app::tests::${aggregateName}_missing`
      : `tui_app::tests::${aggregateName}`
    const rustResult = spawnSync(
      'cargo',
      [
        'test',
        '--locked',
        '-p',
        'crabcode-tui',
        exactAggregateTestName,
        '--lib',
        '--',
        '--exact',
      ],
      {
        cwd: resolve(root, 'crates'),
        encoding: 'utf8',
        env: evidenceEnvironment,
      },
    )
    if (rustResult.status !== 0) {
      fail(
        `Rust lifecycle evidence failed:\n${rustResult.stdout}${rustResult.stderr}`,
      )
    }
    if (
      !rustResult.stdout.includes(`test ${exactAggregateTestName} ... ok`) ||
      !/test result: ok\. 1 passed; 0 failed; 0 ignored;/.test(
        rustResult.stdout,
      )
    ) {
      fail(
        `Rust lifecycle aggregate ${aggregateName} did not execute exactly once:\n${rustResult.stdout}${rustResult.stderr}`,
      )
    }

    const terminalRustFunctions = new Set(
      rustItems
        .filter(item => item.path === terminalRustPath)
        .flatMap(item =>
          item.markers.flatMap(marker => {
            const match = /^fn ([a-zA-Z_][a-zA-Z0-9_]*)\(\)$/.exec(marker)
            return match ? [match[1]] : []
          }),
        ),
    )
    for (const functionName of [...terminalRustFunctions].sort()) {
      const exactTestName = process.argv.includes(
        '--test-inject-missing-terminal-test',
      )
        ? `terminal::tests::${functionName}_missing`
        : `terminal::tests::${functionName}`
      const terminalResult = spawnSync(
        'cargo',
        [
          'test',
          '--locked',
          '-p',
          'crabcode-tui',
          exactTestName,
          '--lib',
          '--',
          '--exact',
        ],
        {
          cwd: resolve(root, 'crates'),
          encoding: 'utf8',
          env: evidenceEnvironment,
        },
      )
      if (terminalResult.status !== 0) {
        fail(
          `Rust terminal lifecycle evidence ${functionName} failed:\n${terminalResult.stdout}${terminalResult.stderr}`,
        )
      }
      if (
        !terminalResult.stdout.includes(`test ${exactTestName} ... ok`) ||
        !/test result: ok\. 1 passed; 0 failed; 0 ignored;/.test(
          terminalResult.stdout,
        )
      ) {
        fail(
          `Rust terminal lifecycle evidence ${functionName} did not execute exactly once:\n${terminalResult.stdout}${terminalResult.stderr}`,
        )
      }
    }
  }

  // Run files serially. Several fixtures launch a real QueryEngine or Rust
  // linker; serial evidence keeps their command deadlines meaningful instead
  // of turning host CPU contention into a false lifecycle failure.
  for (const [pathIndex, path] of bunPaths.entries()) {
    const bunResult = spawnSync(process.execPath, ['test', path], {
      cwd: root,
      encoding: 'utf8',
      env: evidenceEnvironment,
    })
    if (bunResult.status !== 0) {
      fail(
        `Bun lifecycle evidence failed for ${path}:\n${bunResult.stdout}${bunResult.stderr}`,
      )
    }
    const expectedTests = new Set(bunTestsByPath.get(path))
    if (
      pathIndex === 0 &&
      process.argv.includes('--test-inject-missing-bun-test')
    ) {
      expectedTests.add('__missing_bun_lifecycle_evidence_test__')
    }
    const passedTests = new Map()
    const skippedTests = new Set()
    for (const line of `${bunResult.stdout}\n${bunResult.stderr}`.split(/\r?\n/u)) {
      const passed = /^\(pass\) (.*?)(?: \[[^\]]+\])?$/.exec(line)
      if (passed) {
        passedTests.set(passed[1], (passedTests.get(passed[1]) ?? 0) + 1)
        continue
      }
      const skipped = /^\((?:skip|todo)\) (.*)$/.exec(line)
      if (skipped) skippedTests.add(skipped[1])
    }
    if (
      pathIndex === 0 &&
      process.argv.includes('--test-inject-skipped-bun-result')
    ) {
      const injected = [...expectedTests][0]
      if (injected) {
        passedTests.delete(injected)
        skippedTests.add(injected)
      }
    }
    for (const testName of [...expectedTests].sort()) {
      const passCount = passedTests.get(testName) ?? 0
      if (passCount !== 1 || skippedTests.has(testName)) {
        fail(
          `Bun lifecycle evidence ${JSON.stringify(testName)} in ${path} did not execute exactly once as a passing test; passes=${passCount} skipped=${skippedTests.has(testName)}`,
        )
      }
    }
  }
}

if (requireComplete) {
  executeLifecycleEvidence()
}
// A diagnostic report may describe unfinished work, but every non-report
// invocation must fail while even one lifecycle cell remains explicitly
// unverified. The package-level release gate additionally uses
// --require-complete, which executes the evidence suites above.
if (!detailedReport && unverifiedProfiles.length > 0) {
  fail(
    `lifecycle audit contains unverified coverage: ${unverifiedProfiles
      .map(
        profile =>
          `${profile.id}[${profile.dimensions.join(',')}]`,
      )
      .join(', ')}`,
  )
}

const verificationStatus =
  unverifiedProfiles.length > 0
    ? 'unverified'
    : incompleteProfiles.length > 0
      ? 'incomplete'
      : 'verified'

const lifecycleSummary = {
  knownTokens: allKnown.length,
  profiles: audit.profiles.length,
  dimensions: expectedDimensions.length,
  coverageCells: allKnown.length * expectedDimensions.length,
  coverageCounts,
  incompleteProfiles,
  unverifiedProfiles,
}
if (detailedReport) {
  lifecycleSummary.commands = [...tokenProfiles.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([token, profile]) => ({
      token,
      profile: profile.id,
      ownerClass: profile.ownerClass,
      discovery:
        rendererVisible.includes(token) || runtimeVisible.includes(token)
          ? 'visible'
          : runtimeStaticHidden.includes(token)
            ? 'hidden'
            : runtimeDynamicHidden.includes(token)
              ? 'dynamic_hidden'
              : 'unadvertised_fail_closed',
      coverage: Object.fromEntries(
        expectedDimensions.map(dimension => [
          dimension,
          profile.coverage[dimension].status,
        ]),
      ),
    }))
}

process.stdout.write(
  `${JSON.stringify({
    schemaVersion: contract.schemaVersion,
    reference: reference.length,
    rendererLocal: rendererLocal.length,
    runtimeCatalog: runtimeCatalog.length,
    failClosedOnly: failClosedOnly.length,
    nonReferenceKnown:
      rendererExtensions.length +
      runtimeExtensions.length +
      failClosedExtensions.length,
    referenceStatus,
    lifecycle: lifecycleSummary,
    status: verificationStatus,
  })}\n`,
)
