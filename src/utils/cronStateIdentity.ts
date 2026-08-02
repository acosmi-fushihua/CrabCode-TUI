import { createHash } from 'crypto'
import { existsSync, realpathSync } from 'fs'
import { homedir } from 'os'
import {
  basename,
  dirname,
  isAbsolute,
  join,
  normalize,
  sep,
} from 'path'

const DOMAIN = 'crabcode-cron-state-identity-v1'
export const CRON_STATE_IDENTITY_PREFIX = 'cron-state-v1:'

const LEGACY_STATE_DIR_NAMES = ['.clawdbot', '.moltbot', '.moldbot'] as const

function nonEmptyEnv(name: string): string | null {
  const value = process.env[name]?.trim()
  return value ? value : null
}

function resolveUserPath(value: string): string {
  const trimmed = value.trim()
  if (trimmed.startsWith('~')) {
    const home = nonEmptyEnv('CRABCODE_HOME') ?? homedir()
    const rest = trimmed.slice(1).replace(/^\//, '')
    return rest.length === 0 ? home : join(home, rest)
  }
  return trimmed
}

function normalizedProfileSuffix(): string {
  const profile = (process.env.CRABCODE_PROFILE ?? '').trim()
  if (profile.length === 0 || profile.toLowerCase() === 'default') return ''
  return `-${profile}`
}

/** Mirrors `acosmi_config::paths::resolve_state_dir` for cron IPC clients. */
export function resolveCronStateDir(): string {
  const configDir = nonEmptyEnv('CRABCODE_CONFIG_DIR')
  if (configDir) return resolveUserPath(configDir)

  const stateDir = nonEmptyEnv('CRABCODE_STATE_DIR')
  if (stateDir) return resolveUserPath(stateDir)

  const home = nonEmptyEnv('CRABCODE_HOME') ?? homedir()
  const suffix = normalizedProfileSuffix()
  const current = join(home, `.crabcode${suffix}`)
  if (suffix.length > 0 || existsSync(current)) return current

  for (const legacy of LEGACY_STATE_DIR_NAMES) {
    const candidate = join(home, legacy)
    if (existsSync(candidate)) return candidate
  }
  return current
}

/** Windows paths and pipe names compare case-insensitively. */
export function normalizeWindowsIdentityText(text: string): string {
  let normalized = text.replaceAll('\\', '/').toLowerCase()
  // Rust `std::fs::canonicalize` commonly emits the Win32 verbatim prefix;
  // Node's `realpathSync.native` may omit it. Strip it on both sides.
  if (normalized.startsWith('//?/unc/')) {
    normalized = `//${normalized.slice('//?/unc/'.length)}`
  } else if (normalized.startsWith('//?/')) {
    normalized = normalized.slice('//?/'.length)
  }
  return normalized
}

function platformIdentityText(text: string): string {
  return process.platform === 'win32'
    ? normalizeWindowsIdentityText(text)
    : text
}

function absoluteWithoutEarlyRealpath(path: string): string {
  if (isAbsolute(path)) return path
  return `${process.cwd()}${sep}${path}`
}

/**
 * Canonicalize symlinks when possible. For a nonexistent leaf, realpath the
 * deepest existing ancestor and append the normalized missing suffix. This is
 * the TypeScript mirror of daemon-launcher's v1 path contract.
 */
export function normalizeCronIdentityPath(path: string): string {
  const absolute = absoluteWithoutEarlyRealpath(path)
  try {
    return platformIdentityText(realpathSync.native(absolute))
  } catch {
    // Cold-start paths often do not exist yet. Normalize dot segments only
    // after the full realpath attempt, then search for a canonical ancestor.
  }

  const lexical = normalize(absolute)
  let probe = lexical
  const missing: string[] = []
  while (true) {
    try {
      const canonical = realpathSync.native(probe)
      return platformIdentityText(join(canonical, ...missing))
    } catch {
      const parent = dirname(probe)
      if (parent === probe) return platformIdentityText(lexical)
      missing.unshift(basename(probe))
      probe = parent
    }
  }
}

function normalizeTransportNamespace(transportNamespace: string): string {
  if (
    process.platform === 'win32' ||
    transportNamespace.startsWith('\\\\.\\pipe\\')
  ) {
    return normalizeWindowsIdentityText(transportNamespace)
  }
  return normalizeCronIdentityPath(transportNamespace)
}

/** Shared v1 hash/serialization primitive (pinned by a Rust+TS fixture). */
export function stateIdentityFromNormalized(
  statePath: string,
  transportNamespace: string,
): string {
  const state = Buffer.from(statePath, 'utf8')
  const transport = Buffer.from(transportNamespace, 'utf8')
  const stateLength = Buffer.alloc(8)
  const transportLength = Buffer.alloc(8)
  stateLength.writeBigUInt64BE(BigInt(state.length))
  transportLength.writeBigUInt64BE(BigInt(transport.length))

  const digest = createHash('sha256')
    .update(DOMAIN, 'utf8')
    .update(Buffer.from([0]))
    .update(stateLength)
    .update(state)
    .update(transportLength)
    .update(transport)
    .digest('hex')
  return `${CRON_STATE_IDENTITY_PREFIX}${digest}`
}

/** Expected identity for this process and a concrete cron endpoint. */
export function resolveCronStateIdentity(transportNamespace: string): string {
  const statePath = normalizeCronIdentityPath(resolveCronStateDir())
  const transport = normalizeTransportNamespace(transportNamespace)
  return stateIdentityFromNormalized(statePath, transport)
}
