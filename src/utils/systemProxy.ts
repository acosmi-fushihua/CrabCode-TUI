/**
 * W-SYSPROXY-DISCOVERY P0 (2026-07-25) — read-only OS proxy probe.
 *
 * CrabCode discovers its proxy from environment variables only
 * (src/utils/proxy.ts::getProxyUrl). A machine whose proxy lives in the OS
 * settings pane instead — Windows "Internet Settings", macOS Network →
 * Proxies — therefore runs direct while the user believes a proxy is in
 * effect, and every failed fetch reads as an unexplained network error
 * (2026-07-24 根因审计 §7 F3).
 *
 * This module reads the OS setting so the `/proxy` command can report it and,
 * on `/proxy use-system`, write it into user-level settings.env. It
 * deliberately does NOT feed proxy discovery.
 *
 * HARD INVARIANT — this module is never imported by src/utils/proxy.ts.
 * Auto-adopting a discovered proxy would silently disarm the WebFetch
 * connect-time SSRF guard machine-wide (WebFetchTool/utils.ts keys that
 * decision on `envProxyActive`), turning a deliberate user act into an
 * implicit default, and local proxies happily connect to intranet and
 * metadata addresses on the caller's behalf. P1 (2026-07-25) settled this
 * permanently: the probe's output reaches the app only through settings.env,
 * i.e. through a proxy the user declared — which also keeps subprocesses
 * (Bash curl, MCP stdio servers) consistent with the app and leaves a stale
 * `ProxyEnable=1` pointing at a dead port harmless.
 * tests/unit/system-proxy-probe.test.ts pins the invariant against drift.
 *
 * Fail-soft everywhere: any non-zero exit, timeout, parse miss or exception
 * degrades to `probe-failed`. The probe never throws and never blocks — a
 * diagnostic command must not be able to break the session.
 */
import { execFileNoThrow } from './execFileNoThrow.js'

export type SystemProxySnapshot =
  /** A concrete host:port the user can paste into settings.env. */
  | { kind: 'static'; url: string; bypass?: string }
  /** Auto-configuration script detected. CrabCode never evaluates PAC. */
  | { kind: 'pac'; pacUrl: string }
  /** Platform readable and confirmed to have no proxy configured. */
  | { kind: 'none' }
  /** No single source of truth on this platform (Linux desktops disagree). */
  | { kind: 'unsupported-platform'; platform: string }
  /** Read or parse failed — reported honestly rather than guessed. */
  | { kind: 'probe-failed' }

type ProbeExec = (
  file: string,
  args: string[],
) => Promise<{ stdout: string; code: number }>

export type SystemProxyProbeDeps = {
  /** Test seam — defaults to process.platform. */
  platform?: NodeJS.Platform
  /** Test seam — defaults to a bounded execFileNoThrow call. */
  exec?: ProbeExec
}

/**
 * Budget for the whole probe subprocess.
 *
 * Sized for *process spawn latency under load*, not for the registry read —
 * the read itself is ~50ms, but a loaded Windows box (real-time AV scanning
 * every spawn) has been measured taking 2.2–2.8s just to get `reg.exe`
 * running, and this repo already carries a documented family of cold-spawn
 * tests that blow a 5s budget on the same machine. An under-sized budget does
 * not merely degrade the diagnostic: `/proxy use-system` refuses to write on
 * `probe-failed`, so a transient load spike would read as "the command is
 * broken". 10s is a hung-process backstop, not a latency guess.
 */
export const PROBE_TIMEOUT_MS = 10_000

const WINDOWS_INTERNET_SETTINGS_KEY =
  'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings'

const defaultExec: ProbeExec = async (file, args) => {
  const result = await execFileNoThrow(file, args, {
    timeout: PROBE_TIMEOUT_MS,
    preserveOutputOnError: true,
  })
  return { stdout: result.stdout, code: result.code }
}

/**
 * Read the OS proxy setting. Deliberately un-memoized: the only caller is a
 * manually invoked command, and the hard invariant above forbids hot-path use,
 * so a cache would buy nothing while adding mutable module state.
 */
export async function probeSystemProxy(
  deps: SystemProxyProbeDeps = {},
): Promise<SystemProxySnapshot> {
  const platform = deps.platform ?? process.platform
  const exec = deps.exec ?? defaultExec
  try {
    if (platform === 'win32') return await probeWindows(exec)
    if (platform === 'darwin') return await probeDarwin(exec)
    return { kind: 'unsupported-platform', platform }
  } catch {
    return { kind: 'probe-failed' }
  }
}

// --- Windows ---------------------------------------------------------------

async function probeWindows(exec: ProbeExec): Promise<SystemProxySnapshot> {
  // Absolute path when SystemRoot is known: PATH resolution is the same class
  // of risk that let a WSL stub hijack `bash` on this platform before.
  // `reg query` reads the whole key in one call — it takes at most one /v.
  const file = process.env.SystemRoot
    ? `${process.env.SystemRoot}\\System32\\reg.exe`
    : 'reg'
  const { stdout, code } = await exec(file, [
    'query',
    WINDOWS_INTERNET_SETTINGS_KEY,
  ])
  if (code !== 0) return { kind: 'probe-failed' }
  const values = parseRegQuery(stdout)
  // Internet Settings is never a valueless key on a real Windows profile, so
  // parsing nothing means the output shape defeated us (locale, redirection,
  // a future reg format). Reporting `none` there would tell the user "no system
  // proxy" while one is configured; the honest-failure bucket says so instead.
  if (values.size === 0) return { kind: 'probe-failed' }
  // Windows treats any non-zero ProxyEnable as on. Testing `=== 1` would report
  // "no system proxy" on a machine that has one — the exact lie this probe
  // exists to prevent.
  const enabled = (parseRegDword(values.get('proxyenable')) ?? 0) !== 0
  const server = values.get('proxyserver')?.trim() ?? ''
  if (enabled && server) {
    const hostPort = pickWindowsProxyEndpoint(server)
    // Enabled with a value we cannot turn into a recipe (a socks-only entry,
    // say). Reporting `none` would be a lie and guessing would hand the user a
    // broken recipe, so this joins the honest-failure bucket.
    const url = hostPort ? toProxyUrl(hostPort) : undefined
    if (!url) return { kind: 'probe-failed' }
    const bypass = values.get('proxyoverride')?.trim()
    return { kind: 'static', url, ...(bypass ? { bypass } : {}) }
  }
  const pacUrl = values.get('autoconfigurl')?.trim()
  if (pacUrl) return { kind: 'pac', pacUrl }
  return { kind: 'none' }
}

/**
 * `reg query` prints one indented `<name>    <REG_TYPE>    <value>` row per
 * value; the value itself may contain spaces and may be empty. Names are
 * lower-cased because the registry is case-insensitive.
 */
function parseRegQuery(stdout: string): Map<string, string> {
  const values = new Map<string, string>()
  for (const line of stdout.split(/\r?\n/)) {
    const match = /^\s+(\S+)\s+REG_[A-Z_]+\s*(.*)$/.exec(line)
    if (match) values.set(match[1]!.toLowerCase(), match[2] ?? '')
  }
  return values
}

function parseRegDword(raw: string | undefined): number | null {
  if (!raw) return null
  const parsed = Number(raw.trim())
  return Number.isFinite(parsed) ? parsed : null
}

/**
 * `ProxyServer` is either a bare `host:port` for every protocol, or the legacy
 * per-protocol form `http=host:port;https=host:port;socks=host:port`.
 */
function pickWindowsProxyEndpoint(server: string): string | undefined {
  if (!server.includes('=')) return server
  const byProtocol = new Map<string, string>()
  for (const entry of server.split(';')) {
    const index = entry.indexOf('=')
    if (index <= 0) continue
    const protocol = entry.slice(0, index).trim().toLowerCase()
    const endpoint = entry.slice(index + 1).trim()
    if (protocol && endpoint) byProtocol.set(protocol, endpoint)
  }
  return byProtocol.get('https') ?? byProtocol.get('http')
}

// --- macOS -----------------------------------------------------------------

async function probeDarwin(exec: ProbeExec): Promise<SystemProxySnapshot> {
  const { stdout, code } = await exec('/usr/sbin/scutil', ['--proxies'])
  if (code !== 0) return { kind: 'probe-failed' }
  const values = parseScutilProxies(stdout)
  const bypass = parseScutilExceptions(stdout)
  const https = readScutilEndpoint(values, 'HTTPS')
  const http = readScutilEndpoint(values, 'HTTP')
  const endpoint = https ?? http
  if (endpoint) {
    const url = toProxyUrl(endpoint)
    if (!url) return { kind: 'probe-failed' }
    return { kind: 'static', url, ...(bypass ? { bypass } : {}) }
  }
  if (values.get('ProxyAutoConfigEnable') === '1') {
    const pacUrl = values.get('ProxyAutoConfigURLString')?.trim()
    if (pacUrl) return { kind: 'pac', pacUrl }
  }
  return { kind: 'none' }
}

/** `scutil --proxies` prints `  <Key> : <value>` rows inside a dictionary. */
function parseScutilProxies(stdout: string): Map<string, string> {
  const values = new Map<string, string>()
  for (const line of stdout.split(/\r?\n/)) {
    const match = /^\s*([A-Za-z][A-Za-z0-9]*)\s*:\s*(.+)$/.exec(line)
    if (match) values.set(match[1]!, match[2]!.trim())
  }
  return values
}

/**
 * `ExceptionsList : <array> { 0 : *.local … }` — collect the numeric rows of
 * that block. Display only: the entries use Apple's wildcard syntax, which is
 * not NO_PROXY syntax, so they are never folded into the printed recipe.
 */
function parseScutilExceptions(stdout: string): string | undefined {
  const entries: string[] = []
  let inList = false
  for (const line of stdout.split(/\r?\n/)) {
    if (!inList) {
      if (/^\s*ExceptionsList\s*:\s*<array>\s*\{/.test(line)) inList = true
      continue
    }
    if (/^\s*\}/.test(line)) break
    const match = /^\s*\d+\s*:\s*(.+)$/.exec(line)
    if (match) entries.push(match[1]!.trim())
  }
  return entries.length ? entries.join(',') : undefined
}

function readScutilEndpoint(
  values: Map<string, string>,
  prefix: 'HTTP' | 'HTTPS',
): string | undefined {
  if (values.get(`${prefix}Enable`) !== '1') return undefined
  const host = values.get(`${prefix}Proxy`)?.trim()
  const port = values.get(`${prefix}Port`)?.trim()
  if (!host) return undefined
  return port ? `${host}:${port}` : host
}

// --- shared ----------------------------------------------------------------

/** host, host:port, [v6]:port, optionally already carrying a scheme. */
const PROXY_ENDPOINT_RE = /^(?:[a-z][a-z0-9+.-]*:\/\/)?[A-Za-z0-9._~%:[\]-]+$/i

/**
 * Turn a probed endpoint into a proxy URL, rejecting anything that is not a
 * plain host[:port]. The value is printed verbatim inside a JSON recipe the
 * user pastes into settings.json, so a stray quote or backslash would hand
 * them broken JSON; an endpoint we cannot vouch for degrades to the
 * honest-failure bucket instead.
 *
 * Proxy endpoints are spoken to over plain HTTP even when they carry HTTPS
 * traffic, so a scheme-less `host:port` becomes `http://host:port` — the shape
 * HTTPS_PROXY expects. An endpoint that already names a scheme is left alone.
 */
function toProxyUrl(endpoint: string): string | undefined {
  if (!PROXY_ENDPOINT_RE.test(endpoint)) return undefined
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(endpoint)
    ? endpoint
    : `http://${endpoint}`
}

// --- bypass list translation (P1) ------------------------------------------

/**
 * NO_PROXY entries CrabCode always writes alongside an adopted proxy.
 *
 * Not cosmetic: `configureGlobalAgents()` installs a process-wide undici
 * dispatcher and an axios interceptor, so without a loopback bypass the
 * machine's own services — the oauthapi-llm sidecar on its random loopback
 * port, the app-server, a local model server — would have their traffic
 * handed to the proxy. This floor is why the recipe has always listed
 * NO_PROXY as mandatory rather than optional.
 */
export const LOOPBACK_NO_PROXY_FLOOR = ['localhost', '127.0.0.1', '::1']

export type BypassTranslation = {
  /** Comma-separated NO_PROXY value: the floor plus everything expressible. */
  noProxy: string
  /** System entries with no NO_PROXY equivalent, reported rather than dropped silently. */
  dropped: string[]
}

/**
 * Translate one OS bypass entry into `shouldBypassProxy` syntax, or reject it.
 *
 * The two dialects genuinely differ — Windows speaks `<local>` and `10.*`,
 * macOS speaks `169.254/16` — and `shouldBypassProxy` (src/utils/proxy.ts)
 * understands only exact hostnames, IPs, `host:port` and `.suffix`. An entry
 * we cannot express exactly is refused so the caller can surface it, because
 * an inert entry silently sends traffic the user meant to keep off the proxy.
 */
function translateBypassEntry(entry: string): string | undefined {
  // A bare "*" means "bypass everything". Carrying it over would neuter the
  // very proxy we are adopting, so it is refused rather than honoured.
  if (entry === '*') return undefined
  // Windows tokens: <local> (dotless hostnames), <-loopback>. No equivalent.
  if (entry.startsWith('<')) return undefined
  // CIDR ("169.254/16") — shouldBypassProxy has no netmask matching.
  if (entry.includes('/')) return undefined
  // "*.corp.example" is exactly the `.suffix` form; any other wildcard
  // placement ("10.*", "a*b") is not expressible.
  if (entry.startsWith('*.')) {
    const suffix = entry.slice(1)
    return suffix.includes('*') ? undefined : suffix
  }
  if (entry.includes('*')) return undefined
  // Defensive: nothing that could corrupt the comma-separated NO_PROXY value.
  if (/["'\\\s,;]/.test(entry)) return undefined
  return entry
}

/**
 * Build the NO_PROXY value that accompanies an adopted system proxy.
 *
 * Windows separates its ProxyOverride with `;`, macOS's ExceptionsList arrives
 * comma-joined from the probe; both are accepted. The loopback floor always
 * leads, duplicates are folded case-insensitively, and untranslatable entries
 * come back in `dropped` for the caller to print.
 */
export function translateSystemBypass(
  bypass: string | undefined,
): BypassTranslation {
  const kept = [...LOOPBACK_NO_PROXY_FLOOR]
  const seen = new Set(kept.map(entry => entry.toLowerCase()))
  const dropped: string[] = []
  for (const raw of (bypass ?? '').split(/[;,\s]+/)) {
    const entry = raw.trim()
    if (!entry) continue
    const translated = translateBypassEntry(entry)
    if (!translated) {
      if (!dropped.includes(entry)) dropped.push(entry)
      continue
    }
    const key = translated.toLowerCase()
    if (seen.has(key)) continue
    seen.add(key)
    kept.push(translated)
  }
  return { noProxy: kept.join(','), dropped }
}
