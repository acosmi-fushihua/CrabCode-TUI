import { realpath as fsRealpath, stat as fsStat } from 'fs/promises'
import { homedir } from 'os'
import { basename, dirname, isAbsolute, relative, resolve, sep } from 'path'
import {
  getStrictKnownMarketplaces,
  isSourceAllowedByPolicy,
  isSourceInBlocklist,
} from './marketplaceHelpers.js'
import { OFFICIAL_MARKETPLACE_SOURCE } from './officialMarketplace.js'
import { normalizeMarketplaceSparsePaths } from './marketplaceSparsePaths.js'
import type { MarketplaceSource } from './schemas.js'

const NETWORK_RAW_AMBIGUITY = /[\\\u0000-\u001f\u007f]/u
const GIT_REPOSITORY_HOSTS = new Set(['github.com', 'gitlab.com'])
const GIT_REPOSITORY_SEGMENT = /^[a-z0-9._~-]+$/iu

export const RECOMMENDED_MARKETPLACE_SOURCE = {
  source: 'github',
  repo: 'acosmi/crab-LawAgent',
} as const satisfies MarketplaceSource

interface MarketplaceIngressPolicyContext {
  homeDirectory: string
  hasStrictKnownMarketplaces: boolean
  isBlocked: (source: MarketplaceSource) => boolean
  isAllowed: (source: MarketplaceSource) => boolean
}

export type LocalMarketplaceIngressSource = Extract<
  MarketplaceSource,
  { source: 'file' | 'directory' }
>

type LocalMarketplaceIngressStat = {
  dev: number | bigint
  ino: number | bigint
  isFile(): boolean
  isDirectory(): boolean
}

export type LocalMarketplaceIngressIdentity = Readonly<{
  homeDirectory: string
  canonicalHome: string
  sourcePath: string
  canonicalTarget: string
  sourceKind: LocalMarketplaceIngressSource['source']
  device: string
  inode: string
}>

export type MarketplaceIngressSourceValidation = Readonly<{
  source: MarketplaceSource
  localIdentity?: LocalMarketplaceIngressIdentity
}>

type LocalMarketplaceIngressIdentityDependencies = {
  homeDirectory: string
  realpath: (path: string) => Promise<string>
  stat: (path: string) => Promise<LocalMarketplaceIngressStat>
}

let localIdentityDependenciesOverride: Partial<LocalMarketplaceIngressIdentityDependencies> | null =
  null

function localIdentityDependencies(): LocalMarketplaceIngressIdentityDependencies {
  return {
    homeDirectory: homedir(),
    realpath: fsRealpath,
    stat: (path) => fsStat(path, { bigint: true }),
    ...localIdentityDependenciesOverride,
  }
}

/** Narrow test seam for platforms where creating symlinks is unavailable. */
export function __setLocalMarketplaceIngressIdentityDependenciesForTest(
  dependencies: Partial<LocalMarketplaceIngressIdentityDependencies> | null,
): void {
  localIdentityDependenciesOverride = dependencies
}

export class MarketplaceIngressPolicyError extends Error {
  constructor(message: string) {
    super(`Marketplace source URL not allowed: ${message}`)
    this.name = 'MarketplaceIngressPolicyError'
  }
}

function reject(message: string): never {
  throw new MarketplaceIngressPolicyError(message)
}

function isLocalSource(
  source: MarketplaceSource,
): source is Extract<MarketplaceSource, { source: 'file' | 'directory' }> {
  return source.source === 'file' || source.source === 'directory'
}

function isCanonicalPathInside(home: string, target: string): boolean {
  const rel = relative(home, target)
  return (
    rel === '' ||
    (rel !== '..' && !rel.startsWith(`..${sep}`) && !isAbsolute(rel))
  )
}

function stableIdentityField(
  value: number | bigint,
  field: 'device' | 'inode',
): string {
  if (typeof value === 'bigint') return value.toString(10)
  if (Number.isSafeInteger(value) && value >= 0) return String(value)
  reject(`local marketplace ${field} identity is unavailable`)
}

async function captureLocalIdentityWithDependencies(
  source: LocalMarketplaceIngressSource,
  dependencies: LocalMarketplaceIngressIdentityDependencies,
): Promise<LocalMarketplaceIngressIdentity> {
  if (source.path.includes('\0')) {
    reject('local paths must not contain a NUL byte')
  }
  if (!isAbsolute(source.path)) {
    reject('parsed local paths must be absolute')
  }

  let canonicalHome: string
  let canonicalTarget: string
  try {
    canonicalHome = await dependencies.realpath(dependencies.homeDirectory)
  } catch {
    reject('the user home directory could not be canonicalized')
  }
  try {
    canonicalTarget = await dependencies.realpath(source.path)
  } catch {
    reject(
      'the local marketplace path does not exist or cannot be canonicalized',
    )
  }

  if (!isCanonicalPathInside(canonicalHome, canonicalTarget)) {
    reject('local marketplace paths must remain inside the canonical user home')
  }

  let targetStats: LocalMarketplaceIngressStat
  try {
    targetStats = await dependencies.stat(canonicalTarget)
  } catch {
    reject('the canonical local marketplace path cannot be inspected')
  }

  if (source.source === 'file') {
    if (!targetStats.isFile() || !canonicalTarget.endsWith('.json')) {
      reject('file sources must resolve to a regular .json file')
    }
    if (
      basename(canonicalTarget) !== 'marketplace.json' ||
      basename(dirname(canonicalTarget)) !== '.crabcode-plugin'
    ) {
      reject(
        'file sources must target <root>/.crabcode-plugin/marketplace.json',
      )
    }
  } else if (!targetStats.isDirectory()) {
    reject('directory sources must resolve to an actual directory')
  }

  return {
    homeDirectory: dependencies.homeDirectory,
    canonicalHome,
    // Policy returns the canonical target as the authoritative source path.
    // Bind the token to that path rather than to an expendable input symlink.
    sourcePath: canonicalTarget,
    canonicalTarget,
    sourceKind: source.source,
    device: stableIdentityField(targetStats.dev, 'device'),
    inode: stableIdentityField(targetStats.ino, 'inode'),
  }
}

/**
 * Capture a stable identity token for an already-parsed local remote ingress
 * marketplace source. Callers should persist/use the canonicalTarget returned
 * in the token as the source path.
 */
export async function captureLocalMarketplaceIngressIdentity(
  source: LocalMarketplaceIngressSource,
): Promise<LocalMarketplaceIngressIdentity> {
  return captureLocalIdentityWithDependencies(
    source,
    localIdentityDependencies(),
  )
}

/**
 * Revalidate a captured local source immediately before every security-
 * sensitive read or commit. The home alias, canonical target, source type,
 * device, and inode must all remain identical.
 */
export async function revalidateLocalMarketplaceIngressIdentity(
  source: LocalMarketplaceIngressSource,
  identity: LocalMarketplaceIngressIdentity,
): Promise<void> {
  if (
    source.source !== identity.sourceKind ||
    source.path !== identity.sourcePath
  ) {
    reject('local marketplace source no longer matches its captured identity')
  }

  const dependencies = localIdentityDependencies()
  let canonicalHome: string
  let canonicalTarget: string
  try {
    canonicalHome = await dependencies.realpath(identity.homeDirectory)
    canonicalTarget = await dependencies.realpath(source.path)
  } catch {
    reject('local marketplace identity changed while the operation was running')
  }

  if (
    canonicalHome !== identity.canonicalHome ||
    canonicalTarget !== identity.canonicalTarget ||
    !isCanonicalPathInside(identity.canonicalHome, canonicalTarget)
  ) {
    reject('local marketplace identity changed while the operation was running')
  }

  let targetStats: LocalMarketplaceIngressStat
  try {
    targetStats = await dependencies.stat(canonicalTarget)
  } catch {
    reject('local marketplace identity changed while the operation was running')
  }
  const typeMatches =
    identity.sourceKind === 'file'
      ? targetStats.isFile()
      : targetStats.isDirectory()
  if (
    !typeMatches ||
    stableIdentityField(targetStats.dev, 'device') !== identity.device ||
    stableIdentityField(targetStats.ino, 'inode') !== identity.inode
  ) {
    reject('local marketplace identity changed while the operation was running')
  }
}

function hasPortableTraversal(rawInput: string): boolean {
  return rawInput
    .trim()
    .replace(/\\/gu, '/')
    .split('/')
    .some((segment) => segment === '..')
}

function assertUnambiguousNetworkRaw(value: string): void {
  if (NETWORK_RAW_AMBIGUITY.test(value)) {
    reject(
      'network source text must not contain backslashes or ASCII control characters',
    )
  }
}

function normalizeSparsePathsOnSource(
  source: MarketplaceSource,
): MarketplaceSource {
  if (
    (source.source === 'git' || source.source === 'github') &&
    source.sparsePaths !== undefined
  ) {
    return {
      ...source,
      sparsePaths: normalizeMarketplaceSparsePaths(source.sparsePaths),
    }
  }
  return source
}

function rawUrlAuthority(url: string): string | null {
  const schemeSeparator = url.indexOf('://')
  if (schemeSeparator < 0) return null
  const afterScheme = url.slice(schemeSeparator + 3)
  const authorityEnd = afterScheme.search(/[/?#]/u)
  return authorityEnd < 0 ? afterScheme : afterScheme.slice(0, authorityEnd)
}

function assertCanonicalGitRepositoryUrl(
  rawInput: string,
  sourceUrl: string,
  parsed: URL,
): void {
  const rawTrimmed = rawInput.trim()
  if (/[?#]/u.test(rawTrimmed) || /[?#]/u.test(sourceUrl)) {
    reject('git repository URLs must not contain a query or fragment')
  }

  const authority = rawUrlAuthority(sourceUrl)
  if (
    authority === null ||
    authority.toLowerCase() !== parsed.hostname.toLowerCase()
  ) {
    // After userinfo is rejected, any authority suffix is a port. Comparing
    // the raw authority is necessary because WHATWG drops an explicit :443.
    reject('git repository URLs must not specify a port')
  }
  if (parsed.port.length > 0) {
    reject('git repository URLs must not specify a port')
  }
  if (!GIT_REPOSITORY_HOSTS.has(parsed.hostname)) {
    reject('git sources are limited to github.com or gitlab.com')
  }

  const pathSegments = parsed.pathname.slice(1).split('/')
  if (pathSegments.length !== 2) {
    reject('git repository URLs must use exactly /<org>/<repo>[.git]')
  }
  const [organization, repositoryWithSuffix] = pathSegments
  const repository = repositoryWithSuffix?.endsWith('.git')
    ? repositoryWithSuffix.slice(0, -4)
    : repositoryWithSuffix
  if (
    !organization ||
    !repository ||
    organization === '.' ||
    organization === '..' ||
    repository === '.' ||
    repository === '..' ||
    !GIT_REPOSITORY_SEGMENT.test(organization) ||
    !GIT_REPOSITORY_SEGMENT.test(repository)
  ) {
    reject('git repository URLs must use exactly /<org>/<repo>[.git]')
  }
}

function canonicalizeNetworkSource(
  rawInput: string,
  inputSource: MarketplaceSource,
): MarketplaceSource {
  assertUnambiguousNetworkRaw(rawInput)
  const source = normalizeSparsePathsOnSource(inputSource)

  if (source.source === 'github') {
    const segments = source.repo.split('/')
    if (
      segments.length !== 2 ||
      segments.some(
        (segment) =>
          segment.length === 0 ||
          segment === '.' ||
          segment === '..' ||
          !GIT_REPOSITORY_SEGMENT.test(segment),
      )
    ) {
      reject('GitHub shorthand must use the exact owner/repository form')
    }
    return source
  }

  if (source.source !== 'git' && source.source !== 'url') {
    reject(
      `remote ingress marketplace/add does not accept '${source.source}' sources`,
    )
  }

  // The remote ingress protocol accepts a source string, not caller-supplied HTTP
  // headers. Reject a forged direct-runtime object instead of letting an exact
  // URL/host allowlist match authorize arbitrary credential or routing headers.
  if (source.source === 'url' && source.headers !== undefined) {
    reject('remote ingress URL sources must not include custom HTTP headers')
  }

  assertUnambiguousNetworkRaw(source.url)

  let parsed: URL
  try {
    parsed = new URL(source.url)
  } catch {
    reject('network sources must be valid HTTPS URLs')
  }
  if (parsed.protocol !== 'https:') {
    reject(
      'network sources must use HTTPS; SSH, HTTP, git://, and SCP are rejected',
    )
  }
  if (parsed.username.length > 0 || parsed.password.length > 0) {
    reject('HTTPS URLs must not contain embedded user credentials')
  }
  if (parsed.hostname.length === 0) {
    reject('HTTPS URLs must include a hostname')
  }

  if (source.source === 'git') {
    assertCanonicalGitRepositoryUrl(rawInput, source.url, parsed)
  }

  return { ...source, url: parsed.href }
}

function isExactFirstPartySource(source: MarketplaceSource): boolean {
  if (
    source.source !== 'github' ||
    source.ref !== undefined ||
    source.path !== undefined
  ) {
    return false
  }
  if (source.repo === OFFICIAL_MARKETPLACE_SOURCE.repo) {
    return source.sparsePaths === undefined
  }
  return source.repo === RECOMMENDED_MARKETPLACE_SOURCE.repo
}

async function canonicalizeLocalSource(
  rawInput: string,
  source: LocalMarketplaceIngressSource,
  context: MarketplaceIngressPolicyContext,
): Promise<{
  source: LocalMarketplaceIngressSource
  localIdentity: LocalMarketplaceIngressIdentity
}> {
  if (rawInput.includes('\0') || source.path.includes('\0')) {
    reject('local paths must not contain a NUL byte')
  }
  if (hasPortableTraversal(rawInput)) {
    reject("local path input must not contain a '..' segment")
  }

  const localIdentity = await captureLocalIdentityWithDependencies(source, {
    ...localIdentityDependencies(),
    homeDirectory: context.homeDirectory,
  })
  return {
    source: { ...source, path: localIdentity.canonicalTarget },
    localIdentity,
  }
}

async function validateWithContextAndCapture(
  rawInput: string,
  source: MarketplaceSource,
  context: MarketplaceIngressPolicyContext,
): Promise<MarketplaceIngressSourceValidation> {
  // Enterprise blocklist is checked before transport/default-trust decisions.
  if (isLocalSource(source)) {
    const trimmed = rawInput.trim()
    const expanded = trimmed.startsWith('~')
      ? `${context.homeDirectory}${trimmed.slice(1)}`
      : trimmed
    const lexicalSource = { ...source, path: resolve(expanded) }
    if (
      lexicalSource.path !== source.path &&
      context.isBlocked(lexicalSource)
    ) {
      reject(
        'the lexical local source is explicitly blocked by enterprise policy',
      )
    }
  }
  if (context.isBlocked(source)) {
    reject('the source is explicitly blocked by enterprise policy')
  }

  let checkedSource = source
  let localIdentity: LocalMarketplaceIngressIdentity | undefined
  let isNetworkSource = false
  if (isLocalSource(source)) {
    const localValidation = await canonicalizeLocalSource(
      rawInput,
      source,
      context,
    )
    checkedSource = localValidation.source
    localIdentity = localValidation.localIdentity
  } else {
    isNetworkSource = true
    checkedSource = canonicalizeNetworkSource(rawInput, source)
  }

  // Lexical and canonical sources can match different exact policy entries.
  // Either block wins; a strict network source must be allowed in both forms.
  if (context.isBlocked(checkedSource)) {
    reject('the canonical source is explicitly blocked by enterprise policy')
  }

  if (context.hasStrictKnownMarketplaces) {
    if (
      (isNetworkSource && !context.isAllowed(source)) ||
      !context.isAllowed(checkedSource)
    ) {
      reject('the source is not in strictKnownMarketplaces')
    }
    return { source: checkedSource, ...(localIdentity && { localIdentity }) }
  }

  if (isLocalSource(checkedSource) || isExactFirstPartySource(checkedSource)) {
    return { source: checkedSource, ...(localIdentity && { localIdentity }) }
  }

  reject(
    'custom network sources require an explicit strictKnownMarketplaces match',
  )
}

async function validateWithContext(
  rawInput: string,
  source: MarketplaceSource,
  context: MarketplaceIngressPolicyContext,
): Promise<MarketplaceSource> {
  return (await validateWithContextAndCapture(rawInput, source, context)).source
}

function currentPolicyContext(): MarketplaceIngressPolicyContext {
  const strictKnownMarketplaces = getStrictKnownMarketplaces()
  return {
    homeDirectory: localIdentityDependencies().homeDirectory,
    hasStrictKnownMarketplaces: strictKnownMarketplaces !== null,
    isBlocked: isSourceInBlocklist,
    isAllowed: isSourceAllowedByPolicy,
  }
}

/**
 * remote ingress-only marketplace/add ingress gate. Global CLI policy semantics stay
 * unchanged; the remote-client ingress boundary defaults to two exact first-party
 * GitHub sources or a canonical local path contained by the user home.
 */
export async function validateMarketplaceIngressSource(
  rawInput: string,
  source: MarketplaceSource,
): Promise<MarketplaceSource> {
  return validateWithContext(rawInput, source, currentPolicyContext())
}

/**
 * Manager-facing policy gate that returns the local identity captured by the
 * same validation pass. Network sources intentionally have no identity token.
 */
export async function validateAndCaptureMarketplaceIngressSource(
  rawInput: string,
  source: MarketplaceSource,
): Promise<MarketplaceIngressSourceValidation> {
  return validateWithContextAndCapture(rawInput, source, currentPolicyContext())
}

/** Test seam for policy behavior without mutating process-wide managed settings. */
export async function __validateMarketplaceIngressSourceForTest(
  rawInput: string,
  source: MarketplaceSource,
  context: MarketplaceIngressPolicyContext,
): Promise<MarketplaceSource> {
  return validateWithContext(rawInput, source, context)
}
