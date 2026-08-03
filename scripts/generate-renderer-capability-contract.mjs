#!/usr/bin/env bun

import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const GENERATED_RUST_CONTRACT_PATH = resolve(
  REPO_ROOT,
  'crates/crabcode-tui/src/generated_renderer_contract.rs',
)
const EVENT_POLICY_PATH = resolve(
  REPO_ROOT,
  'contracts/renderer-protocol/v1/event-policy.json',
)
const GENERATED_JSON_CONTRACT_PATH = resolve(
  REPO_ROOT,
  'contracts/renderer-protocol/v1/renderer-contract.json',
)
const EVENT_DISPOSITIONS = new Set([
  'presentation-only',
  'recoverable',
  'turn-fatal',
  'protocol-fatal',
])

/**
 * Compiler-expanded TypeScript protocol families and their intended Rust
 * migration owners. These are source symbols, not a handwritten capability
 * allowlist: adding or removing a union member changes the generated Rust file
 * and fails `bun run check` until the native owner is reviewed.
 */
const CONTRACT_SPECS = Object.freeze([
  {
    id: 'query_message_type',
    source: 'src/types/message.ts',
    symbol: 'Message',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_runtime -> sdk_projection -> scrollback_projection',
  },
  {
    id: 'direct_system_subtype',
    source: 'src/types/message.ts',
    symbol: 'SystemMessage',
    discriminator: 'subtype',
    rustOwner: 'crabcode-tui::sdk_runtime::DirectSystemSubtype + sdk_projection',
  },
  {
    id: 'progress_type',
    source: 'src/Tool.ts',
    symbol: 'Progress',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_projection progress lifecycle',
  },
  {
    id: 'attachment_type',
    source: 'src/utils/attachments-types.ts',
    symbol: 'Attachment',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_projection attachment disposition partition',
  },
  {
    id: 'stream_event_type',
    source: 'src/types/api-types.ts',
    symbol: 'NormalizedAcosmiChatStreamEvent',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_projection generation-aware stream reducer',
  },
  {
    id: 'assistant_content_block_type',
    source: 'src/types/api-types.ts',
    symbol: 'BetaContentBlock',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_projection::AssistantBlockType',
  },
  {
    id: 'user_content_block_type',
    source: 'src/types/api-types.ts',
    symbol: 'BetaContentBlockParam',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_projection::DirectUserBlockType',
  },
  {
    id: 'sdk_message_type',
    source: 'src/entrypoints/sdk/coreTypes.generated.ts',
    symbol: 'SDKMessage',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_runtime::EnvelopeClass',
  },
  {
    id: 'sdk_system_subtype',
    source: 'src/entrypoints/sdk/coreTypes.generated.ts',
    symbol: 'SDKMessage',
    discriminator: 'subtype',
    filter: { discriminator: 'type', values: ['system'] },
    rustOwner: 'crabcode-tui::sdk_runtime::SystemSubtype',
  },
  {
    id: 'stdout_system_subtype',
    source: 'src/entrypoints/sdk/controlTypes.ts',
    symbol: 'StdoutMessage',
    discriminator: 'subtype',
    filter: { discriminator: 'type', values: ['system'] },
    rustOwner: 'crabcode-tui::sdk_runtime::SystemSubtype',
  },
  {
    id: 'sdk_result_subtype',
    source: 'src/entrypoints/sdk/coreTypes.generated.ts',
    symbol: 'SDKMessage',
    discriminator: 'subtype',
    filter: { discriminator: 'type', values: ['result'] },
    rustOwner: 'crabcode-tui::sdk_runtime correlated result lifecycle',
  },
  {
    id: 'control_request_subtype',
    source: 'src/entrypoints/sdk/controlTypes.ts',
    symbol: 'SDKControlRequestInner',
    discriminator: 'subtype',
    rustOwner: 'crabcode-tui::sdk_runtime reverse-control classifier',
  },
  {
    id: 'stdout_message_type',
    source: 'src/entrypoints/sdk/controlTypes.ts',
    symbol: 'StdoutMessage',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_runtime bounded stdout reader',
  },
  {
    id: 'stdin_message_type',
    source: 'src/entrypoints/sdk/controlTypes.ts',
    symbol: 'StdinMessage',
    discriminator: 'type',
    rustOwner: 'crabcode-tui::sdk_runtime bounded outbound writer',
  },
  {
    id: 'private_runtime_action_kind',
    source: 'src/cli/directTuiRuntimeActions.ts',
    symbol: 'DirectTuiRuntimeAction',
    discriminator: 'kind',
    rustOwner: 'crabcode-tui::sdk_runtime::validate_private_runtime_action',
  },
  {
    id: 'private_runtime_result_kind',
    source: 'src/cli/directTuiRuntimeActions.ts',
    symbol: 'DirectTuiRuntimeResult',
    discriminator: 'kind',
    rustOwner: 'crabcode-tui::sdk_runtime private result classifier',
  },
  {
    id: 'private_setup_request_route',
    source: 'src/cli/crabcodeTuiBridgeProtocol.ts',
    symbol: 'CrabCodeTuiSetupRequest',
    discriminators: ['kind', 'stage', 'phase'],
    rustOwner: 'crabcode-tui::tui_app private setup request router',
  },
  {
    id: 'private_setup_response_route',
    source: 'src/cli/crabcodeTuiBridgeProtocol.ts',
    symbol: 'CrabCodeTuiSetupResponse',
    discriminators: ['kind', 'stage', 'phase'],
    rustOwner: 'crabcode-tui::tui_app private setup response encoder',
  },
])

const RUST_ARRAYS = Object.freeze([
  ['GENERATED_DIRECT_MESSAGE_TYPES', 'query_message_type', false],
  ['GENERATED_DIRECT_SYSTEM_SUBTYPES', 'direct_system_subtype', true],
  ['GENERATED_DIRECT_PROGRESS_TYPES', 'progress_type', false],
  ['GENERATED_DIRECT_ATTACHMENT_TYPES', 'attachment_type', false],
  ['GENERATED_STREAM_EVENT_TYPES', 'stream_event_type', true],
  [
    'GENERATED_ASSISTANT_CONTENT_BLOCK_TYPES',
    'assistant_content_block_type',
    true,
  ],
  ['GENERATED_USER_CONTENT_BLOCK_TYPES', 'user_content_block_type', true],
  ['GENERATED_SDK_MESSAGE_TYPES', 'sdk_message_type', true],
  ['GENERATED_SDK_SYSTEM_SUBTYPES', 'sdk_system_subtype', true],
  ['GENERATED_STDOUT_SYSTEM_SUBTYPES', 'stdout_system_subtype', false],
  ['GENERATED_SDK_RESULT_SUBTYPES', 'sdk_result_subtype', true],
  ['GENERATED_CONTROL_REQUEST_SUBTYPES', 'control_request_subtype', false],
  ['GENERATED_STDOUT_MESSAGE_TYPES', 'stdout_message_type', true],
  ['GENERATED_STDIN_MESSAGE_TYPES', 'stdin_message_type', true],
  [
    'GENERATED_PRIVATE_RUNTIME_ACTION_KINDS',
    'private_runtime_action_kind',
    true,
  ],
  [
    'GENERATED_PRIVATE_RUNTIME_RESULT_KINDS',
    'private_runtime_result_kind',
    true,
  ],
  [
    'GENERATED_PRIVATE_SETUP_REQUEST_ROUTES',
    'private_setup_request_route',
    true,
  ],
  [
    'GENERATED_PRIVATE_SETUP_RESPONSE_ROUTES',
    'private_setup_response_route',
    true,
  ],
])

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function stable(value) {
  if (Array.isArray(value)) return value.map(stable)
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map(key => [key, stable(value[key])]),
    )
  }
  return value
}

function canonicalJson(value) {
  return JSON.stringify(stable(value))
}

function unionMembers(type) {
  return type.isUnion() ? type.types : [type]
}

function literalPropertyValues(checker, type, propertyName, sourceFile) {
  const property = checker.getPropertyOfType(type, propertyName)
  if (!property) return { literals: [], broadString: false }
  const declaration =
    property.valueDeclaration ?? property.declarations?.[0] ?? sourceFile
  const propertyType = checker.getTypeOfSymbolAtLocation(property, declaration)
  const literals = []
  let broadString = false
  for (const member of unionMembers(propertyType)) {
    if (member.isStringLiteral()) literals.push(member.value)
    else if ((member.flags & ts.TypeFlags.String) !== 0) broadString = true
  }
  return { literals, broadString }
}

function memberMatchesFilter(checker, member, filter, sourceFile) {
  if (!filter) return true
  const extracted = literalPropertyValues(
    checker,
    member,
    filter.discriminator,
    sourceFile,
  )
  if (extracted.broadString) {
    throw new Error(
      `cannot filter broad string discriminator ${filter.discriminator}`,
    )
  }
  return extracted.literals.some(value => filter.values.includes(value))
}

function extractFamily(program, checker, spec) {
  const absoluteSource = resolve(REPO_ROOT, spec.source)
  const sourceFile = program.getSourceFile(absoluteSource)
  if (!sourceFile) {
    throw new Error(`TypeScript source not in program: ${spec.source}`)
  }
  const moduleSymbol = checker.getSymbolAtLocation(sourceFile)
  if (!moduleSymbol) throw new Error(`source has no module symbol: ${spec.source}`)
  const exported = checker
    .getExportsOfModule(moduleSymbol)
    .find(symbol => symbol.name === spec.symbol)
  if (!exported) {
    throw new Error(`missing exported type ${spec.symbol} in ${spec.source}`)
  }

  const declaredType = checker.getDeclaredTypeOfSymbol(exported)
  const values = new Set()
  let matchingMembers = 0
  const discriminators = spec.discriminators ?? [spec.discriminator]
  for (const member of unionMembers(declaredType)) {
    if (!memberMatchesFilter(checker, member, spec.filter, sourceFile)) continue
    matchingMembers += 1
    let memberRoutes = ['']
    for (const discriminator of discriminators) {
      const extracted = literalPropertyValues(
        checker,
        member,
        discriminator,
        sourceFile,
      )
      if (extracted.broadString) {
        throw new Error(
          `${spec.id} has a broad string ${discriminator}; the contract would not be exact`,
        )
      }
      const discriminatorValues =
        extracted.literals.length > 0 ? extracted.literals : ['-']
      memberRoutes = memberRoutes.flatMap(route =>
        discriminatorValues.map(value =>
          discriminators.length === 1
            ? value
            : `${route}${route ? '|' : ''}${discriminator}=${value}`,
        ),
      )
    }
    for (const route of memberRoutes) values.add(route)
  }

  if (matchingMembers === 0 || values.size === 0) {
    throw new Error(`${spec.id} produced no literal discriminator values`)
  }

  return {
    id: spec.id,
    source: spec.source,
    sourceSha256: sha256(readFileSync(absoluteSource)),
    symbol: spec.symbol,
    ...(spec.discriminators
      ? { discriminators: spec.discriminators }
      : { discriminator: spec.discriminator }),
    ...(spec.filter ? { filter: spec.filter } : {}),
    rustOwner: spec.rustOwner,
    values: [...values].sort(),
    count: values.size,
  }
}

function loadProgram() {
  const tsconfigPath = resolve(REPO_ROOT, 'tsconfig.json')
  const loaded = ts.readConfigFile(tsconfigPath, ts.sys.readFile)
  if (loaded.error) {
    throw new Error(
      ts.flattenDiagnosticMessageText(loaded.error.messageText, '\n'),
    )
  }
  const parsed = ts.parseJsonConfigFileContent(
    loaded.config,
    ts.sys,
    REPO_ROOT,
    undefined,
    tsconfigPath,
  )
  if (parsed.errors.length > 0) {
    throw new Error(
      parsed.errors
        .map(error =>
          ts.flattenDiagnosticMessageText(error.messageText, '\n'),
        )
        .join('\n'),
    )
  }
  return ts.createProgram({ rootNames: parsed.fileNames, options: parsed.options })
}

function loadAndResolveEventPolicy(families) {
  if (!existsSync(EVENT_POLICY_PATH)) {
    throw new Error(
      `event policy missing: ${relative(REPO_ROOT, EVENT_POLICY_PATH)}`,
    )
  }
  const parsed = JSON.parse(readFileSync(EVENT_POLICY_PATH, 'utf8'))
  if (parsed?.schemaVersion !== 1 || typeof parsed.families !== 'object') {
    throw new Error('event policy must have schemaVersion=1 and families')
  }

  const extractedIds = new Set(families.map(family => family.id))
  const policyIds = Object.keys(parsed.families)
  for (const id of extractedIds) {
    if (!(id in parsed.families)) {
      throw new Error(`event policy is missing family ${id}`)
    }
  }
  for (const id of policyIds) {
    if (!extractedIds.has(id)) {
      throw new Error(`event policy contains stale family ${id}`)
    }
  }

  const resolved = {}
  for (const family of families) {
    const policy = parsed.families[family.id]
    if (typeof policy?.owner !== 'string' || policy.owner.trim() === '') {
      throw new Error(`event policy family ${family.id} has no owner`)
    }
    if (!EVENT_DISPOSITIONS.has(policy.default)) {
      throw new Error(
        `event policy family ${family.id} has invalid default ${String(policy.default)}`,
      )
    }
    if (
      policy.overrides === null ||
      typeof policy.overrides !== 'object' ||
      Array.isArray(policy.overrides)
    ) {
      throw new Error(`event policy family ${family.id} overrides must be an object`)
    }
    const values = new Set(family.values)
    for (const [value, disposition] of Object.entries(policy.overrides)) {
      if (!values.has(value)) {
        throw new Error(
          `event policy family ${family.id} overrides unknown value ${value}`,
        )
      }
      if (!EVENT_DISPOSITIONS.has(disposition)) {
        throw new Error(
          `event policy family ${family.id} value ${value} has invalid disposition ${String(disposition)}`,
        )
      }
    }
    resolved[family.id] = {
      owner: policy.owner,
      default: policy.default,
      values: Object.fromEntries(
        family.values.map(value => [
          value,
          policy.overrides[value] ?? policy.default,
        ]),
      ),
    }
  }
  return {
    schemaVersion: 1,
    source: relative(REPO_ROOT, EVENT_POLICY_PATH),
    sourceSha256: sha256(readFileSync(EVENT_POLICY_PATH)),
    families: resolved,
  }
}

function generateRendererCapabilityContract() {
  const program = loadProgram()
  const checker = program.getTypeChecker()
  const families = CONTRACT_SPECS.map(spec =>
    extractFamily(program, checker, spec),
  )
  const totalCapabilities = families.reduce(
    (sum, family) => sum + family.values.length,
    0,
  )
  const eventPolicy = loadAndResolveEventPolicy(families)
  const body = {
    schemaVersion: 1,
    generatedBy: 'scripts/generate-renderer-capability-contract.mjs',
    generationAuthority:
      'current TypeScript compiler-expanded unions; no renderer allowlist',
    migrationAuthority:
      'rustOwner identifies the native module that must reach behavioral parity before the TypeScript family can be removed',
    typescriptVersion: ts.version,
    tsconfig: relative(REPO_ROOT, resolve(REPO_ROOT, 'tsconfig.json')),
    totalCapabilities,
    families,
    eventPolicy,
  }
  return {
    ...body,
    contractSha256: sha256(canonicalJson(body)),
    contractHashScope:
      'canonical JSON excluding contractSha256 and contractHashScope',
  }
}

function rustDisposition(value) {
  const variants = {
    'presentation-only': 'PresentationOnly',
    recoverable: 'Recoverable',
    'turn-fatal': 'TurnFatal',
    'protocol-fatal': 'ProtocolFatal',
  }
  const variant = variants[value]
  if (!variant) throw new Error(`unknown event disposition ${value}`)
  return `GeneratedEventDisposition::${variant}`
}

function rustStreamDispositionPolicy(contract) {
  const family = contract.eventPolicy.families.stream_event_type
  const values = Object.entries(family.values)
    .map(
      ([value, disposition]) =>
        `    (${JSON.stringify(value)}, ${rustDisposition(disposition)}),`,
    )
    .join('\n')
  return `#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedEventDisposition {
    PresentationOnly,
    Recoverable,
    TurnFatal,
    ProtocolFatal,
}

#[allow(dead_code)]
pub(crate) const GENERATED_STREAM_EVENT_POLICIES: &[(&str, GeneratedEventDisposition)] = &[
${values}
];

#[allow(dead_code)]
pub(crate) fn generated_stream_event_disposition(
    event_type: &str,
) -> Option<GeneratedEventDisposition> {
    GENERATED_STREAM_EVENT_POLICIES
        .iter()
        .find_map(|(known, disposition)| (*known == event_type).then_some(*disposition))
}`
}

function rustStringArray(name, family, testOnly) {
  const values = family.values
    .map(value => `    ${JSON.stringify(value)},`)
    .join('\n')
  const cfg = testOnly ? '#[cfg(test)]\n#[allow(dead_code)]\n' : ''
  return `${cfg}pub(crate) const ${name}: &[&str] = &[\n${values}\n];`
}

function rustMigrationFamilies(families) {
  const values = families
    .map(
      family =>
        `    (\n        ${JSON.stringify(family.id)},\n        ${JSON.stringify(family.source)},\n        ${JSON.stringify(family.symbol)},\n        ${JSON.stringify(family.rustOwner)},\n    ),`,
    )
    .join('\n')
  return `#[cfg(test)]\n#[allow(dead_code)]\npub(crate) const GENERATED_TS_TO_RUST_MIGRATION_FAMILIES: &[(&str, &str, &str, &str)] = &[\n${values}\n];`
}

function serializeRustContract(contract) {
  const families = new Map(contract.families.map(family => [family.id, family]))
  const required = id => {
    const family = families.get(id)
    if (!family) throw new Error(`generated Rust contract missing family ${id}`)
    return family
  }
  return `${[
    '// @generated by scripts/generate-renderer-capability-contract.mjs',
    '// Do not edit by hand. Regenerate from the current TypeScript unions.',
    '',
    'pub(crate) const GENERATED_RENDERER_CAPABILITY_CONTRACT_SHA256: &str =',
    `    ${JSON.stringify(contract.contractSha256)};`,
    '',
    rustStreamDispositionPolicy(contract),
    '',
    ...RUST_ARRAYS.flatMap(([name, familyId, testOnly]) => [
      rustStringArray(name, required(familyId), testOnly),
      '',
    ]),
    rustMigrationFamilies(contract.families),
  ].join('\n')}\n`
}

function checkGeneratedContract() {
  if (!existsSync(GENERATED_RUST_CONTRACT_PATH)) {
    throw new Error(
      `generated Rust contract missing: ${relative(REPO_ROOT, GENERATED_RUST_CONTRACT_PATH)}; run --write`,
    )
  }
  if (!existsSync(GENERATED_JSON_CONTRACT_PATH)) {
    throw new Error(
      `generated JSON contract missing: ${relative(REPO_ROOT, GENERATED_JSON_CONTRACT_PATH)}; run --write`,
    )
  }
  const contract = generateRendererCapabilityContract()
  const expectedRust = serializeRustContract(contract)
  const actualRust = readFileSync(GENERATED_RUST_CONTRACT_PATH, 'utf8')
  if (actualRust !== expectedRust) {
    throw new Error(
      `generated Rust renderer contract is stale: ${relative(REPO_ROOT, GENERATED_RUST_CONTRACT_PATH)}; run bun scripts/generate-renderer-capability-contract.mjs --write and review every changed Rust owner`,
    )
  }
  const expectedJson = `${JSON.stringify(contract, null, 2)}\n`
  const actualJson = readFileSync(GENERATED_JSON_CONTRACT_PATH, 'utf8')
  if (actualJson !== expectedJson) {
    throw new Error(
      `generated JSON renderer contract is stale: ${relative(REPO_ROOT, GENERATED_JSON_CONTRACT_PATH)}; run --write`,
    )
  }
  return contract
}

function main(argv) {
  if (argv.length !== 1 || !['--write', '--check', '--print'].includes(argv[0])) {
    throw new Error(
      'usage: bun scripts/generate-renderer-capability-contract.mjs --write|--check|--print',
    )
  }
  if (argv[0] === '--write') {
    const contract = generateRendererCapabilityContract()
    mkdirSync(dirname(GENERATED_JSON_CONTRACT_PATH), { recursive: true })
    writeFileSync(
      GENERATED_RUST_CONTRACT_PATH,
      serializeRustContract(contract),
      { mode: 0o644 },
    )
    writeFileSync(
      GENERATED_JSON_CONTRACT_PATH,
      `${JSON.stringify(contract, null, 2)}\n`,
      { mode: 0o644 },
    )
    console.log(
      `wrote ${relative(REPO_ROOT, GENERATED_RUST_CONTRACT_PATH)} and ${relative(REPO_ROOT, GENERATED_JSON_CONTRACT_PATH)} (${contract.totalCapabilities} capabilities, ${contract.contractSha256})`,
    )
    return
  }
  if (argv[0] === '--check') {
    const contract = checkGeneratedContract()
    console.log(
      `renderer capability contract PASS (${contract.totalCapabilities} capabilities, ${contract.contractSha256})`,
    )
    return
  }
  process.stdout.write(`${JSON.stringify(generateRendererCapabilityContract(), null, 2)}\n`)
}

try {
  main(process.argv.slice(2))
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error))
  process.exitCode = 1
}
