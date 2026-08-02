export const BROWSER_COMMAND_SCHEMA_VERSION = 1 as const
export const BROWSER_COMMAND_RESULT_KIND = 'browser.command.result' as const

export const BROWSER_ERROR_KINDS = [
  'invalid_request',
  'permission_required',
  'unsupported_action',
  'backend_unavailable',
  'backend_failed',
  // 浏览器运行时(chromium)缺失或按需下载失败,区别于 backend_unavailable(缺
  // crabcode-browser 二进制)。对齐 acosmi-cmd-browser BrowserErrorKind::BrowserRuntimeMissing。
  'browser_runtime_missing',
  'timeout',
] as const

export type BrowserErrorKind = (typeof BROWSER_ERROR_KINDS)[number]

export type BrowserCommandAction = {
  method: string
  path: string
}

export type BrowserCommandError = {
  kind: BrowserErrorKind | string
  message: string
}

export type BrowserCommandEnvelope<T = unknown> = {
  schemaVersion: typeof BROWSER_COMMAND_SCHEMA_VERSION
  kind: typeof BROWSER_COMMAND_RESULT_KIND
  ok: boolean
  backend: string
  profile: string
  action: BrowserCommandAction
  data: T | null
  error: BrowserCommandError | null
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

export function isBrowserErrorKind(value: unknown): value is BrowserErrorKind {
  return (
    typeof value === 'string' &&
    BROWSER_ERROR_KINDS.includes(value as BrowserErrorKind)
  )
}

export function isBrowserCommandEnvelope(
  value: unknown,
): value is BrowserCommandEnvelope {
  if (!isRecord(value)) return false
  if (value.schemaVersion !== BROWSER_COMMAND_SCHEMA_VERSION) return false
  if (value.kind !== BROWSER_COMMAND_RESULT_KIND) return false
  if (typeof value.ok !== 'boolean') return false
  if (typeof value.backend !== 'string') return false
  if (typeof value.profile !== 'string') return false
  if (!isRecord(value.action)) return false
  if (typeof value.action.method !== 'string') return false
  if (typeof value.action.path !== 'string') return false

  if (value.error !== null) {
    if (!isRecord(value.error)) return false
    if (typeof value.error.kind !== 'string') return false
    if (typeof value.error.message !== 'string') return false
  }

  return true
}

export function parseBrowserCommandEnvelope<T = unknown>(
  stdout: string,
): BrowserCommandEnvelope<T> {
  const trimmed = stdout.trim()
  if (!trimmed) {
    throw new Error('browser command output is empty')
  }

  let parsed: unknown
  try {
    parsed = JSON.parse(trimmed)
  } catch (error) {
    throw new Error(
      `browser command output is not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
    )
  }

  if (!isBrowserCommandEnvelope(parsed)) {
    throw new Error('browser command output does not match schema version 1')
  }

  return parsed as BrowserCommandEnvelope<T>
}

export function createBrowserCommandEnvelope<T>(
  options: {
    backend: string
    profile: string
    action: BrowserCommandAction
    data?: T | null
    error?: BrowserCommandError | null
    ok?: boolean
  },
): BrowserCommandEnvelope<T> {
  const error = options.error ?? null
  return {
    schemaVersion: BROWSER_COMMAND_SCHEMA_VERSION,
    kind: BROWSER_COMMAND_RESULT_KIND,
    ok: options.ok ?? error === null,
    backend: options.backend,
    profile: options.profile,
    action: options.action,
    data: options.data ?? null,
    error,
  }
}
