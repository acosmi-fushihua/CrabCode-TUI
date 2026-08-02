import {
  BROWSER_ERROR_KINDS,
  isBrowserCommandEnvelope,
  type BrowserErrorKind,
} from './protocol.js'

export const BROWSER_CAPABILITY_FAILURE_KINDS = [
  ...BROWSER_ERROR_KINDS,
  'requires_config',
] as const

export type BrowserCapabilityFailureKind =
  (typeof BROWSER_CAPABILITY_FAILURE_KINDS)[number]

export type BrowserCapabilityFailure = {
  kind: BrowserCapabilityFailureKind
  backend: 'crabcode-browser'
  retryable: boolean
  guidance: string
}

export type BrowserIntent = {
  backend: 'builtin'
  skill: 'crabcode-browser'
}

export type AutomationSurfaceIntent = BrowserIntent
export type AutomationSurfaceCapabilityFailure = BrowserCapabilityFailure

const BROWSER_SKILL_NAMES = new Set([
  'crabcode-browser',
  'browser-automation',
])

export function automationSurfaceForSkillName(
  skillName: string,
): AutomationSurfaceIntent['backend'] | undefined {
  return BROWSER_SKILL_NAMES.has(skillName) ? 'builtin' : undefined
}

export const AUTOMATION_SURFACE_ROUTING_POLICY = {
  purposeBuilt:
    'For semantic work on a linked resource, prefer a purpose-built connector, API, or CLI when one is available; a URL alone is not permission to drive a page.',
  browser:
    'Use the isolated CrabCode browser for explicit web-UI interaction: pages, tabs, DOM, console or network inspection, screenshots, and page actions. It uses an isolated profile and does not attach to a desktop browser session.',
  noSilentFallback:
    'If the isolated browser is unavailable, report that boundary instead of switching to a different browser profile or desktop-control mechanism.',
} as const

export const BROWSER_AUTOMATION_SKILL_DESCRIPTION =
  'Browser automation for explicit interactive web-UI tasks in an isolated Chromium session. Prefer a purpose-built connector, API, or CLI for semantic linked-resource work.'

export const BROWSER_AUTOMATION_SKILL_WHEN_TO_USE =
  `${AUTOMATION_SURFACE_ROUTING_POLICY.purposeBuilt} ${AUTOMATION_SURFACE_ROUTING_POLICY.browser} ${AUTOMATION_SURFACE_ROUTING_POLICY.noSilentFallback}`

export function getAutomationSurfaceRoutingGuidance(
  activeSurface: 'browser',
): string {
  return `## Automation surface selection

${AUTOMATION_SURFACE_ROUTING_POLICY.purposeBuilt}

Active surface for this skill: **${activeSurface}**. ${AUTOMATION_SURFACE_ROUTING_POLICY.browser}

${AUTOMATION_SURFACE_ROUTING_POLICY.noSilentFallback}`
}

export type BrowserIntentRouteDecision =
  | { type: 'none' }
  | ({ type: 'skill'; slashInput: string } & BrowserIntent)
  | { type: 'failure'; failure: BrowserCapabilityFailure }

export type AutomationSurfaceRouteDecision = BrowserIntentRouteDecision

export const BROWSER_INTENT_MAX_INPUT_CHARS = 400
export const BROWSER_INTENT_MAX_INPUT_LINES = 4

const BROWSER_TARGET =
  /(?:https?:\/\/|\b(?:browser|webpage|web page|website|site|page|tab|dom|console|network panel|网页|网站|页面|浏览器|标签页)\b)/iu
const BROWSER_ACTION =
  /(?:\b(?:open|navigate|visit|click|type|fill|submit|inspect|interact|test|debug|screenshot|capture|scroll|select)\b|打开|访问|点击|输入|填写|提交|检查|交互|测试|调试|截图|滚动|选择)/iu
const INFORMATIONAL_ONLY =
  /^(?:what|why|when|where|who|how (?:does|do|is|are|can)|explain|describe|summarize|什么|为什么|何时|哪里|谁|解释|说明|总结)\b/iu
const SOURCE_TASK =
  /(?:\b(?:source|code|implementation|function|class|module|file|repository|repo|compile|build)\b|源码|代码|实现|函数|类|模块|文件|仓库|编译|构建)/iu

function boundedUserIntent(input: string): string | null {
  const text = input.trim()
  if (
    text.length === 0 ||
    text.length > BROWSER_INTENT_MAX_INPUT_CHARS ||
    text.split(/\r?\n/u).length > BROWSER_INTENT_MAX_INPUT_LINES ||
    text.startsWith('/') ||
    text.startsWith('```')
  ) {
    return null
  }
  return text
}

export function classifyBrowserIntent(input: string): BrowserIntent | null {
  const text = boundedUserIntent(input)
  if (!text || INFORMATIONAL_ONLY.test(text) || SOURCE_TASK.test(text)) {
    return null
  }
  if (!BROWSER_TARGET.test(text) || !BROWSER_ACTION.test(text)) return null
  return { backend: 'builtin', skill: 'crabcode-browser' }
}

export function classifyAutomationSurfaceIntent(
  input: string,
): AutomationSurfaceIntent | null {
  return classifyBrowserIntent(input)
}

function browserUnavailableFailure(): BrowserCapabilityFailure {
  return {
    kind: 'backend_unavailable',
    backend: 'crabcode-browser',
    retryable: false,
    guidance:
      'The isolated browser skill is unavailable in this TUI runtime. Repair the browser helper or installation; do not claim the page action ran.',
  }
}

export function decideBrowserIntentRoute(options: {
  input: string
  availableCommandNames: readonly string[]
  localCapabilityRoutingAllowed?: boolean
}): BrowserIntentRouteDecision {
  const intent = classifyBrowserIntent(options.input)
  if (!intent) return { type: 'none' }
  if (options.localCapabilityRoutingAllowed === false) {
    return { type: 'failure', failure: browserUnavailableFailure() }
  }
  if (!options.availableCommandNames.includes(intent.skill)) {
    return { type: 'failure', failure: browserUnavailableFailure() }
  }
  return {
    type: 'skill',
    ...intent,
    slashInput: `/${intent.skill} ${options.input}`,
  }
}

export function decideAutomationSurfaceRoute(options: {
  input: string
  availableCommandNames: readonly string[]
  availableToolNames?: readonly string[]
  localCapabilityRoutingAllowed?: boolean
}): AutomationSurfaceRouteDecision {
  return decideBrowserIntentRoute(options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function isBrowserCliCommand(input: unknown): boolean {
  if (!isRecord(input) || typeof input.command !== 'string') return false
  return /^(?:(?:[A-Za-z_][A-Za-z0-9_]*=[^\s]+)\s+)*(?:"[^"]*\/(?:crabcode|acosmi)"|'[^']*\/(?:crabcode|acosmi)'|(?:[^\s]*\/)?(?:crabcode|acosmi))\s+browser(?:\s|$)/u.test(
    input.command.trim(),
  )
}

function browserToolBackend(
  toolName: string,
  input: unknown,
): BrowserCapabilityFailure['backend'] | null {
  if (toolName === 'Skill' && isRecord(input) && input.skill === 'crabcode-browser') {
    return 'crabcode-browser'
  }
  return toolName === 'Bash' && isBrowserCliCommand(input)
    ? 'crabcode-browser'
    : null
}

const FAILURE_GUIDANCE: Record<BrowserCapabilityFailureKind, string> = {
  invalid_request: 'Correct the browser action arguments before retrying.',
  permission_required:
    'The browser action was blocked by the current permission or sandbox policy; request approval or use an allowed action.',
  unsupported_action:
    'The isolated browser does not support this action; use a supported action without switching profiles.',
  backend_unavailable:
    'The isolated browser is unavailable; inspect its installation before retrying.',
  backend_failed:
    'The isolated browser failed; preserve its original error kind and diagnostics.',
  browser_runtime_missing:
    'The Chromium runtime is missing or could not be downloaded; repair it before retrying.',
  timeout:
    'The browser action timed out; retry only when the operation is safe and can be narrowed.',
  requires_config:
    'The browser helper is not configured or its version does not match this TUI runtime.',
}

function knownFailureKind(value: string): BrowserCapabilityFailureKind | null {
  return BROWSER_CAPABILITY_FAILURE_KINDS.includes(
    value as BrowserCapabilityFailureKind,
  )
    ? (value as BrowserCapabilityFailureKind)
    : null
}

function failureFromKind(
  kind: BrowserCapabilityFailureKind,
): BrowserCapabilityFailure {
  return {
    kind,
    backend: 'crabcode-browser',
    retryable:
      kind === 'timeout' ||
      kind === 'backend_failed' ||
      kind === 'backend_unavailable',
    guidance: FAILURE_GUIDANCE[kind],
  }
}

export function classifyBrowserCapabilityFailure(options: {
  toolName: string
  input: unknown
  message: string
  permissionDenied?: boolean
}): BrowserCapabilityFailure | null {
  if (!browserToolBackend(options.toolName, options.input)) return null
  if (options.permissionDenied) return failureFromKind('permission_required')

  const text = options.message.trim()
  try {
    const parsed: unknown = JSON.parse(text)
    if (isBrowserCommandEnvelope(parsed) && parsed.error) {
      return failureFromKind(
        knownFailureKind(parsed.error.kind) ?? 'backend_failed',
      )
    }
  } catch {
    // Shell failures frequently wrap the structured result in exit text.
  }
  const bracketed = /(?:browser\s+error\s*)?\[([a-z_]+)\]/iu.exec(text)?.[1]
  const bracketedKind = bracketed ? knownFailureKind(bracketed) : null
  if (bracketedKind) return failureFromKind(bracketedKind)
  if (/(?:permission denied|permission_required|approval required)/iu.test(text)) {
    return failureFromKind('permission_required')
  }
  if (/(?:browser_runtime_missing|browser runtime.{0,16}(?:missing|not installed)|failed to download browser)/iu.test(text)) {
    return failureFromKind('browser_runtime_missing')
  }
  if (/(?:backend_unavailable|failed to launch crabcode-browser|browser backend.{0,16}(?:unavailable|not found))/iu.test(text)) {
    return failureFromKind('backend_unavailable')
  }
  if (/(?:unsupported_action|not supported by the .*browser)/iu.test(text)) {
    return failureFromKind('unsupported_action')
  }
  if (/(?:timed? out|timeout)/iu.test(text)) return failureFromKind('timeout')
  return failureFromKind('backend_failed')
}

const BROWSER_CLI_SKEW_MARKER_TAG = 'browser-cli-version-skew'
const BROWSER_ENVELOPE_KIND_RE = /"kind"\s*:\s*"browser\.command\.result"/u
const BROWSER_ENVELOPE_CLI_BUILD_ID_RE = /"cliBuildId"\s*:\s*"([^"]+)"/u

function buildIdVersionSegment(id: string): string | null {
  return id.split('+', 1)[0] || null
}

function isAuthoritativeBuildId(id: string | undefined | null): id is string {
  return typeof id === 'string' && id.length > 0 && !id.endsWith('+unknown')
}

export type BrowserCliVersionSkew = {
  failure: BrowserCapabilityFailure
  cliBuildId: string | null
  hostBuildId: string
}

export function detectBrowserCliVersionSkew(options: {
  toolName: string
  input: unknown
  message: string
  hostBuildId?: string | null
}): BrowserCliVersionSkew | null {
  if (options.toolName !== 'Bash' || !isBrowserCliCommand(options.input)) {
    return null
  }
  const hostBuildId =
    options.hostBuildId !== undefined
      ? options.hostBuildId
      : (process.env.CRABCODE_HOST_BUILD_ID ?? null)
  if (!isAuthoritativeBuildId(hostBuildId)) return null

  let envelopePresent = false
  let cliBuildId: string | null = null
  try {
    const parsed: unknown = JSON.parse(options.message.trim())
    if (isBrowserCommandEnvelope(parsed)) {
      envelopePresent = true
      const raw = (parsed as Record<string, unknown>).cliBuildId
      cliBuildId = typeof raw === 'string' && raw.length > 0 ? raw : null
    }
  } catch {
    // Fall through to a bounded tolerant scan.
  }
  if (!envelopePresent) {
    if (!BROWSER_ENVELOPE_KIND_RE.test(options.message)) return null
    cliBuildId = BROWSER_ENVELOPE_CLI_BUILD_ID_RE.exec(options.message)?.[1] ?? null
  }
  if (cliBuildId !== null) {
    if (!isAuthoritativeBuildId(cliBuildId)) return null
    if (buildIdVersionSegment(cliBuildId) === buildIdVersionSegment(hostBuildId)) {
      return null
    }
  }

  const observed = cliBuildId ?? 'legacy build without cliBuildId'
  return {
    cliBuildId,
    hostBuildId,
    failure: {
      kind: 'requires_config',
      backend: 'crabcode-browser',
      retryable: false,
      guidance: `The crabcode browser helper is a different build than this TUI runtime (helper: ${observed}; runtime: ${hostBuildId}). Update the installation so both versions match before retrying.`,
    },
  }
}

export function buildBrowserCliVersionSkewMarker(
  skew: BrowserCliVersionSkew,
): string {
  const payload = JSON.stringify({
    type: 'browser_cli_version_skew',
    kind: skew.failure.kind,
    backend: skew.failure.backend,
    retryable: skew.failure.retryable,
    cliBuildId: skew.cliBuildId,
    hostBuildId: skew.hostBuildId,
    guidance: skew.failure.guidance.slice(0, 700),
  })
  return `<${BROWSER_CLI_SKEW_MARKER_TAG}>${payload}</${BROWSER_CLI_SKEW_MARKER_TAG}>`
}

export function appendBrowserCliVersionSkewMarker(options: {
  toolName: string
  input: unknown
  message: string
  hostBuildId?: string | null
}): string {
  if (options.message.includes(`<${BROWSER_CLI_SKEW_MARKER_TAG}>`)) {
    return options.message
  }
  const skew = detectBrowserCliVersionSkew(options)
  return skew
    ? [options.message, buildBrowserCliVersionSkewMarker(skew)]
        .filter(Boolean)
        .join('\n\n')
    : options.message
}

export function buildBrowserCapabilityFailureMarker(
  failure: BrowserCapabilityFailure,
): string {
  const payload = JSON.stringify({
    type: 'browser_capability_failure',
    kind: failure.kind,
    backend: failure.backend,
    retryable: failure.retryable,
    guidance: failure.guidance.slice(0, 500),
  })
  return `<browser-capability-failure>${payload}</browser-capability-failure>`
}

export function appendBrowserCapabilityFailureMarker(options: {
  toolName: string
  input: unknown
  message: string
  permissionDenied?: boolean
}): string {
  if (options.message.includes('<browser-capability-failure>')) {
    return options.message
  }
  const failure = classifyBrowserCapabilityFailure(options)
  return failure
    ? [options.message, buildBrowserCapabilityFailureMarker(failure)]
        .filter(Boolean)
        .join('\n\n')
    : options.message
}

export function getBrowserFailurePreservationReminder(
  backend: BrowserCapabilityFailure['backend'],
): string {
  return `## Failure contract

Browser failures are capability results. Preserve the producer's exact failure kind and backend. Use one of: ${BROWSER_CAPABILITY_FAILURE_KINDS.join(', ')}. Never flatten the failure into a generic apology or silently switch browser profiles. Selected backend: ${backend}.`
}

export type { BrowserErrorKind }
