import { isIP } from 'net'

/**
 * Shared SSRF address-classification core.
 *
 * Two consumers with different loopback policy share this one battle-tested
 * classifier:
 *
 *   - HTTP hooks (`src/utils/hooks/ssrfGuard.ts`) call it with
 *     `allowLoopback: true` — local dev policy servers are a primary hook use
 *     case, so 127.0.0.0/8 and ::1 are permitted.
 *   - WebFetch (`src/tools/WebFetchTool/utils.ts`) calls it with
 *     `allowLoopback: false` — the agent fetching `http://127.0.0.1` /
 *     `http://localhost` / cloud-metadata endpoints is an SSRF, so loopback is
 *     blocked.
 *
 * The classifier handles IPv4-mapped IPv6 (`::ffff:127.0.0.1`,
 * `::ffff:7f00:1`, expanded and partially-expanded forms), fc00::/7,
 * fe80::/10, CGNAT 100.64/10, and the WebFetch-only extra ranges
 * (documentation/benchmark/multicast). Hostnames are NOT resolved here — the
 * caller is responsible for running this on each address returned by
 * `dns.lookup`, or on an IP literal directly.
 */

export interface AddressClassificationOptions {
  /**
   * When true, loopback (127.0.0.0/8, ::1) is ALLOWED (returns false).
   * When false, loopback is BLOCKED (returns true). Defaults to false.
   */
  allowLoopback?: boolean
}

/**
 * Returns true if `address` (an IP literal) is in a range that should not be
 * reached. Non-IP input returns false (the caller has already validated via
 * isIP / dns.lookup, which only yields valid IP literals).
 */
export function isBlockedAddressCore(
  address: string,
  options: AddressClassificationOptions = {},
): boolean {
  const allowLoopback = options.allowLoopback ?? false
  const v = isIP(address)
  if (v === 4) {
    return isBlockedV4(address, allowLoopback)
  }
  if (v === 6) {
    return isBlockedV6(address, allowLoopback)
  }
  return false
}

function isBlockedV4(address: string, allowLoopback: boolean): boolean {
  const parts = address.split('.').map(Number)
  const [a, b] = parts
  if (
    parts.length !== 4 ||
    a === undefined ||
    b === undefined ||
    parts.some(n => Number.isNaN(n))
  ) {
    return false
  }

  // 127.0.0.0/8 — loopback. Allowed only when the caller opts in (hooks);
  // blocked for WebFetch.
  if (a === 127) return !allowLoopback

  // 0.0.0.0/8 — "this" network
  if (a === 0) return true
  // 10.0.0.0/8 — private
  if (a === 10) return true
  // 169.254.0.0/16 — link-local, cloud metadata
  if (a === 169 && b === 254) return true
  // 172.16.0.0/12 — private
  if (a === 172 && b >= 16 && b <= 31) return true
  // 100.64.0.0/10 — shared address space (RFC 6598, CGNAT). Some cloud
  // providers use this range for metadata endpoints (e.g. Alibaba Cloud at
  // 100.100.100.200).
  if (a === 100 && b >= 64 && b <= 127) return true
  // 192.168.0.0/16 — private
  if (a === 192 && b === 168) return true

  // WebFetch-only extra ranges (no legitimate fetch target lives here, and
  // they are common SSRF probe targets). Hooks historically did not block
  // these; gating them behind !allowLoopback keeps hooks byte-identical while
  // WebFetch (allowLoopback:false) gets the stricter set.
  if (!allowLoopback) {
    // 192.0.0.0/24 — IETF protocol assignments
    if (a === 192 && b === 0 && parts[2] === 0) return true
    // 192.0.2.0/24 — TEST-NET-1 (documentation)
    if (a === 192 && b === 0 && parts[2] === 2) return true
    // 198.18.0.0/15 — benchmark
    if (a === 198 && (b === 18 || b === 19)) return true
    // 198.51.100.0/24 — TEST-NET-2 (documentation)
    if (a === 198 && b === 51 && parts[2] === 100) return true
    // 203.0.113.0/24 — TEST-NET-3 (documentation)
    if (a === 203 && b === 0 && parts[2] === 113) return true
    // 224.0.0.0/4 — multicast; 240.0.0.0/4 — reserved; 255.255.255.255 broadcast
    if (a >= 224) return true
  }

  return false
}

function isBlockedV6(address: string, allowLoopback: boolean): boolean {
  const lower = address.toLowerCase()

  // ::1 loopback. Allowed only when the caller opts in (hooks).
  if (lower === '::1') return !allowLoopback

  // :: unspecified
  if (lower === '::') return true

  // IPv4-mapped IPv6 (0:0:0:0:0:ffff:X:Y in any representation — ::ffff:a.b.c.d,
  // ::ffff:XXXX:YYYY, expanded, or partially expanded). Extract the embedded
  // IPv4 address and delegate to the v4 check (which respects allowLoopback).
  // Without this, hex-form mapped addresses (e.g. ::ffff:a9fe:a9fe =
  // 169.254.169.254, or ::ffff:7f00:1 = 127.0.0.1) bypass the guard.
  const mappedV4 = extractMappedIPv4(lower)
  if (mappedV4 !== null) {
    return isBlockedV4(mappedV4, allowLoopback)
  }

  // fc00::/7 — unique local addresses (fc00:: through fdff::)
  if (lower.startsWith('fc') || lower.startsWith('fd')) {
    return true
  }

  // fe80::/10 — link-local. The /10 means fe80 through febf, but the first
  // hextet is always fe80 in practice (RFC 4291 requires the next 54 bits
  // to be zero). Check both to be safe.
  const firstHextet = lower.split(':')[0]
  if (
    firstHextet &&
    firstHextet.length === 4 &&
    firstHextet >= 'fe80' &&
    firstHextet <= 'febf'
  ) {
    return true
  }

  return false
}

/**
 * Expand `::` and optional trailing dotted-decimal so an IPv6 address is
 * represented as exactly 8 hex groups. Returns null if expansion is not
 * well-formed (the caller has already validated with isIP, so this is
 * defensive).
 */
function expandIPv6Groups(addr: string): number[] | null {
  // Handle trailing dotted-decimal IPv4 (e.g. ::ffff:169.254.169.254).
  // Replace it with its two hex groups so the rest of the expansion is uniform.
  let tailHextets: number[] = []
  if (addr.includes('.')) {
    const lastColon = addr.lastIndexOf(':')
    const v4 = addr.slice(lastColon + 1)
    addr = addr.slice(0, lastColon)
    const octets = v4.split('.').map(Number)
    if (
      octets.length !== 4 ||
      octets.some(n => !Number.isInteger(n) || n < 0 || n > 255)
    ) {
      return null
    }
    tailHextets = [
      (octets[0]! << 8) | octets[1]!,
      (octets[2]! << 8) | octets[3]!,
    ]
  }

  // Expand `::` (at most one) into the right number of zero groups.
  const dbl = addr.indexOf('::')
  let head: string[]
  let tail: string[]
  if (dbl === -1) {
    head = addr.split(':')
    tail = []
  } else {
    const headStr = addr.slice(0, dbl)
    const tailStr = addr.slice(dbl + 2)
    head = headStr === '' ? [] : headStr.split(':')
    tail = tailStr === '' ? [] : tailStr.split(':')
  }

  const target = 8 - tailHextets.length
  const fill = target - head.length - tail.length
  if (fill < 0) return null

  const hex = [...head, ...new Array<string>(fill).fill('0'), ...tail]
  const nums = hex.map(h => parseInt(h, 16))
  if (nums.some(n => Number.isNaN(n) || n < 0 || n > 0xffff)) {
    return null
  }
  nums.push(...tailHextets)
  return nums.length === 8 ? nums : null
}

/**
 * Extract the embedded IPv4 address from an IPv4-mapped IPv6 address
 * (0:0:0:0:0:ffff:X:Y) in any valid representation — compressed, expanded,
 * hex groups, or trailing dotted-decimal. Returns null if the address is
 * not an IPv4-mapped IPv6 address.
 */
function extractMappedIPv4(addr: string): string | null {
  const g = expandIPv6Groups(addr)
  if (!g) return null
  // IPv4-mapped: first 80 bits zero, next 16 bits ffff, last 32 bits = IPv4
  if (
    g[0] === 0 &&
    g[1] === 0 &&
    g[2] === 0 &&
    g[3] === 0 &&
    g[4] === 0 &&
    g[5] === 0xffff
  ) {
    const hi = g[6]!
    const lo = g[7]!
    return `${hi >> 8}.${hi & 0xff}.${lo >> 8}.${lo & 0xff}`
  }
  return null
}
