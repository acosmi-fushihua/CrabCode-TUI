/**
 * Domain check service — TS port of Go services/gateway/search/domain.go.
 *
 * After the Go cut, WebFetchTool calls this directly instead of routing
 * through the Go hub's domain.check IPC handler. Behavior is preserved:
 *   - 5s timeout
 *   - GET {CRABCODE_DOMAIN_CHECK_URL}?domain={escaped}
 *   - Response { can_fetch: bool|null, reason?: string }
 *   - Fail-closed: transport/non-200/decode/indeterminate → throws
 *     (the caller, checkDomainBlocklist, turns the throw into a 'check_failed'
 *     status so the fetch is refused rather than silently allowed)
 */

import { createCombinedAbortSignal } from '../../utils/combinedAbortSignal.js'

const DOMAIN_CHECK_TIMEOUT_MS = 5_000

export interface DomainCheckResult {
  allowed: boolean
  reason?: string
}

export async function checkDomain(
  domain: string,
  signal?: AbortSignal,
): Promise<DomainCheckResult> {
  const checkURL = process.env.CRABCODE_DOMAIN_CHECK_URL
  if (!checkURL) return { allowed: true }

  const reqURL = `${checkURL}?domain=${encodeURIComponent(domain)}`
  const { signal: composedSignal, cleanup } = createCombinedAbortSignal(
    signal,
    { timeoutMs: DOMAIN_CHECK_TIMEOUT_MS },
  )

  try {
    let resp: Response
    try {
      resp = await fetch(reqURL, { method: 'GET', signal: composedSignal })
    } catch (e) {
      // Fail-closed: transport error must propagate, not be treated as allowed.
      throw new Error(
        `domain check transport error: ${e instanceof Error ? e.message : String(e)}`,
      )
    }

    if (!resp.ok) {
      // Fail-closed: a non-200 from the check service is not a green light.
      throw new Error(`domain check returned HTTP ${resp.status}`)
    }

    let data: { can_fetch?: boolean | null; reason?: string }
    try {
      data = (await resp.json()) as typeof data
    } catch (e) {
      // Fail-closed: an undecodable response cannot be interpreted as allowed.
      throw new Error(
        `domain check decode error: ${e instanceof Error ? e.message : String(e)}`,
      )
    }

    if (data.can_fetch == null) {
      // Fail-closed: indeterminate verdict (null) is not an allow.
      throw new Error('domain check returned indeterminate verdict (can_fetch=null)')
    }

    return { allowed: !!data.can_fetch, reason: data.reason }
  } finally {
    cleanup()
  }
}

/** Domain check is configured when the env var is set. */
export function isDomainCheckConfigured(): boolean {
  return !!process.env.CRABCODE_DOMAIN_CHECK_URL
}
