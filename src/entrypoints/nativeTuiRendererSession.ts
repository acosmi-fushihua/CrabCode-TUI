import { randomUUID } from 'node:crypto'
import { realpath } from 'node:fs/promises'
import { homedir } from 'node:os'
import { resolve } from 'node:path'
import type { ZodType } from 'zod/v4'

import {
  CRABCODE_TUI_THEME_SETTINGS,
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
  CRABCODE_TUI_SETUP_SUBTYPE,
  CrabCodeTuiRendererContextResponseSchema,
  CrabCodeTuiRendererScrollSpeedResponseSchema,
  CrabCodeTuiSetupRequestSchema,
  CrabCodeTuiWorkspaceTrustResponseSchema,
  type CrabCodeTuiSetupControlRequest,
  type CrabCodeTuiSetupRequest,
} from '../cli/crabcodeTuiBridgeProtocol.js'
import type { NotificationChannel } from '../utils/config.js'
import { Stream } from '../utils/stream.js'
import type { ThemeSetting } from '../utils/theme.js'

export const NATIVE_TUI_RENDERER_PROTOCOL_VERSION =
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION
/**
 * Exact `TransportLimits::default().max_stdout_frame_bytes` from the Rust
 * renderer. The setup adapter measures the complete control envelope before
 * every write; this is not an estimated payload allowance.
 */
export const NATIVE_TUI_MAX_FRAME_BYTES = 16 * 1024 * 1024
const NATIVE_TUI_SETUP_REQUEST_ID_SAMPLE = `crabcode-tui-setup-${'0'.repeat(36)}`

type TrustAuthority = {
  isWorkspacePathTrusted(cwd: string): boolean
  getRendererConfiguration(): {
    verbose: boolean
    preferredNotificationChannel: NotificationChannel
    messageIdleNotificationThresholdMs: number
    uiLanguage: 'zh-CN' | 'en-US'
    themeSetting: ThemeSetting
    syntaxHighlightingDisabled: boolean
  }
  acceptWorkspace(cwd: string, isHomeDirectory: boolean): void
  activateInteractiveSession(): void
}

type RendererTransport = {
  input: AsyncIterable<string | Uint8Array>
  writeLine(line: string): Promise<void>
}

export type NativeTuiRendererSession = {
  /**
   * Project the renderer context only after the backend configuration
   * authority has completed initialization. The stdin router itself is
   * established earlier so Rust's single SDK initialize line cannot race
   * startup imports.
   */
  bindRendererContext(): Promise<void>
  /**
   * Project the one historical renderer-local scroll input after the child
   * has applied its post-trust managed environment. This is not a generic
   * settings bridge and may run only once before StructuredIO handoff.
   */
  projectRendererScrollSpeed(rawValue: string | undefined): Promise<void>
  ensureWorkspaceTrust(cwd: string): Promise<void>
  /**
   * Process-private, closed renderer exchange. This is not a backend or SDK
   * capability: every accepted request is a member of the setup projection
   * union and every response is correlated by request_id.
   */
  requestSetup<Response>(
    request: CrabCodeTuiSetupRequest,
    responseSchema: ZodType<Response>,
  ): Promise<Response>
  /**
   * End the renderer-only startup phase and transfer sole stdin ownership to
   * StructuredIO. The SDK initialize line stashed during setup is yielded
   * first, followed by every subsequent stdin line without loss or replay.
   */
  finishSetup(): Promise<AsyncIterable<string>>
}

type RendererSessionDependencies = {
  authority?: TrustAuthority
  transport?: RendererTransport
  canonicalize?: (path: string) => Promise<string>
  currentWorkingDirectory?: () => string
  homeDirectory?: () => string
}

type PendingExchange = {
  schema: ZodType<unknown>
  resolve(value: unknown): void
  reject(error: unknown): void
}

class NativeTuiWorkspaceTrustDeclinedError extends Error {
  constructor(cwd: string) {
    super(`Workspace trust was declined for ${cwd}`)
    this.name = 'NativeTuiWorkspaceTrustDeclinedError'
  }
}

/**
 * Establish one lossless stdin router before setup starts.
 *
 * Rust is allowed to send its one SDK initialize request immediately. The
 * router stashes that exact line while setup exchanges are active; every
 * other setup-time line must be a correlated response. This avoids the former
 * per-exchange `data` listeners, which could discard a second line delivered
 * in the same chunk.
 */
export async function startNativeTuiRendererSession(
  dependencies: RendererSessionDependencies = {},
): Promise<NativeTuiRendererSession> {
  const canonicalize = dependencies.canonicalize ?? canonicalizeWorkspace
  const currentWorkingDirectory =
    dependencies.currentWorkingDirectory ?? (() => process.cwd())
  const homeDirectory = dependencies.homeDirectory ?? homedir
  const transport = dependencies.transport ?? createProcessRendererTransport()
  const router = new NativeTuiSetupLineRouter(transport)
  let authority: TrustAuthority
  try {
    authority = dependencies.authority ?? (await loadTrustAuthority())
  } catch (error) {
    router.abort(asError(error))
    throw error
  }

  let rendererContextState: 'unbound' | 'binding' | 'bound' | 'failed' =
    'unbound'
  let rendererContextCwd: string | undefined
  let canonicalHome: string | undefined
  let trustChecked = false
  let rendererScrollSpeedProjected = false

  function requireRendererContext(): void {
    if (rendererContextState !== 'bound') {
      throw new Error(
        'Native TUI renderer context must be bound after configuration initialization',
      )
    }
  }

  return {
    async bindRendererContext(): Promise<void> {
      if (rendererContextState !== 'unbound') {
        throw new Error('Native TUI renderer context may be bound only once')
      }
      rendererContextState = 'binding'
      try {
        const [initialCwd, resolvedHome] = await Promise.all([
          canonicalize(currentWorkingDirectory()),
          canonicalize(homeDirectory()),
        ])
        const rendererConfiguration = authority.getRendererConfiguration()
        await router.requestSetup(
          {
            subtype: CRABCODE_TUI_SETUP_SUBTYPE,
            protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
            kind: 'renderer_context',
            cwd: initialCwd,
            config_verbose: rendererConfiguration.verbose,
            preferred_notification_channel:
              rendererConfiguration.preferredNotificationChannel,
            message_idle_notification_threshold_ms:
              rendererConfiguration.messageIdleNotificationThresholdMs,
            ui_language: rendererConfiguration.uiLanguage,
            theme_setting: rendererConfiguration.themeSetting,
            syntax_highlighting_disabled:
              rendererConfiguration.syntaxHighlightingDisabled,
          },
          CrabCodeTuiRendererContextResponseSchema,
        )
        rendererContextCwd = initialCwd
        canonicalHome = resolvedHome
        rendererContextState = 'bound'
      } catch (error) {
        rendererContextState = 'failed'
        router.abort(asError(error))
        throw error
      }
    },
    async ensureWorkspaceTrust(requestedCwd: string): Promise<void> {
      requireRendererContext()
      if (trustChecked) {
        throw new Error('Native TUI workspace trust may be checked only once')
      }
      trustChecked = true
      const [canonicalCwd, liveProcessCwd] = await Promise.all([
        canonicalize(requestedCwd),
        canonicalize(currentWorkingDirectory()),
      ])
      if (canonicalCwd !== liveProcessCwd) {
        throw new Error(
          `Native TUI trust cwd mismatch: requested ${canonicalCwd}, process ${liveProcessCwd}`,
        )
      }
      if (canonicalCwd !== rendererContextCwd) {
        throw new Error(
          `Native TUI trust cwd mismatch: requested ${canonicalCwd}, renderer context ${rendererContextCwd ?? '<unbound>'}`,
        )
      }

      const trusted = authority.isWorkspacePathTrusted(canonicalCwd)
      if (trusted) {
        authority.activateInteractiveSession()
        return
      }
      const response = await router.requestSetup(
        {
          subtype: CRABCODE_TUI_SETUP_SUBTYPE,
          protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
          kind: 'workspace_trust',
        },
        CrabCodeTuiWorkspaceTrustResponseSchema,
      )
      if (response.decision === 'reject') {
        throw new NativeTuiWorkspaceTrustDeclinedError(canonicalCwd)
      }
      authority.acceptWorkspace(canonicalCwd, canonicalCwd === canonicalHome)
      authority.activateInteractiveSession()
    },
    async projectRendererScrollSpeed(
      rawValue: string | undefined,
    ): Promise<void> {
      requireRendererContext()
      if (rendererScrollSpeedProjected) {
        throw new Error(
          'Native TUI renderer scroll speed may be projected only once',
        )
      }
      rendererScrollSpeedProjected = true
      await router.requestSetup(
        {
          subtype: CRABCODE_TUI_SETUP_SUBTYPE,
          protocol_version: NATIVE_TUI_RENDERER_PROTOCOL_VERSION,
          kind: 'renderer_scroll_speed',
          raw_value: rawValue ?? null,
        },
        CrabCodeTuiRendererScrollSpeedResponseSchema,
      )
    },
    requestSetup<Response>(
      request: CrabCodeTuiSetupRequest,
      responseSchema: ZodType<Response>,
    ): Promise<Response> {
      requireRendererContext()
      return router.requestSetup(request, responseSchema)
    },
    async finishSetup(): Promise<AsyncIterable<string>> {
      requireRendererContext()
      return router.finishSetup()
    },
  }
}

class NativeTuiSetupLineRouter {
  private readonly pending = new Map<string, PendingExchange>()
  private readonly runtimeInput = new Stream<string>()
  private readonly initialize = deferred<string>()
  private readonly pendingDrainWaiters = new Set<
    ReturnType<typeof deferred<void>>
  >()
  private phase: 'setup' | 'draining' | 'runtime' = 'setup'
  private initializeLine: string | undefined
  private inputEnded = false
  private fatalError: Error | undefined

  constructor(private readonly transport: RendererTransport) {
    // The router can fail before finishSetup() starts awaiting initialize.
    // Attach a rejection observer immediately so that startup failures cannot
    // surface as an unhandled promise rejection.
    void this.initialize.promise.catch(() => {})
    void this.pump()
  }

  abort(error: Error): void {
    this.fail(error)
  }

  requestSetup<Response>(
    request: CrabCodeTuiSetupRequest,
    responseSchema: ZodType<Response>,
  ): Promise<Response> {
    if (this.phase !== 'setup') {
      return Promise.reject(
        new Error('Native TUI setup exchange started after runtime handoff'),
      )
    }
    if (this.fatalError) return Promise.reject(this.fatalError)

    const validatedRequest = CrabCodeTuiSetupRequestSchema.parse(request)
    const requestId = `crabcode-tui-setup-${randomUUID()}`
    const envelope: CrabCodeTuiSetupControlRequest = {
      type: 'control_request',
      request_id: requestId,
      request: validatedRequest,
    }
    const encodedEnvelope = JSON.stringify(envelope)
    const encodedBytes = Buffer.byteLength(encodedEnvelope, 'utf8')
    if (encodedBytes > NATIVE_TUI_MAX_FRAME_BYTES) {
      return Promise.reject(
        new Error(
          `Native TUI setup frame has ${encodedBytes} bytes; Rust transport limit is ${NATIVE_TUI_MAX_FRAME_BYTES}`,
        ),
      )
    }
    const response = new Promise<Response>(
      (resolveResponse, rejectResponse) => {
        this.pending.set(requestId, {
          schema: responseSchema as ZodType<unknown>,
          resolve: value => resolveResponse(value as Response),
          reject: rejectResponse,
        })
      },
    )
    void this.transport
      .writeLine(encodedEnvelope)
      .catch(error => this.fail(asError(error)))
    return response
  }

  async finishSetup(): Promise<AsyncIterable<string>> {
    if (this.phase !== 'setup') {
      throw new Error('Native TUI renderer session was already handed off')
    }
    // Close admission synchronously before awaiting the initialize line or
    // outstanding responses. This prevents a late renderer-only exchange from
    // racing the StructuredIO handoff.
    this.phase = 'draining'
    await this.initialize.promise
    await this.waitForPendingExchanges()
    if (this.fatalError) throw this.fatalError
    if (!this.initializeLine) {
      throw new Error('Native TUI renderer session has no SDK initialize line')
    }

    this.phase = 'runtime'
    this.runtimeInput.enqueue(`${this.initializeLine}\n`)
    if (this.inputEnded) this.runtimeInput.done()
    return this.runtimeInput
  }

  private async pump(): Promise<void> {
    let content = ''
    try {
      for await (const block of this.transport.input) {
        content +=
          typeof block === 'string'
            ? block
            : Buffer.from(block).toString('utf8')
        for (;;) {
          const newline = content.indexOf('\n')
          if (newline < 0) break
          const line = content.slice(0, newline).replace(/\r$/, '')
          content = content.slice(newline + 1)
          this.routeLine(line)
        }
      }
      if (content.length > 0) this.routeLine(content.replace(/\r$/, ''))
      this.inputEnded = true
      if (this.phase === 'runtime') {
        this.runtimeInput.done()
      } else if (!this.initializeLine || this.pending.size > 0) {
        this.fail(
          new Error('Native TUI renderer transport reached EOF during setup'),
        )
      }
    } catch (error) {
      this.fail(asError(error))
    }
  }

  private routeLine(line: string): void {
    if (this.fatalError) return
    if (this.phase === 'runtime') {
      this.runtimeInput.enqueue(`${line}\n`)
      return
    }
    if (line.length === 0) {
      this.fail(
        new Error('Native TUI renderer transport received an empty line'),
      )
      return
    }

    let message: unknown
    try {
      message = JSON.parse(line)
    } catch {
      this.fail(
        new Error('Native TUI renderer transport received invalid JSON'),
      )
      return
    }
    if (isSetupControlResponse(message)) {
      this.routeSetupResponse(message)
      return
    }
    if (isSdkInitializeControlRequest(message)) {
      if (this.initializeLine) {
        this.fail(
          new Error(
            'Native TUI renderer transport received duplicate initialize',
          ),
        )
        return
      }
      this.initializeLine = line
      this.initialize.resolve(line)
      return
    }
    this.fail(
      new Error(
        'Native TUI renderer transport received a non-setup message before handoff',
      ),
    )
  }

  private routeSetupResponse(message: SetupControlResponse): void {
    const exchange = this.pending.get(message.response.request_id)
    if (!exchange) {
      this.fail(new Error('Native TUI setup response correlation mismatch'))
      return
    }
    this.pending.delete(message.response.request_id)
    if (message.response.subtype === 'error') {
      const error = new Error(message.response.error)
      exchange.reject(error)
      this.fail(error)
      return
    }
    try {
      exchange.resolve(exchange.schema.parse(message.response.response))
    } catch (error) {
      exchange.reject(error)
      this.fail(asError(error))
      return
    }
    this.notifyPendingDrain()
  }

  private waitForPendingExchanges(): Promise<void> {
    if (this.fatalError) return Promise.reject(this.fatalError)
    if (this.pending.size === 0) return Promise.resolve()
    const waiter = deferred<void>()
    this.pendingDrainWaiters.add(waiter)
    return waiter.promise
  }

  private notifyPendingDrain(): void {
    if (this.pending.size !== 0) return
    for (const waiter of this.pendingDrainWaiters) waiter.resolve()
    this.pendingDrainWaiters.clear()
  }

  private fail(error: Error): void {
    if (this.fatalError) return
    this.fatalError = error
    this.initialize.reject(error)
    for (const exchange of this.pending.values()) exchange.reject(error)
    this.pending.clear()
    for (const waiter of this.pendingDrainWaiters) waiter.reject(error)
    this.pendingDrainWaiters.clear()
    this.runtimeInput.error(error)
  }
}

/**
 * Measure the complete frame with an identifier whose byte length exactly
 * matches every generated setup request id. Chunk producers use this to find
 * the largest admissible payload without relying on a guessed safety margin.
 */
export function nativeTuiSetupControlFrameByteLength(
  request: CrabCodeTuiSetupRequest,
): number {
  const validatedRequest = CrabCodeTuiSetupRequestSchema.parse(request)
  const envelope: CrabCodeTuiSetupControlRequest = {
    type: 'control_request',
    request_id: NATIVE_TUI_SETUP_REQUEST_ID_SAMPLE,
    request: validatedRequest,
  }
  return Buffer.byteLength(JSON.stringify(envelope), 'utf8')
}

type SetupControlResponse = {
  type: 'control_response'
  response:
    | {
        subtype: 'success'
        request_id: string
        response: unknown
      }
    | {
        subtype: 'error'
        request_id: string
        error: string
      }
}

function isSetupControlResponse(value: unknown): value is SetupControlResponse {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ['type', 'response']) ||
    value.type !== 'control_response'
  ) {
    return false
  }
  const response = value.response
  if (
    !isRecord(response) ||
    typeof response.request_id !== 'string' ||
    response.request_id.length === 0
  ) {
    return false
  }
  if (response.subtype === 'success') {
    return hasExactKeys(response, ['subtype', 'request_id', 'response'])
  }
  return (
    response.subtype === 'error' &&
    typeof response.error === 'string' &&
    hasExactKeys(response, ['subtype', 'request_id', 'error'])
  )
}

function isSdkInitializeControlRequest(value: unknown): boolean {
  if (
    !isRecord(value) ||
    value.type !== 'control_request' ||
    typeof value.request_id !== 'string' ||
    value.request_id.length === 0 ||
    !isRecord(value.request)
  ) {
    return false
  }
  return value.request.subtype === 'initialize'
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function hasExactKeys(
  value: Record<string, unknown>,
  expectedKeys: readonly string[],
): boolean {
  const actualKeys = Object.keys(value)
  return (
    actualKeys.length === expectedKeys.length &&
    expectedKeys.every(key => Object.hasOwn(value, key))
  )
}

function deferred<T>(): {
  promise: Promise<T>
  resolve(value: T | PromiseLike<T>): void
  reject(error: unknown): void
} {
  let resolvePromise!: (value: T | PromiseLike<T>) => void
  let rejectPromise!: (error: unknown) => void
  const promise = new Promise<T>((resolveValue, rejectValue) => {
    resolvePromise = resolveValue
    rejectPromise = rejectValue
  })
  return {
    promise,
    resolve: resolvePromise,
    reject: rejectPromise,
  }
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error))
}

async function canonicalizeWorkspace(path: string): Promise<string> {
  return realpath(resolve(path))
}

async function loadTrustAuthority(): Promise<TrustAuthority> {
  const [configAuthority, runtimeState, settingsAuthority, envAuthority] =
    await Promise.all([
      import('../utils/config.js'),
      import('../bootstrap/state.js'),
      import('../utils/settings/settings.js'),
      import('../utils/envUtils.js'),
    ])
  return {
    isWorkspacePathTrusted: configAuthority.isWorkspacePathTrusted,
    getRendererConfiguration() {
      const config = configAuthority.getGlobalConfig()
      return {
        verbose: config.verbose,
        preferredNotificationChannel: config.preferredNotifChannel,
        messageIdleNotificationThresholdMs:
          config.messageIdleNotifThresholdMs,
        uiLanguage: normalizeUiLanguage(config.uiLanguage),
        themeSetting: normalizeThemeSetting(config.theme),
        syntaxHighlightingDisabled:
          (settingsAuthority.getInitialSettings()
            .syntaxHighlightingDisabled ??
            false) ||
          envAuthority.isEnvDefinedFalsy(
            process.env.CRABCODE_SYNTAX_HIGHLIGHT,
          ),
      }
    },
    acceptWorkspace(cwd, isHomeDirectory) {
      if (!isHomeDirectory) {
        configAuthority.saveProjectConfigForPath(cwd, current => ({
          ...current,
          hasTrustDialogAccepted: true,
        }))
      }
    },
    activateInteractiveSession() {
      runtimeState.setSessionTrustAccepted(true)
    },
  }
}

function normalizeUiLanguage(value: unknown): 'zh-CN' | 'en-US' {
  return value === 'en-US' ? 'en-US' : 'zh-CN'
}

function normalizeThemeSetting(value: unknown): ThemeSetting {
  return typeof value === 'string' &&
    (CRABCODE_TUI_THEME_SETTINGS as readonly string[]).includes(value)
    ? (value as ThemeSetting)
    : 'dark'
}

function createProcessRendererTransport(): RendererTransport {
  return {
    input: process.stdin,
    async writeLine(line: string): Promise<void> {
      if (line.includes('\n') || line.includes('\r')) {
        throw new Error('Native TUI renderer transport accepts one JSON line')
      }
      await new Promise<void>((resolveWrite, rejectWrite) => {
        process.stdout.write(`${line}\n`, error => {
          if (error) rejectWrite(error)
          else resolveWrite()
        })
      })
    },
  }
}
