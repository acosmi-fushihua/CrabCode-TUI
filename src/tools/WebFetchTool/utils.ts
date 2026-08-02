import axios, { type AxiosResponse } from 'axios'
import { LRUCache } from 'lru-cache'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../../services/analytics/index.js'
import { queryFastMode } from '../../services/api/crabcode.js'
import { AbortError } from '../../utils/errors.js'
import { getWebFetchUserAgent } from '../../utils/http.js'
import { logError } from '../../utils/log.js'
import {
  isBinaryContentType,
  persistBinaryContent,
} from '../../utils/mcpOutputStorage.js'
import { getSettings_DEPRECATED } from '../../utils/settings/settings.js'
import { asSystemPrompt } from '../../utils/systemPromptType.js'
import { isPreapprovedHost } from './preapproved.js'
import { makeSecondaryModelPrompt } from './prompt.js'
// Local SSRF guard (P1-5) dependencies. Placed after the original import block
// so the W-APP-SERVER-UNIFIED-COMM audit allowlist keys (which pin
// queryFastMode to its line) stay stable.
import type { AddressFamily, LookupAddress as AxiosLookupAddress } from 'axios'
import { lookup as dnsLookup } from 'dns'
import { isIP } from 'net'
import { logForDebugging } from '../../utils/debug.js'
import { getProxyUrl, shouldBypassProxy } from '../../utils/proxy.js'
import {
  isFakeIpBenchmarkAddress,
  isFakeIpDnsEnvironment,
} from '../../utils/ssrf/fakeIpEnv.js'
import { isBlockedAddressCore } from '../../utils/ssrf/index.js'

// Custom error classes for domain blocking
class DomainBlockedError extends Error {
  constructor(domain: string) {
    super(`CrabCode is unable to fetch from ${domain}`)
    this.name = 'DomainBlockedError'
  }
}

class DomainCheckFailedError extends Error {
  constructor(domain: string) {
    super(
      `Unable to verify if domain ${domain} is safe to fetch. This may be due to network restrictions or enterprise security policies blocking acosmi.com.`,
    )
    this.name = 'DomainCheckFailedError'
  }
}

class EgressBlockedError extends Error {
  constructor(public readonly domain: string) {
    super(
      JSON.stringify({
        error_type: 'EGRESS_BLOCKED',
        domain,
        message: `Access to ${domain} is blocked by the network egress proxy.`,
      }),
    )
    this.name = 'EgressBlockedError'
  }
}

// Cache for storing fetched URL content
type CacheEntry = {
  bytes: number
  code: number
  codeText: string
  content: string
  contentType: string
  persistedPath?: string
  persistedSize?: number
}

// Cache with 15-minute TTL and 50MB size limit
// LRUCache handles automatic expiration and eviction
const CACHE_TTL_MS = 15 * 60 * 1000 // 15 minutes
const MAX_CACHE_SIZE_BYTES = 50 * 1024 * 1024 // 50MB

const URL_CACHE = new LRUCache<string, CacheEntry>({
  maxSize: MAX_CACHE_SIZE_BYTES,
  ttl: CACHE_TTL_MS,
})

// Separate cache for preflight domain checks. URL_CACHE is URL-keyed, so
// fetching two paths on the same domain triggers two identical preflight
// HTTP round-trips to acosmi.com/api/v4. This hostname-keyed cache avoids
// that. Only 'allowed' is cached — blocked/failed re-check on next attempt.
const DOMAIN_CHECK_CACHE = new LRUCache<string, true>({
  max: 128,
  ttl: 5 * 60 * 1000, // 5 minutes — shorter than URL_CACHE TTL
})

export function clearWebFetchCache(): void {
  URL_CACHE.clear()
  DOMAIN_CHECK_CACHE.clear()
}

// Lazy singleton — defers the turndown → @mixmark-io/domino import (~1.4MB
// retained heap) until the first HTML fetch, and reuses one instance across
// calls (construction builds 15 rule objects; .turndown() is stateless).
// @types/turndown ships only `export =` (no .d.mts), so TS types the import
// as the class itself while Bun wraps CJS in { default } — hence the cast.
type TurndownCtor = typeof import('turndown')
let turndownServicePromise: Promise<InstanceType<TurndownCtor>> | undefined
function getTurndownService(): Promise<InstanceType<TurndownCtor>> {
  return (turndownServicePromise ??= import('turndown').then(m => {
    const Turndown = (m as unknown as { default: TurndownCtor }).default
    return new Turndown()
  }))
}

// PSR requested limiting the length of URLs to 250 to lower the potential
// for a data exfiltration. However, this is too restrictive for some customers'
// legitimate use cases, such as JWT-signed URLs (e.g., cloud service signed URLs)
// that can be much longer. We already require user approval for each domain,
// which provides a primary security boundary. In addition, CrabCode has
// other data exfil channels, and this one does not seem relatively high risk,
// so I'm removing that length restriction. -ab
const MAX_URL_LENGTH = 2000

// Per PSR:
// "Implement resource consumption controls because setting limits on CPU,
// memory, and network usage for the Web Fetch tool can prevent a single
// request or user from overwhelming the system."
const MAX_HTTP_CONTENT_LENGTH = 10 * 1024 * 1024

// Timeout for the main HTTP fetch request (60 seconds).
// Prevents hanging indefinitely on slow/unresponsive servers.
const FETCH_TIMEOUT_MS = 60_000

// Timeout for the domain blocklist preflight check (10 seconds).
const DOMAIN_CHECK_TIMEOUT_MS = 10_000

// Cap same-host redirect hops. Without this a malicious server can return
// a redirect loop (/a → /b → /a …) and the per-request FETCH_TIMEOUT_MS
// resets on every hop, hanging the tool until user interrupt. 10 matches
// common client defaults (axios=5, follow-redirects=21, Chrome=20).
const MAX_REDIRECTS = 10

// Truncate to not spend too many tokens
export const MAX_MARKDOWN_LENGTH = 100_000

export function isPreapprovedUrl(url: string): boolean {
  try {
    const parsedUrl = new URL(url)
    return isPreapprovedHost(parsedUrl.hostname, parsedUrl.pathname)
  } catch {
    return false
  }
}

// ---------------------------------------------------------------------------
// Local SSRF guard (P1-5)
//
// WebFetch must NOT reach loopback, private, link-local, cloud-metadata, or
// other non-routable targets. Unlike HTTP hooks (which allow loopback for
// local-dev policy servers), WebFetch blocks loopback too — a model fetching
// http://127.0.0.1 / http://localhost / http://169.254.169.254 is an SSRF.
//
// Defense in depth:
//   1. DNS-free pre-checks (protocol, credentials, metadata hostnames, IP
//      literals) run in validateURL / assertWebFetchUrlAllowed — these apply
//      even when a proxy is active (so HTTP_PROXY users still get protection).
//   2. webFetchGuardedLookup runs at axios connect time and rejects any
//      resolved address in a blocked range — closing the DNS-rebinding window.
//      It is only wired when NO proxy is active (a proxy resolves DNS itself,
//      so a lookup-based guard would validate the proxy IP, not the target;
//      mirrors src/utils/hooks/execHttpHook.ts).
// ---------------------------------------------------------------------------

const BLOCKED_METADATA_HOSTNAMES = new Set<string>([
  'metadata.google.internal',
  'metadata.goog',
  'metadata.amazonaws.com',
  'localhost',
])

/** WebFetch address classification: loopback is BLOCKED (allowLoopback:false). */
export function isWebFetchBlockedAddress(address: string): boolean {
  return isBlockedAddressCore(address, { allowLoopback: false })
}

/**
 * Block well-known cloud-metadata hostnames and `localhost` / `*.localhost`.
 * These resolve to loopback/metadata addresses but the string check catches
 * them DNS-free (and before any resolver that might special-case `localhost`).
 */
export function isBlockedMetadataHostname(hostname: string): boolean {
  const h = hostname.toLowerCase().replace(/\.$/, '')
  if (BLOCKED_METADATA_HOSTNAMES.has(h)) return true
  // *.localhost (RFC 6761 reserves the whole TLD to loopback)
  if (h === 'localhost' || h.endsWith('.localhost')) return true
  return false
}

/**
 * DNS-free pre-checks. Throws if the URL is not allowed for WebFetch:
 *   - protocol must be http: or https:
 *   - no embedded credentials
 *   - hostname must not be a blocked metadata hostname / localhost
 *   - if the host is an IP literal, it must not be in a blocked range
 *
 * Returns the parsed URL on success. Hostname (non-literal) DNS resolution is
 * NOT done here — that is the job of webFetchGuardedLookup at connect time.
 */
export function assertWebFetchUrlAllowed(url: string): URL {
  let parsed: URL
  try {
    parsed = new URL(url)
  } catch {
    throw new Error('Invalid URL')
  }

  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw new Error(
      `Blocked URL scheme "${parsed.protocol}" — only http and https are allowed`,
    )
  }

  if (parsed.username || parsed.password) {
    throw new Error('Blocked URL with embedded credentials')
  }

  // URL.hostname KEEPS the brackets on IPv6 literals (e.g. [::1]); strip them
  // so isIP / the classifier see a bare address. The WHATWG parser also
  // canonicalizes obfuscated IPv4 (octal/hex/decimal → dotted-decimal) and
  // IPv4-mapped IPv6 (::ffff:127.0.0.1 → ::ffff:7f00:1) for us.
  const hostname = parsed.hostname
  const bareHost = hostname.replace(/^\[|\]$/g, '')

  if (isBlockedMetadataHostname(bareHost)) {
    throw new Error(
      `Blocked local/metadata hostname "${hostname}" (SSRF protection)`,
    )
  }

  // If the host is an IP literal, validate it directly (DNS-free). This
  // catches octal/hex/decimal/dotted IPv4 and IPv6 literals up front so the
  // literal cases are blocked even when a proxy is active (no lookup runs).
  if (isIP(bareHost) !== 0 && isWebFetchBlockedAddress(bareHost)) {
    throw new Error(
      `Blocked local/private address "${hostname}" (SSRF protection)`,
    )
  }

  return parsed
}

type GuardedLookupCallback = (
  err: Error | null,
  address: AxiosLookupAddress | AxiosLookupAddress[],
  family?: AddressFamily,
) => void

/**
 * An axios `lookup`-compatible function for WebFetch. Resolves the hostname
 * and rejects if ANY resolved address is in a blocked range (loopback
 * included). Returns only the checked addresses to axios, minimizing the
 * DNS-rebinding window. IP literals are validated without DNS.
 */
export function webFetchGuardedLookup(
  hostname: string,
  options: object,
  callback: GuardedLookupCallback,
): void {
  const wantsAll = 'all' in options && options.all === true

  const ipVersion = isIP(hostname)
  if (ipVersion !== 0) {
    // IP literals NEVER get the fake-ip allowance below: the TUN cannot
    // reverse-map a literal placeholder to a domain, and a literal benchmark
    // IP has no legitimate fetch target (audit §7.1 F1 — 字面量恒拒).
    if (isWebFetchBlockedAddress(hostname)) {
      callback(webFetchSsrfError(hostname, hostname), '')
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
    void deliverGuardedLookupResult(hostname, addresses, wantsAll, callback)
  })
}

/**
 * DNS-free core of the connect-time guard: classifies the RESOLVED addresses
 * and delivers them (or a rejection) to the axios lookup callback. Split out
 * of webFetchGuardedLookup so the deterministic part is unit-testable without
 * stubbing DNS.
 *
 * Fake-ip allowance (W-WEBFETCH-FAKEIP-SSRF F1, 2026-07-24 审计): a resolved
 * address in 198.18.0.0/15 while the machine is confirmed to run fake-ip DNS
 * is a placeholder owned by the proxy client's TUN — connecting to it reaches
 * the user's own proxy (which routes by hostname), not a real benchmark-range
 * host, so it is allowed through. The allowance covers ONLY that range and
 * ONLY resolved (never literal) addresses; loopback/private/link-local and
 * every other blocked range keep rejecting, and a single non-allowed blocked
 * address still rejects the whole lookup.
 */
export async function deliverGuardedLookupResult(
  hostname: string,
  addresses: { address: string; family: number }[],
  wantsAll: boolean,
  callback: GuardedLookupCallback,
): Promise<void> {
  try {
    for (const { address } of addresses) {
      if (!isWebFetchBlockedAddress(address)) continue
      if (isFakeIpBenchmarkAddress(address) && (await isFakeIpDnsEnvironment())) {
        logForDebugging(
          `WebFetch fake-ip allowance: ${hostname} resolved to placeholder ${address}; fake-ip DNS environment confirmed, connection proceeds via the proxy TUN`,
        )
        continue
      }
      callback(
        webFetchSsrfError(hostname, address, {
          fakeIpHint: isFakeIpBenchmarkAddress(address),
        }),
        '',
      )
      return
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
  } catch (error) {
    // Defensive: the guard must always answer the callback exactly once —
    // an unexpected throw would otherwise hang axios until its timeout.
    // Nothing after a callback invocation above can throw, so reaching here
    // implies no callback has fired yet.
    callback(error as Error, '')
  }
}

/**
 * 终态签名 producer（`web_fetch_ssrf_blocked`）：判据 =
 * WEB_FETCH_SSRF_BLOCKED_PREFIX 前缀，文案即契约——改前缀必须同步
 * `src/services/tools/terminalToolError.ts` 签名表 +
 * `tests/unit/terminal-tool-error-classification.test.ts`。前缀恒定，
 * 后缀（fake-ip 指引）可扩不破约。
 */
export const WEB_FETCH_SSRF_BLOCKED_PREFIX = 'WebFetch blocked:'

// Appended ONLY when the rejected address sits in 198.18.0.0/15 but the
// fake-ip environment probe did NOT confirm (probe true never produces this
// error for resolved addresses). Gives the user the three actionable exits.
const FAKE_IP_HINT =
  ' The address is in 198.18.0.0/15, the placeholder range used by proxy clients in fake-ip DNS mode, but a fake-ip environment could not be confirmed on this machine. If this host should be reachable: switch the proxy client DNS enhanced-mode from fake-ip to redir-host, add the domain to its fake-ip-filter, or start the session with an explicit proxy (https_proxy plus NO_PROXY=localhost,127.0.0.1,::1).'

function webFetchSsrfError(
  hostname: string,
  address: string,
  options: { fakeIpHint?: boolean } = {},
): NodeJS.ErrnoException {
  const err = new Error(
    `${WEB_FETCH_SSRF_BLOCKED_PREFIX} ${hostname} resolves to ${address} (local/private/link-local address; SSRF protection).${options.fakeIpHint ? FAKE_IP_HINT : ''}`,
  )
  return Object.assign(err, {
    code: 'ERR_WEB_FETCH_BLOCKED_ADDRESS',
    hostname,
    address,
  })
}

export function validateURL(url: string): boolean {
  if (url.length > MAX_URL_LENGTH) {
    return false
  }

  let parsed
  try {
    parsed = new URL(url)
  } catch {
    return false
  }

  // Local SSRF guard (P1-5): enforce the DNS-free checks here so that
  // disallowed protocols (only http/https permitted), embedded credentials,
  // metadata hostnames, and blocked IP literals are rejected up front — even
  // when no DNS lookup will run (e.g. a proxy is active). Hostname resolution
  // itself is deferred to webFetchGuardedLookup at connect time.
  try {
    assertWebFetchUrlAllowed(url)
  } catch {
    return false
  }

  // Initial filter that this isn't a privileged, company-internal URL
  // by checking that the hostname is publicly resolvable
  const hostname = parsed.hostname
  const parts = hostname.split('.')
  if (parts.length < 2) {
    return false
  }

  return true
}

type DomainCheckResult =
  | { status: 'allowed' }
  | { status: 'blocked' }
  | { status: 'check_failed'; error: Error }

/**
 * Built-in domain blocklist for known malicious/high-risk domains.
 * This replaces the external acosmi.com domain_info API call with a local check.
 * The blocklist can be updated in releases without requiring network access.
 */
const BUILTIN_BLOCKED_DOMAINS = new Set<string>([
  // Placeholder — populated from upstream sync in future releases
])

/**
 * Check whether a domain is allowed for fetching.
 *
 * Priority:
 * 1. Domain check cache → allowed (fast path)
 * 2. Built-in blocklist → blocked (pure local, no network)
 * 3. External domain check (if CRABCODE_DOMAIN_CHECK_URL configured) → per response
 * 4. Default → allowed (open policy)
 */
export async function checkDomainBlocklist(
  domain: string,
): Promise<DomainCheckResult> {
  if (DOMAIN_CHECK_CACHE.has(domain)) {
    return { status: 'allowed' }
  }

  // 1. Built-in blocklist (pure local check)
  if (BUILTIN_BLOCKED_DOMAINS.has(domain)) {
    return { status: 'blocked' }
  }

  // 2. External check via in-process domain check service (if configured)
  if (process.env.CRABCODE_DOMAIN_CHECK_URL) {
    try {
      const { checkDomain } = await import('../../services/domain/domain.js')
      const result = await checkDomain(domain)
      if (result.allowed) {
        DOMAIN_CHECK_CACHE.set(domain, true)
        return { status: 'allowed' }
      }
      return { status: 'blocked' }
    } catch (e) {
      logError(e)
      // Domain check error — fail-CLOSED. When an external check is configured
      // and the check itself fails (transport / non-200 / decode / indeterminate
      // can_fetch==null, all surfaced as throws by checkDomain), we must not
      // silently allow the fetch. Surface 'check_failed' so getURLMarkdownContent
      // throws DomainCheckFailedError instead of proceeding.
      return { status: 'check_failed', error: e as Error }
    }
  }

  // 3. Default: allow (open policy when no external check configured)
  DOMAIN_CHECK_CACHE.set(domain, true)
  return { status: 'allowed' }
}

/**
 * Check if a redirect is safe to follow
 * Allows redirects that:
 * - Add or remove "www." in the hostname
 * - Keep the origin the same but change path/query params
 * - Or both of the above
 */
export function isPermittedRedirect(
  originalUrl: string,
  redirectUrl: string,
): boolean {
  try {
    const parsedOriginal = new URL(originalUrl)
    const parsedRedirect = new URL(redirectUrl)

    if (parsedRedirect.protocol !== parsedOriginal.protocol) {
      return false
    }

    if (parsedRedirect.port !== parsedOriginal.port) {
      return false
    }

    if (parsedRedirect.username || parsedRedirect.password) {
      return false
    }

    // Now check hostname conditions
    // 1. Adding www. is allowed: example.com -> www.example.com
    // 2. Removing www. is allowed: www.example.com -> example.com
    // 3. Same host (with or without www.) is allowed: paths can change
    const stripWww = (hostname: string) => hostname.replace(/^www\./, '')
    const originalHostWithoutWww = stripWww(parsedOriginal.hostname)
    const redirectHostWithoutWww = stripWww(parsedRedirect.hostname)
    return originalHostWithoutWww === redirectHostWithoutWww
  } catch (_error) {
    return false
  }
}

/**
 * Helper function to handle fetching URLs with custom redirect handling
 * Recursively follows redirects if they pass the redirectChecker function
 *
 * Per PSR:
 * "Do not automatically follow redirects because following redirects could
 * allow for an attacker to exploit an open redirect vulnerability in a
 * trusted domain to force a user to make a request to a malicious domain
 * unknowingly"
 */
type RedirectInfo = {
  type: 'redirect'
  originalUrl: string
  redirectUrl: string
  statusCode: number
}

export async function getWithPermittedRedirects(
  url: string,
  signal: AbortSignal,
  redirectChecker: (originalUrl: string, redirectUrl: string) => boolean,
  depth = 0,
): Promise<AxiosResponse<ArrayBuffer> | RedirectInfo> {
  if (depth > MAX_REDIRECTS) {
    throw new Error(`Too many redirects (exceeded ${MAX_REDIRECTS})`)
  }

  // Local SSRF guard wiring (P1-5). The lookup-based guard validates the
  // resolved IP at connect time, but it is SILENTLY DISABLED when a proxy is
  // active (the proxy resolves DNS, so `lookup` would validate the proxy IP,
  // not the target). Mirror src/utils/hooks/execHttpHook.ts: skip `lookup`
  // under a proxy, but the DNS-free IP-literal/metadata pre-checks in
  // validateURL/assertWebFetchUrlAllowed still apply, so HTTP_PROXY users are
  // not left with zero protection. Each axios call re-runs `lookup`, so the
  // recursive same-host redirect below is re-validated automatically.
  const envProxyActive =
    getProxyUrl() !== undefined && !shouldBypassProxy(url)

  try {
    return await axios.get(url, {
      signal,
      timeout: FETCH_TIMEOUT_MS,
      maxRedirects: 0,
      responseType: 'arraybuffer',
      maxContentLength: MAX_HTTP_CONTENT_LENGTH,
      // Explicit false prevents axios's own env-var proxy detection; when an
      // env-var proxy is configured, the global axios interceptor installed by
      // configureGlobalAgents() handles it via httpsAgent instead.
      proxy: false,
      // Skip the lookup guard when a proxy is active (it would validate the
      // proxy IP, not the target). DNS-free pre-checks in validateURL still
      // protect IP-literal / metadata targets in that case.
      lookup: envProxyActive ? undefined : webFetchGuardedLookup,
      headers: {
        Accept: 'text/markdown, text/html, */*',
        'User-Agent': getWebFetchUserAgent(),
      },
    })
  } catch (error) {
    if (
      axios.isAxiosError(error) &&
      error.response &&
      [301, 302, 307, 308].includes(error.response.status)
    ) {
      const redirectLocation = error.response.headers.location
      if (!redirectLocation) {
        throw new Error('Redirect missing Location header')
      }

      // Resolve relative URLs against the original URL
      const redirectUrl = new URL(redirectLocation, url).toString()

      if (redirectChecker(url, redirectUrl)) {
        // Recursively follow the permitted redirect
        return getWithPermittedRedirects(
          redirectUrl,
          signal,
          redirectChecker,
          depth + 1,
        )
      } else {
        // Return redirect information to the caller
        return {
          type: 'redirect',
          originalUrl: url,
          redirectUrl,
          statusCode: error.response.status,
        }
      }
    }

    // Detect egress proxy blocks: the proxy returns 403 with
    // X-Proxy-Error: blocked-by-allowlist when egress is restricted
    if (
      axios.isAxiosError(error) &&
      error.response?.status === 403 &&
      error.response.headers['x-proxy-error'] === 'blocked-by-allowlist'
    ) {
      const hostname = new URL(url).hostname
      throw new EgressBlockedError(hostname)
    }

    throw error
  }
}

function isRedirectInfo(
  response: AxiosResponse<ArrayBuffer> | RedirectInfo,
): response is RedirectInfo {
  return 'type' in response && response.type === 'redirect'
}

export type FetchedContent = {
  content: string
  bytes: number
  code: number
  codeText: string
  contentType: string
  persistedPath?: string
  persistedSize?: number
}

export async function getURLMarkdownContent(
  url: string,
  abortController: AbortController,
): Promise<FetchedContent | RedirectInfo> {
  if (!validateURL(url)) {
    throw new Error('Invalid URL')
  }

  // Check cache (LRUCache handles TTL automatically)
  const cachedEntry = URL_CACHE.get(url)
  if (cachedEntry) {
    return {
      bytes: cachedEntry.bytes,
      code: cachedEntry.code,
      codeText: cachedEntry.codeText,
      content: cachedEntry.content,
      contentType: cachedEntry.contentType,
      persistedPath: cachedEntry.persistedPath,
      persistedSize: cachedEntry.persistedSize,
    }
  }

  let parsedUrl: URL
  let upgradedUrl = url

  try {
    parsedUrl = new URL(url)

    // Upgrade http to https if needed
    if (parsedUrl.protocol === 'http:') {
      parsedUrl.protocol = 'https:'
      upgradedUrl = parsedUrl.toString()
    }

    const hostname = parsedUrl.hostname

    // Check if the user has opted to skip the blocklist check
    // This is for enterprise customers with restrictive security policies
    // that prevent outbound connections to acosmi.com
    const settings = getSettings_DEPRECATED()
    if (!settings.skipWebFetchPreflight) {
      const checkResult = await checkDomainBlocklist(hostname)
      switch (checkResult.status) {
        case 'allowed':
          // Continue with the fetch
          break
        case 'blocked':
          throw new DomainBlockedError(hostname)
        case 'check_failed':
          throw new DomainCheckFailedError(hostname)
      }
    }

    if (process.env.USER_TYPE === 'ant') {
      logEvent('tengu_web_fetch_host', {
        hostname:
          hostname as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })
    }
  } catch (e) {
    if (
      e instanceof DomainBlockedError ||
      e instanceof DomainCheckFailedError
    ) {
      // Expected user-facing failures - re-throw without logging as internal error
      throw e
    }
    logError(e)
  }

  const response = await getWithPermittedRedirects(
    upgradedUrl,
    abortController.signal,
    isPermittedRedirect,
  )

  // Check if we got a redirect response
  if (isRedirectInfo(response)) {
    return response
  }

  const rawBuffer = Buffer.from(response.data)
  // Release the axios-held ArrayBuffer copy; rawBuffer owns the bytes now.
  // This lets GC reclaim up to MAX_HTTP_CONTENT_LENGTH (10MB) before Turndown
  // builds its DOM tree (which can be 3-5x the HTML size).
  ;(response as { data: unknown }).data = null
  const contentType = response.headers['content-type'] ?? ''

  // Binary content: save raw bytes to disk with a proper extension so CrabCode
  // can inspect the file later. We still fall through to the utf-8 decode +
  // fast-mode summarizer path below — for PDFs in particular the decoded string
  // has enough ASCII structure (/Title, text streams) for the summarizer, and
  // the saved file is a supplement rather than a replacement.
  let persistedPath: string | undefined
  let persistedSize: number | undefined
  if (isBinaryContentType(contentType)) {
    const persistId = `webfetch-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`
    const result = await persistBinaryContent(rawBuffer, contentType, persistId)
    if (!('error' in result)) {
      persistedPath = result.filepath
      persistedSize = result.size
    }
  }

  const bytes = rawBuffer.length
  const htmlContent = rawBuffer.toString('utf-8')

  let markdownContent: string
  let contentBytes: number
  if (contentType.includes('text/html')) {
    markdownContent = (await getTurndownService()).turndown(htmlContent)
    contentBytes = Buffer.byteLength(markdownContent)
  } else {
    // It's not HTML - just use it raw. The decoded string's UTF-8 byte
    // length equals rawBuffer.length (modulo U+FFFD replacement on invalid
    // bytes — negligible for cache eviction accounting), so skip the O(n)
    // Buffer.byteLength scan.
    markdownContent = htmlContent
    contentBytes = bytes
  }

  // Store the fetched content in cache. Note that it's stored under
  // the original URL, not the upgraded or redirected URL.
  const entry: CacheEntry = {
    bytes,
    code: response.status,
    codeText: response.statusText,
    content: markdownContent,
    contentType,
    persistedPath,
    persistedSize,
  }
  // lru-cache requires positive integers; clamp to 1 for empty responses.
  URL_CACHE.set(url, entry, { size: Math.max(1, contentBytes) })
  return entry
}

export async function applyPromptToMarkdown(
  prompt: string,
  markdownContent: string,
  signal: AbortSignal,
  isNonInteractiveSession: boolean,
  isPreapprovedDomain: boolean,
): Promise<string> {
  // Truncate content to avoid "Prompt is too long" errors from the secondary model
  const truncatedContent =
    markdownContent.length > MAX_MARKDOWN_LENGTH
      ? markdownContent.slice(0, MAX_MARKDOWN_LENGTH) +
        '\n\n[Content truncated due to length...]'
      : markdownContent

  const modelPrompt = makeSecondaryModelPrompt(
    truncatedContent,
    prompt,
    isPreapprovedDomain,
  )
  const assistantMessage = await queryFastMode({
    systemPrompt: asSystemPrompt([]),
    userPrompt: modelPrompt,
    signal,
    options: {
      querySource: 'web_fetch_apply',
      agents: [],
      isNonInteractiveSession,
      hasAppendSystemPrompt: false,
      mcpTools: [],
    },
  })

  // We need to bubble this up, so that the tool call throws, causing us to return
  // an is_error tool_use block to the server, and render a red dot in the UI.
  if (signal.aborted) {
    throw new AbortError()
  }

  const { content } = assistantMessage.message
  if (content.length > 0) {
    const contentBlock = content[0]
    if ('text' in contentBlock!) {
      return contentBlock.text
    }
  }
  return 'No response from model'
}
