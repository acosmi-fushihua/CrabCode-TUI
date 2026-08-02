/**
 * Domain filter shared by all search providers — TS port of Go
 * services/gateway/search/util.go.
 */

import type { SearchOptions } from './types.js'

/**
 * Returns true when the URL is allowed under the blocked-domains list.
 * - Empty / unparsable URL → blocked
 * - No blocklist → allowed
 * - Hostname suffix-matches any blocked entry → blocked
 *
 * Matches the Go implementation's strings.HasSuffix semantics — callers that
 * want exact-match should pass the full hostname including any leading dot.
 */
export function filterDomainAllowed(
  rawURL: string,
  opts: Pick<SearchOptions, 'blockedDomains'>,
): boolean {
  if (!rawURL) return false
  const blocked = opts.blockedDomains
  if (!blocked || blocked.length === 0) return true
  let hostname: string
  try {
    hostname = new URL(rawURL).hostname
  } catch {
    // Unparseable URL → blocked (fail-closed). Callers pass real result URLs
    // from search providers; an unparseable one can't be opened anyway, and
    // returning `true` here would fail OPEN past the blocked-domains list.
    return false
  }
  for (const d of blocked) {
    if (hostname.endsWith(d)) return false
  }
  return true
}
