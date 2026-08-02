/**
 * W-WEBFETCH-FAKEIP-SSRF (2026-07-24) — fake-ip DNS environment detection.
 *
 * Proxy clients in fake-ip DNS mode (Clash/Mihomo/Surge/sing-box TUN) answer
 * nearly every DNS query with a placeholder address from 198.18.0.0/15 (the
 * RFC 2544 benchmark range) and route the subsequent TCP connect by hostname
 * inside the TUN. On such machines the WebFetch connect-time SSRF guard would
 * reject every resolved address as "benchmark range" even though connecting to
 * the placeholder is exactly how this machine reaches the internet — the
 * false rejection even though the placeholder is how the machine reaches the
 * internet.
 *
 * Detection = EITHER branch:
 *   ① interface branch — a local interface address sits in 198.18.0.0/15
 *     (Clash-family TUN defaults to 198.18.0.1);
 *   ② canary branch — resolving example.com / example.org lands in
 *     198.18.0.0/15 (covers sing-box-style setups whose TUN interface address
 *     is OUTSIDE the range while the fake-ip pool is inside; public DNS never
 *     resolves real sites into the reserved benchmark range, so the
 *     false-positive surface is zero).
 *
 * Contract (audit §7.1 F1): lazy — callers probe only when the guard hits a
 * 198.18/15 address; memoized with a 60s TTL; canary lookups time out at 2s;
 * any probe exception ⇒ false (fail-safe: the guard keeps blocking).
 */
import { lookup as dnsLookup } from 'dns'
import { isIP } from 'net'
import { networkInterfaces } from 'os'

/**
 * 198.18.0.0/15 — the fake-ip placeholder pool (= RFC 2544 benchmark range,
 * the "WebFetch-only extra range" blocked at src/utils/ssrf/index.ts). IPv4
 * only: fake-ip pools hand out dotted-quad v4 placeholders; every non-v4
 * input (IPv6, mapped forms, garbage) is NOT eligible for the allowance.
 */
export function isFakeIpBenchmarkAddress(address: string): boolean {
  if (isIP(address) !== 4) return false
  const parts = address.split('.')
  const a = Number(parts[0])
  const b = Number(parts[1])
  return a === 198 && (b === 18 || b === 19)
}

type CanaryLookupFn = (
  hostname: string,
  options: { all: true },
  callback: (
    err: NodeJS.ErrnoException | null,
    addresses: { address: string; family: number }[],
  ) => void,
) => void

export type FakeIpEnvProbeDeps = {
  /** Test seam — defaults to os.networkInterfaces. */
  interfaces?: () => ReturnType<typeof networkInterfaces>
  /** Test seam — defaults to dns.lookup. */
  lookup?: CanaryLookupFn
  /** Test seam — defaults to Date.now. */
  now?: () => number
  /** Test seam — defaults to CANARY_TIMEOUT_MS. */
  canaryTimeoutMs?: number
}

/** Fixed public canary hosts — see module doc for why they cannot misfire. */
const CANARY_HOSTNAMES = ['example.com', 'example.org'] as const
const CANARY_TIMEOUT_MS = 2_000
const CACHE_TTL_MS = 60_000

let cached: { value: boolean; expiresAt: number } | null = null
let inflight: Promise<boolean> | null = null
let testOverride: boolean | null = null

/** Force the detection outcome (guard-level tests). null restores real probing. */
export function __setFakeIpDnsEnvironmentOverrideForTests(
  value: boolean | null,
): void {
  testOverride = value
}

export function __resetFakeIpDnsEnvironmentCacheForTests(): void {
  cached = null
  inflight = null
  testOverride = null
}

/**
 * Whether this machine's DNS is owned by a fake-ip proxy client. Memoized
 * (60s TTL) and in-flight-deduplicated so a burst of parallel WebFetch calls
 * triggers at most one probe.
 */
export function isFakeIpDnsEnvironment(
  deps: FakeIpEnvProbeDeps = {},
): Promise<boolean> {
  if (testOverride !== null) return Promise.resolve(testOverride)
  const now = deps.now?.() ?? Date.now()
  if (cached && now < cached.expiresAt) return Promise.resolve(cached.value)
  if (inflight) return inflight
  const probe = probeFakeIpDnsEnvironment(deps).finally(() => {
    inflight = null
  })
  inflight = probe
  return probe
}

async function probeFakeIpDnsEnvironment(
  deps: FakeIpEnvProbeDeps,
): Promise<boolean> {
  // Each branch fails safe independently: an exotic throw in interface
  // enumeration must not silence the canary branch, and vice versa.
  let value = false
  try {
    value = interfaceInFakeIpRange(deps)
  } catch {
    value = false
  }
  if (!value) {
    try {
      value = await anyCanaryLandsInFakeIpRange(deps)
    } catch {
      value = false
    }
  }
  cached = { value, expiresAt: (deps.now?.() ?? Date.now()) + CACHE_TTL_MS }
  return value
}

function interfaceInFakeIpRange(deps: FakeIpEnvProbeDeps): boolean {
  const interfaces = (deps.interfaces ?? networkInterfaces)()
  return Object.values(interfaces).some(list =>
    (list ?? []).some(entry => isFakeIpBenchmarkAddress(entry.address)),
  )
}

async function anyCanaryLandsInFakeIpRange(
  deps: FakeIpEnvProbeDeps,
): Promise<boolean> {
  // dns.lookup's overloads widen the callback param; the {all:true} call shape
  // always yields LookupAddress[] — the cast pins that single shape.
  const lookup = deps.lookup ?? (dnsLookup as unknown as CanaryLookupFn)
  const timeoutMs = deps.canaryTimeoutMs ?? CANARY_TIMEOUT_MS
  const results = await Promise.all(
    CANARY_HOSTNAMES.map(host =>
      canaryLandsInFakeIpRange(host, lookup, timeoutMs),
    ),
  )
  return results.some(Boolean)
}

function canaryLandsInFakeIpRange(
  host: string,
  lookup: CanaryLookupFn,
  timeoutMs: number,
): Promise<boolean> {
  return new Promise(resolve => {
    let settled = false
    const settle = (value: boolean) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve(value)
    }
    // Deliberately NOT unref'd: an unref'd timer never fires when it is the
    // loop's only pending work (Bun), which would wedge the awaiting guard
    // forever. The timer is short (≤2s) and always cleared on settle, so the
    // worst case is holding process exit for one canary window.
    const timer = setTimeout(() => settle(false), timeoutMs)
    try {
      lookup(host, { all: true }, (err, addresses) => {
        if (err || !Array.isArray(addresses)) {
          settle(false)
          return
        }
        settle(addresses.some(entry => isFakeIpBenchmarkAddress(entry.address)))
      })
    } catch {
      settle(false)
    }
  })
}
