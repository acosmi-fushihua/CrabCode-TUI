import type {
  BetaMessageStreamParams,
  BetaRawMessageStreamEvent,
} from '../../types/api-types.js'
import {
  OpenAIStreamToAnthropicAdapter,
  anthropicToOpenAIChatCompletionRequest,
  type OpenAIChatCompletionChunk,
} from '../../utils/model/customProtocolAdapter.js'
import {
  CLI_INTERNAL_BETA_HEADER,
  CRABCODE_20250219_BETA_HEADER,
} from '../../constants/betas.js'
import { logForDebugging } from '../../utils/debug.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import type { ChatStreamAdapter } from '../acosmi/client.js'
import { UntrustedUpstreamError } from '../api/untrustedUpstreamError.js'
import {
  createOpenAIChatCompletionRepairState,
  maybeRepairOpenAIChatCompletionRequest,
} from '../customModel/openAIChatCompletionRepair.js'

const DEFAULT_CONNECT_TIMEOUT_MS = 30_000
const RETRY_AFTER_MAX_MS = 10_000
const RETRY_AFTER_MAX_RETRIES = 2
const ANTHROPIC_VERSION = '2023-06-01'
const BLOCKED_BETA_PREFIXES = ['crabcode-', 'cli-internal-']
const DEBUG_SAMPLE_LINE_LIMIT = 6
const DEBUG_SAMPLE_LINE_MAX_CHARS = 240
const BLOCKED_BETA_HEADERS = new Set(
  [CRABCODE_20250219_BETA_HEADER, CLI_INTERNAL_BETA_HEADER].filter(Boolean),
)

export type CompatibleEndpointProtocol =
  | 'anthropic-compatible'
  | 'openai-compatible'

export interface CompatibleEndpointRuntime {
  protocol: CompatibleEndpointProtocol
  endpoint: string
  modelId: string
  headers?: Readonly<Record<string, string>>
  adapterLabel: string
  /** Opt-in diagnostics; wrappers own the env name and log namespace. */
  debugStreamEnv?: string
  debugLogLabel?: string
  /** Custom BYO preserves its historical explicit-off behavior; account does not. */
  anthropicThinkingPolicy?: 'preserve' | 'custom-explicit'
}

export interface CompatibleEndpointChatStreamDeps {
  fetch: typeof globalThis.fetch
  connectTimeoutMs?: number
  sleep?: (ms: number, signal: AbortSignal) => Promise<void>
}

interface CompatibleEndpointStreamDebugTrace {
  noteResponse(response: Response): void
  noteHttpDetail(detail: string): void
  noteRetry(reason: string): void
  noteChunk(value: Uint8Array): void
  noteLine(line: string, events: BetaRawMessageStreamEvent[]): void
  noteEvents(events: BetaRawMessageStreamEvent[]): void
  finish(
    outcome: string,
    extra?: Record<string, string | number | boolean | null | undefined>,
  ): void
}

function createCompatibleEndpointStreamDebugTrace(
  runtime: CompatibleEndpointRuntime,
): CompatibleEndpointStreamDebugTrace | null {
  if (
    !runtime.debugStreamEnv ||
    !isEnvTruthy(process.env[runtime.debugStreamEnv])
  ) {
    return null
  }

  const startedAt = Date.now()
  const eventTypeCounts = new Map<string, number>()
  const contentTypes = new Set<string>()
  const requestIds = new Set<string>()
  const statuses: string[] = []
  const retryReasons: string[] = []
  const sampleLines: string[] = []
  let attempts = 0
  let rawChunks = 0
  let rawBytes = 0
  let rawLines = 0
  let dataLines = 0
  let parsedDataLines = 0
  let nonJsonDataLines = 0
  let yieldedEvents = 0
  let finished = false

  const addSample = (sample: string): void => {
    if (sampleLines.length >= DEBUG_SAMPLE_LINE_LIMIT) return
    sampleLines.push(truncateDebugSample(sample, DEBUG_SAMPLE_LINE_MAX_CHARS))
  }
  const noteEvents = (events: BetaRawMessageStreamEvent[]): void => {
    yieldedEvents += events.length
    for (const event of events) {
      eventTypeCounts.set(event.type, (eventTypeCounts.get(event.type) ?? 0) + 1)
    }
  }

  return {
    noteResponse(response): void {
      attempts += 1
      statuses.push(String(response.status))
      const contentType = response.headers.get('content-type')
      if (contentType) contentTypes.add(contentType)
      for (const header of [
        'anthropic-request-id',
        'request-id',
        'x-request-id',
      ]) {
        const value = response.headers.get(header)
        if (value) requestIds.add(`${header}=${redactDebugText(value)}`)
      }
    },
    noteHttpDetail(detail): void {
      if (detail.trim()) addSample(`http-detail:${summarizeDebugPayloadText(detail)}`)
    },
    noteRetry(reason): void {
      retryReasons.push(reason)
    },
    noteChunk(value): void {
      rawChunks += 1
      rawBytes += value.byteLength
    },
    noteLine(line, events): void {
      rawLines += 1
      const trimmed = line.trim()
      if (trimmed.startsWith('data:')) {
        dataLines += 1
        const data = trimmed.slice('data:'.length).trim()
        if (data !== '' && data !== '[DONE]') {
          try {
            JSON.parse(data)
            parsedDataLines += 1
          } catch {
            nonJsonDataLines += 1
          }
        }
      }
      if (trimmed !== '') addSample(summarizeDebugLine(line))
      noteEvents(events)
    },
    noteEvents,
    finish(outcome, extra = {}): void {
      if (finished) return
      finished = true
      const eventTypes = [...eventTypeCounts.entries()]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([type, count]) => `${type}:${count}`)
      logForDebugging(
        `[${runtime.debugLogLabel ?? 'compatible-endpoint-stream'}] ${JSON.stringify({
          outcome,
          protocol: runtime.protocol,
          endpoint: summarizeEndpointForDebug(runtime.endpoint),
          modelId: runtime.modelId,
          attempts,
          statuses,
          contentTypes: [...contentTypes],
          requestIds: [...requestIds],
          retryReasons,
          rawChunks,
          rawBytes,
          rawLines,
          dataLines,
          parsedDataLines,
          nonJsonDataLines,
          yieldedEvents,
          eventTypes,
          hasMessageStart: eventTypeCounts.has('message_start'),
          hasContentDelta: eventTypeCounts.has('content_block_delta'),
          hasMessageStop: eventTypeCounts.has('message_stop'),
          durationMs: Date.now() - startedAt,
          sampleLines,
          ...extra,
        })}`,
      )
    },
  }
}

function summarizeEndpointForDebug(endpoint: string): string {
  try {
    const url = new URL(endpoint)
    return `${url.origin}${url.pathname}`
  } catch {
    return '<invalid-endpoint>'
  }
}

function summarizeDebugLine(line: string): string {
  const trimmed = line.trim()
  if (trimmed === '') return '<blank>'
  if (!trimmed.startsWith('data:')) {
    return `line:${truncateDebugSample(redactDebugText(trimmed), DEBUG_SAMPLE_LINE_MAX_CHARS)}`
  }
  const data = trimmed.slice('data:'.length).trim()
  if (data === '') return 'data:<blank>'
  if (data === '[DONE]') return 'data:[DONE]'
  return `data:${summarizeDebugPayloadText(data)}`
}

function summarizeDebugPayloadText(text: string): string {
  try {
    return summarizeDebugPayload(JSON.parse(text))
  } catch {
    return `<non-json len=${text.length}> ${truncateDebugSample(
      redactDebugText(text),
      DEBUG_SAMPLE_LINE_MAX_CHARS,
    )}`
  }
}

function summarizeDebugPayload(payload: unknown): string {
  if (!payload || typeof payload !== 'object') return `<${typeof payload}>`
  const obj = payload as Record<string, unknown>
  const parts: string[] = []
  if (typeof obj.type === 'string') parts.push(`type=${obj.type}`)
  if (typeof obj.event === 'string') parts.push(`event=${obj.event}`)
  if (typeof obj.id === 'string') parts.push(`id=${shortDebugId(obj.id)}`)
  if (typeof obj.model === 'string') parts.push(`model=${obj.model}`)
  if (obj.message && typeof obj.message === 'object') {
    const message = obj.message as Record<string, unknown>
    if (typeof message.model === 'string') parts.push(`message.model=${message.model}`)
    if (typeof message.role === 'string') parts.push(`message.role=${message.role}`)
    if (Array.isArray(message.content)) {
      parts.push(`message.content_blocks=${message.content.length}`)
    }
  }
  if (obj.delta && typeof obj.delta === 'object') {
    parts.push(`delta.keys=${Object.keys(obj.delta as Record<string, unknown>).join(',')}`)
  }
  if (Array.isArray(obj.choices)) {
    parts.push(`choices=${obj.choices.length}`)
    const finishReasons = obj.choices
      .map(choice =>
        choice && typeof choice === 'object'
          ? (choice as Record<string, unknown>).finish_reason
          : undefined,
      )
      .filter((value): value is string => typeof value === 'string')
    if (finishReasons.length > 0) {
      parts.push(`finish_reason=${finishReasons.join(',')}`)
    }
    const deltaKeys = obj.choices.flatMap(choice => {
      if (!choice || typeof choice !== 'object') return []
      const delta = (choice as Record<string, unknown>).delta
      return delta && typeof delta === 'object'
        ? Object.keys(delta as Record<string, unknown>)
        : []
    })
    if (deltaKeys.length > 0) {
      parts.push(`choice_delta.keys=${[...new Set(deltaKeys)].join(',')}`)
    }
  }
  if (obj.error && typeof obj.error === 'object') {
    const error = obj.error as Record<string, unknown>
    if (typeof error.type === 'string') parts.push(`error.type=${error.type}`)
    if (typeof error.code === 'string') parts.push(`error.code=${error.code}`)
    if (typeof error.message === 'string') {
      parts.push(
        `error.message=${truncateDebugSample(redactDebugText(error.message), 120)}`,
      )
    }
  }
  parts.push(`keys=${Object.keys(obj).slice(0, 12).join(',')}`)
  return `{${parts.join(' ')}}`
}

function shortDebugId(value: string): string {
  return value.length <= 18 ? value : `${value.slice(0, 12)}...${value.slice(-4)}`
}

function redactDebugText(text: string): string {
  return text
    .replace(/Bearer\s+[A-Za-z0-9._~+/=-]+/gi, 'Bearer <redacted>')
    .replace(/sk-[A-Za-z0-9._-]{8,}/g, 'sk-<redacted>')
    .replace(
      /"(api[_-]?key|x-api-key|authorization|inference[_-]?key)"\s*:\s*"[^"]*"/gi,
      '"$1":"<redacted>"',
    )
}

function truncateDebugSample(text: string, maxChars: number): string {
  return text.length <= maxChars
    ? text
    : `${text.slice(0, maxChars)}...<truncated>`
}

function summarizeDebugError(error: unknown): string {
  if (error instanceof Error) {
    return `${error.name}:${truncateDebugSample(redactDebugText(error.message), 160)}`
  }
  return truncateDebugSample(redactDebugText(String(error)), 160)
}

function resolveConnectTimeoutMs(): number {
  const raw =
    process.env.CRABCODE_COMPATIBLE_ENDPOINT_CONNECT_TIMEOUT_MS ??
    process.env.CRABCODE_CUSTOM_MODEL_CONNECT_TIMEOUT_MS
  if (raw) {
    const value = Number(raw)
    if (Number.isFinite(value) && value > 0) return value
  }
  return DEFAULT_CONNECT_TIMEOUT_MS
}

export function parseCompatibleEndpointRetryAfterMs(
  header: string | null,
): number | null {
  if (header === null) return null
  const trimmed = header.trim()
  if (trimmed === '') return null
  if (/^\d+$/.test(trimmed)) return Number(trimmed) * 1000
  const dateMs = Date.parse(trimmed)
  if (Number.isNaN(dateMs)) return null
  return Math.max(0, dateMs - Date.now())
}

function resolveDeps(
  deps: Partial<CompatibleEndpointChatStreamDeps> | undefined,
): CompatibleEndpointChatStreamDeps {
  return {
    fetch: deps?.fetch ?? globalThis.fetch.bind(globalThis),
    connectTimeoutMs: deps?.connectTimeoutMs,
    sleep: deps?.sleep,
  }
}

function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise(resolve => {
    const timer = setTimeout(done, ms)
    function done(): void {
      clearTimeout(timer)
      signal.removeEventListener('abort', done)
      resolve()
    }
    signal.addEventListener('abort', done, { once: true })
  })
}

function filterAnthropicBetas(betas: readonly string[] | undefined): string[] {
  if (!Array.isArray(betas)) return []
  const result: string[] = []
  const seen = new Set<string>()
  for (const beta of betas) {
    const normalized = beta.trim()
    if (
      normalized === '' ||
      BLOCKED_BETA_HEADERS.has(normalized) ||
      BLOCKED_BETA_PREFIXES.some(prefix => normalized.startsWith(prefix)) ||
      seen.has(normalized)
    ) {
      continue
    }
    seen.add(normalized)
    result.push(normalized)
  }
  return result
}

function normalizeCustomAnthropicThinking(body: Record<string, unknown>): void {
  const thinkingType = (body.thinking as { type?: unknown } | undefined)?.type
  if (thinkingType === 'enabled' || thinkingType === 'adaptive') {
    body.thinking = { type: 'adaptive' }
    return
  }
  body.thinking = { type: 'disabled' }
  const outputConfig = body.output_config as Record<string, unknown> | undefined
  if (outputConfig && 'effort' in outputConfig) {
    delete outputConfig.effort
    if (Object.keys(outputConfig).length === 0) delete body.output_config
  }
}

async function fetchWithFirstByteTimeout(
  fetchFn: typeof globalThis.fetch,
  runtime: CompatibleEndpointRuntime,
  init: RequestInit,
  outerSignal: AbortSignal,
  timeoutMs: number,
): Promise<{ response: Response; detachAbortForwarder: () => void }> {
  const inner = new AbortController()
  const forwardAbort = (): void => inner.abort(outerSignal.reason)
  if (outerSignal.aborted) inner.abort(outerSignal.reason)
  outerSignal.addEventListener('abort', forwardAbort, { once: true })
  const timer = setTimeout(() => {
    inner.abort(
      new Error(
        `${runtime.adapterLabel}: no response from provider within ${timeoutMs}ms (connect timeout)`,
      ),
    )
  }, timeoutMs)
  let handedOff = false
  try {
    const response = await fetchFn(runtime.endpoint, {
      ...init,
      signal: inner.signal,
    })
    handedOff = true
    return {
      response,
      detachAbortForwarder: () =>
        outerSignal.removeEventListener('abort', forwardAbort),
    }
  } catch (error) {
    if (inner.signal.aborted && !outerSignal.aborted) {
      throw inner.signal.reason instanceof Error ? inner.signal.reason : error
    }
    throw error
  } finally {
    clearTimeout(timer)
    // Keep forwarding user cancellation after headers arrive: the response
    // body is still owned by this fetch signal. The stream consumer detaches
    // only after the body finishes; failed pre-header fetches detach here.
    if (!handedOff) outerSignal.removeEventListener('abort', forwardAbort)
  }
}

function buildRequest(
  runtime: CompatibleEndpointRuntime,
  params: BetaMessageStreamParams,
): { headers: Record<string, string>; body: Record<string, unknown> } {
  const isOpenAI = runtime.protocol === 'openai-compatible'
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    accept: 'text/event-stream',
    ...runtime.headers,
  }
  if (isOpenAI) {
    return {
      headers,
      body: {
        ...anthropicToOpenAIChatCompletionRequest({ ...params, stream: true }),
        model: runtime.modelId,
      },
    }
  }

  const { betas, ...rest } = params
  const forwardedBetas = filterAnthropicBetas(betas)
  if (forwardedBetas.length > 0) {
    headers['anthropic-beta'] = forwardedBetas.join(',')
  }
  if (!('anthropic-version' in headers)) {
    headers['anthropic-version'] = ANTHROPIC_VERSION
  }
  const body: Record<string, unknown> = {
    ...rest,
    model: runtime.modelId,
    stream: true,
  }
  if (runtime.anthropicThinkingPolicy === 'custom-explicit') {
    normalizeCustomAnthropicThinking(body)
  }
  return { headers, body }
}

/**
 * Protocol-neutral compatible endpoint adapter shared by custom and account
 * wrappers. Credentials and endpoint lifecycle remain wrapper-owned.
 */
export function compatibleEndpointChatStreamAdapter(
  runtime: CompatibleEndpointRuntime,
  params: BetaMessageStreamParams,
  deps?: Partial<CompatibleEndpointChatStreamDeps>,
): ChatStreamAdapter {
  const transport = resolveDeps(deps)
  const controller = new AbortController()
  return {
    controller,
    async *[Symbol.asyncIterator]() {
      yield* streamCompatibleEndpoint(runtime, params, transport, controller)
    },
  }
}

async function* streamCompatibleEndpoint(
  runtime: CompatibleEndpointRuntime,
  params: BetaMessageStreamParams,
  deps: CompatibleEndpointChatStreamDeps,
  controller: AbortController,
): AsyncGenerator<unknown> {
  const isOpenAI = runtime.protocol === 'openai-compatible'
  const request = buildRequest(runtime, params)
  const debugTrace = createCompatibleEndpointStreamDebugTrace(runtime)
  let body = request.body
  const connectTimeoutMs = deps.connectTimeoutMs ?? resolveConnectTimeoutMs()
  const sleep = deps.sleep ?? abortableSleep
  const openAIRepairState = createOpenAIChatCompletionRepairState()
  let retryAfterRetries = 0
  let response: Response
  let detachResponseAbort: (() => void) | null = null

  for (;;) {
    try {
      const result = await fetchWithFirstByteTimeout(
        deps.fetch,
        runtime,
        {
          method: 'POST',
          headers: request.headers,
          body: JSON.stringify(body),
        },
        controller.signal,
        connectTimeoutMs,
      )
      response = result.response
      detachResponseAbort = result.detachAbortForwarder
    } catch (error) {
      if (controller.signal.aborted) {
        debugTrace?.finish('aborted')
        return
      }
      debugTrace?.finish('fetch_error', {
        error: summarizeDebugError(error),
      })
      throw error
    }
    debugTrace?.noteResponse(response)
    if (response.ok) break

    const detail = await safeReadText(response)
    debugTrace?.noteHttpDetail(detail)
    detachResponseAbort()
    detachResponseAbort = null
    if (response.status === 400 && isOpenAI) {
      const repair = maybeRepairOpenAIChatCompletionRequest(
        body,
        detail,
        openAIRepairState,
      )
      if (repair) {
        body = repair.body
        debugTrace?.noteRetry(repair.reason)
        continue
      }
    }
    if (
      (response.status === 429 || response.status === 503) &&
      retryAfterRetries < RETRY_AFTER_MAX_RETRIES
    ) {
      const retryAfterMs = parseCompatibleEndpointRetryAfterMs(
        response.headers.get('retry-after'),
      )
      if (retryAfterMs !== null && retryAfterMs <= RETRY_AFTER_MAX_MS) {
        retryAfterRetries += 1
        debugTrace?.noteRetry(
          `retry_after_${response.status}_${retryAfterMs}ms`,
        )
        await sleep(retryAfterMs, controller.signal)
        if (controller.signal.aborted) {
          debugTrace?.finish('aborted')
          return
        }
        continue
      }
    }
    debugTrace?.finish('http_error', { status: response.status })
    throw new UntrustedUpstreamError({
      adapterLabel: runtime.adapterLabel,
      status: response.status,
      detail,
    })
  }

  if (!response.body) {
    detachResponseAbort?.()
    debugTrace?.finish('no_body')
    throw new Error(`${runtime.adapterLabel}: provider response had no body`)
  }
  const reader = response.body.getReader()
  const decoder = new TextDecoder()
  const adapter = isOpenAI ? new OpenAIStreamToAnthropicAdapter() : null
  let buffer = ''
  try {
    while (!controller.signal.aborted) {
      const { done, value } = await reader.read()
      if (done) break
      debugTrace?.noteChunk(value)
      buffer += decoder.decode(value, { stream: true })
      let newlineIndex: number
      while ((newlineIndex = buffer.indexOf('\n')) !== -1) {
        const line = buffer.slice(0, newlineIndex)
        buffer = buffer.slice(newlineIndex + 1)
        const events = acceptSseLine(line, adapter)
        debugTrace?.noteLine(line, events)
        for (const event of events) yield event
      }
    }
    buffer += decoder.decode()
    const trailingEvents = acceptSseLine(buffer, adapter)
    debugTrace?.noteLine(buffer, trailingEvents)
    for (const event of trailingEvents) yield event
    if (adapter && !controller.signal.aborted) {
      const finalizeEvents = adapter.finalize()
      debugTrace?.noteEvents(finalizeEvents)
      for (const event of finalizeEvents) yield event
    }
  } catch (error) {
    if (controller.signal.aborted) {
      debugTrace?.finish('aborted')
      return
    }
    debugTrace?.finish('read_error', {
      error: summarizeDebugError(error),
    })
    throw error
  } finally {
    try {
      reader.releaseLock()
    } catch {
      // Reader already released or errored.
    }
    detachResponseAbort?.()
    debugTrace?.finish(controller.signal.aborted ? 'aborted' : 'completed')
  }
}

function acceptSseLine(
  line: string,
  adapter: OpenAIStreamToAnthropicAdapter | null,
): BetaRawMessageStreamEvent[] {
  const trimmed = line.trim()
  if (!trimmed.startsWith('data:')) return []
  const data = trimmed.slice('data:'.length).trim()
  if (data === '' || data === '[DONE]') return []
  let parsed: unknown
  try {
    parsed = JSON.parse(data)
  } catch {
    return []
  }
  return adapter
    ? adapter.accept(parsed as OpenAIChatCompletionChunk)
    : [parsed as BetaRawMessageStreamEvent]
}

async function safeReadText(response: Response): Promise<string> {
  try {
    return (await response.text()).trim()
  } catch {
    return ''
  }
}
