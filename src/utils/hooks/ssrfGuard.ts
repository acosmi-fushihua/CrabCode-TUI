import type { AddressFamily, LookupAddress as AxiosLookupAddress } from 'axios'
import { lookup as dnsLookup } from 'dns'
import { isIP } from 'net'
import { isBlockedAddressCore } from '../ssrf/index.js'

/**
 * SSRF guard for HTTP hooks.
 *
 * Blocks private, link-local, and other non-routable address ranges to prevent
 * project-configured HTTP hooks from reaching cloud metadata endpoints
 * (169.254.169.254) or internal infrastructure.
 *
 * Loopback (127.0.0.0/8, ::1) is intentionally ALLOWED — local dev policy
 * servers are a primary HTTP hook use case.
 *
 * When a global proxy or the sandbox network proxy is in use, the guard is
 * effectively bypassed for the target host because the proxy performs DNS
 * resolution. The sandbox proxy enforces its own domain allowlist.
 */

/**
 * Returns true if the address is in a range that HTTP hooks should not reach.
 *
 * Delegates to the shared `isBlockedAddressCore` with `allowLoopback: true`.
 * Hooks intentionally allow loopback (local-dev policy servers) and do NOT
 * apply the WebFetch-only extra ranges, so this preserves the historical
 * hooks behavior byte-identically.
 *
 * Blocked IPv4:
 *   0.0.0.0/8        "this" network
 *   10.0.0.0/8       private
 *   100.64.0.0/10    shared address space / CGNAT
 *   169.254.0.0/16   link-local (cloud metadata)
 *   172.16.0.0/12    private
 *   192.168.0.0/16   private
 *
 * Blocked IPv6:
 *   ::               unspecified
 *   fc00::/7         unique local
 *   fe80::/10        link-local
 *   ::ffff:<v4>      mapped IPv4 in a blocked range
 *
 * Allowed (returns false):
 *   127.0.0.0/8      loopback (local dev hooks)
 *   ::1              loopback
 *   everything else
 */
export function isBlockedAddress(address: string): boolean {
  return isBlockedAddressCore(address, { allowLoopback: true })
}

/**
 * A dns.lookup-compatible function that resolves a hostname and rejects
 * addresses in blocked ranges. Used as the `lookup` option in axios request
 * config so that the validated IP is the one the socket connects to — no
 * rebinding window between validation and connection.
 *
 * IP literals in the hostname are validated directly without DNS.
 *
 * Signature matches axios's `lookup` config option (not Node's dns.lookup).
 */
export function ssrfGuardedLookup(
  hostname: string,
  options: object,
  callback: (
    err: Error | null,
    address: AxiosLookupAddress | AxiosLookupAddress[],
    family?: AddressFamily,
  ) => void,
): void {
  const wantsAll = 'all' in options && options.all === true

  // If hostname is already an IP literal, validate it directly. dns.lookup
  // would short-circuit too, but checking here gives a clearer error and
  // avoids any platform-specific lookup behavior for literals.
  const ipVersion = isIP(hostname)
  if (ipVersion !== 0) {
    if (isBlockedAddress(hostname)) {
      callback(ssrfError(hostname, hostname), '')
      return
    }
    const family = ipVersion === 6 ? 6 : 4
    if (wantsAll) {
      callback(null, [{ address: hostname, family }])
    } else {
      callback(null, hostname, family)
    }
    return
  }

  dnsLookup(hostname, { all: true }, (err, addresses) => {
    if (err) {
      callback(err, '')
      return
    }

    for (const { address } of addresses) {
      if (isBlockedAddress(address)) {
        callback(ssrfError(hostname, address), '')
        return
      }
    }

    const first = addresses[0]
    if (!first) {
      callback(
        Object.assign(new Error(`ENOTFOUND ${hostname}`), {
          code: 'ENOTFOUND',
          hostname,
        }),
        '',
      )
      return
    }

    const family = first.family === 6 ? 6 : 4
    if (wantsAll) {
      callback(
        null,
        addresses.map(a => ({
          address: a.address,
          family: a.family === 6 ? 6 : 4,
        })),
      )
    } else {
      callback(null, first.address, family)
    }
  })
}

function ssrfError(hostname: string, address: string): NodeJS.ErrnoException {
  const err = new Error(
    `HTTP hook blocked: ${hostname} resolves to ${address} (private/link-local address). Loopback (127.0.0.1, ::1) is allowed for local dev.`,
  )
  return Object.assign(err, {
    code: 'ERR_HTTP_HOOK_BLOCKED_ADDRESS',
    hostname,
    address,
  })
}
