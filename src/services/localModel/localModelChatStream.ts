/**
 * Local-model chat stream adapter.
 *
 * Bridges a `local:<id>` model reference to the process-managed local
 * inference server. That server speaks the OpenAI-compatible chat completions
 * protocol, so this adapter reuses `customProtocolAdapter`'s OpenAI/Anthropic
 * translation rather than opening an independent direct path: the request is
 * converted with `anthropicToOpenAIChatCompletionRequest` and the SSE response
 * is folded back into Anthropic raw stream events with
 * `OpenAIStreamToAnthropicAdapter`.
 *
 * Scope (PR-9 R1): this module only builds the adapter. Wiring it into
 * `queryModel` is PR-9 R2. The returned value matches the `ChatStreamAdapter`
 * shape from `src/services/acosmi/client.ts`, so R2 can consume a local model
 * exactly like an Acosmi stream.
 *
 * Safety: the inference server is only ever reached over an HTTP loopback URL
 * reported by `localModel/server/status`. A non-loopback host, an https URL,
 * a stopped server, an empty URL, or a model-id mismatch is rejected before
 * any request is sent; the adapter never dials a user-supplied or remote URL.
 */
import type {
  BetaMessageStreamParams,
  BetaRawMessageStreamEvent,
} from '../../types/api-types.js'
import {
  OpenAIStreamToAnthropicAdapter,
  anthropicToOpenAIChatCompletionRequest,
  type OpenAIChatCompletionChunk,
} from '../../utils/model/customProtocolAdapter.js'
import { parseLocalModelReference } from '../../utils/model/localModelReference.js'
import type { ChatStreamAdapter } from '../acosmi/client.js'
import { UntrustedUpstreamError } from '../api/untrustedUpstreamError.js'

/** Loopback hosts the local inference server is allowed to bind to. */
const LOOPBACK_HOSTS = new Set(['127.0.0.1', 'localhost', '[::1]', '::1'])

/** Canonical chat-completions path exposed by the OpenAI-compatible server. */
const CHAT_COMPLETIONS_PATH = '/v1/chat/completions'

export type LocalModelServerStatusResponse = {
  status: {
    state: string
    modelId?: string | null
    url?: string | null
  }
}

let defaultServerStatus:
  | (() => Promise<LocalModelServerStatusResponse>)
  | undefined

/**
 * Installs the surface-owned local-model lifecycle adapter. The ordinary
 * Each runtime surface installs its process-local lifecycle provider.
 */
export function installLocalModelServerStatusProvider(
  provider: (() => Promise<LocalModelServerStatusResponse>) | null,
): void {
  defaultServerStatus = provider ?? undefined
}

/** Injectable dependencies, overridden in tests to avoid a real daemon. */
export interface LocalModelChatStreamDeps {
  /** Reads the current local inference server status. */
  serverStatus: () => Promise<LocalModelServerStatusResponse>
  /** HTTP transport used to POST the chat completion request. */
  fetch: typeof globalThis.fetch
}

function resolveDeps(
  deps: Partial<LocalModelChatStreamDeps> | undefined,
): LocalModelChatStreamDeps {
  return {
    serverStatus:
      deps?.serverStatus ??
      defaultServerStatus ??
      (() =>
        Promise.reject(
          new Error(
            'Local-model lifecycle provider is not installed for this runtime.',
          ),
        )),
    fetch: deps?.fetch ?? globalThis.fetch.bind(globalThis),
  }
}

/**
 * Build a streaming adapter for a `local:<id>` model reference.
 *
 * @throws if `modelReference` is not a well-formed `local:<id>` reference.
 *   Server-status, loopback, and transport failures surface when the returned
 *   async iterator is consumed (mirroring `chatStreamAdapter` in
 *   `src/services/acosmi/client.ts`).
 */
export function localModelChatStreamAdapter(
  modelReference: string,
  params: BetaMessageStreamParams,
  deps?: Partial<LocalModelChatStreamDeps>,
): ChatStreamAdapter {
  const localId = parseLocalModelReference(modelReference)
  if (localId === null) {
    throw new Error(
      'localModelChatStreamAdapter: expected a local:<id> model reference, ' +
        `got ${JSON.stringify(modelReference)}`,
    )
  }
  const resolved = resolveDeps(deps)
  const controller = new AbortController()
  return {
    controller,
    async *[Symbol.asyncIterator]() {
      yield* streamLocalModel(localId, params, resolved, controller)
    },
  }
}

async function* streamLocalModel(
  localId: string,
  params: BetaMessageStreamParams,
  deps: LocalModelChatStreamDeps,
  controller: AbortController,
): AsyncGenerator<unknown> {
  const endpoint = await resolveLoopbackEndpoint(localId, deps)

  // Reuse the OpenAI-compatible request mapping; force streaming and pin the
  // bare local id (params.model is the `local:<id>` reference, which the
  // OpenAI server would not recognise).
  const request = anthropicToOpenAIChatCompletionRequest(params)
  request.stream = true
  // W-CUSTOM-MODEL-PLUS-GATE PR-5（F1）：流式索要 usage（llama-server 支持
  // include_usage；不带则流式响应不含 usage → token 记账恒 0）。
  request.stream_options = { include_usage: true }
  request.model = localId

  let response: Response
  try {
    response = await deps.fetch(endpoint, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        accept: 'text/event-stream',
      },
      body: JSON.stringify(request),
      signal: controller.signal,
    })
  } catch (err) {
    if (controller.signal.aborted) return
    throw err
  }

  if (!response.ok) {
    const detail = await safeReadText(response)
    throw new UntrustedUpstreamError({
      adapterLabel: 'localModelChatStreamAdapter',
      status: response.status,
      detail,
    })
  }

  const body = response.body
  if (!body) {
    throw new Error(
      'localModelChatStreamAdapter: local model server response had no body',
    )
  }

  const reader = body.getReader()
  const decoder = new TextDecoder()
  const adapter = new OpenAIStreamToAnthropicAdapter()
  let buffer = ''
  try {
    while (true) {
      if (controller.signal.aborted) break
      const { done, value } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })
      let newlineIndex: number
      while ((newlineIndex = buffer.indexOf('\n')) !== -1) {
        const line = buffer.slice(0, newlineIndex)
        buffer = buffer.slice(newlineIndex + 1)
        for (const event of acceptSseLine(line, adapter)) yield event
      }
    }
    // Flush any trailing line that arrived without a final newline.
    buffer += decoder.decode()
    for (const event of acceptSseLine(buffer, adapter)) yield event
    // LOW-6: close the Anthropic raw stream even when the OpenAI-compatible
    // SSE body ended without a `finish_reason` chunk. `finalize()` is a no-op
    // when a terminal chunk already emitted `message_stop`.
    if (!controller.signal.aborted) {
      for (const event of adapter.finalize()) yield event
    }
  } catch (err) {
    if (controller.signal.aborted) return
    throw err
  } finally {
    try {
      reader.releaseLock()
    } catch {
      // Reader already released / errored; nothing to clean up.
    }
  }
}

/**
 * Read `localModel/server/status` and resolve the loopback chat-completions
 * endpoint, rejecting anything that is not a running, matching, HTTP-loopback
 * server.
 */
async function resolveLoopbackEndpoint(
  localId: string,
  deps: LocalModelChatStreamDeps,
): Promise<string> {
  const { status } = await deps.serverStatus()

  if (status.state !== 'running') {
    throw new Error(
      `localModelChatStreamAdapter: local model server is not running (state=${status.state})`,
    )
  }
  if (status.modelId !== localId) {
    throw new Error(
      'localModelChatStreamAdapter: local model server is hosting ' +
        `${JSON.stringify(status.modelId ?? null)}, not ${JSON.stringify(localId)}`,
    )
  }

  const rawUrl = typeof status.url === 'string' ? status.url.trim() : ''
  if (rawUrl === '') {
    throw new Error(
      'localModelChatStreamAdapter: local model server status reported no URL',
    )
  }

  let parsed: URL
  try {
    parsed = new URL(rawUrl)
  } catch {
    throw new Error(
      `localModelChatStreamAdapter: local model server URL is not a valid URL: ${JSON.stringify(rawUrl)}`,
    )
  }
  if (parsed.protocol !== 'http:') {
    throw new Error(
      'localModelChatStreamAdapter: local model server URL must use http ' +
        `(loopback only), got ${JSON.stringify(parsed.protocol)}`,
    )
  }
  if (!LOOPBACK_HOSTS.has(parsed.hostname.toLowerCase())) {
    throw new Error(
      'localModelChatStreamAdapter: local model server URL must be loopback, ' +
        `got host ${JSON.stringify(parsed.hostname)}`,
    )
  }

  // Normalise to the origin and append the canonical path so a status.url that
  // already carries a path or query can never redirect the request elsewhere.
  return `${parsed.origin}${CHAT_COMPLETIONS_PATH}`
}

/**
 * Translate one SSE line into Anthropic raw stream events. Non-`data:` lines,
 * blank `data:` payloads, the `[DONE]` sentinel, and non-JSON chunks all yield
 * no events.
 */
function acceptSseLine(
  line: string,
  adapter: OpenAIStreamToAnthropicAdapter,
): BetaRawMessageStreamEvent[] {
  const trimmed = line.trim()
  if (!trimmed.startsWith('data:')) return []
  const data = trimmed.slice('data:'.length).trim()
  if (data === '' || data === '[DONE]') return []
  let chunk: OpenAIChatCompletionChunk
  try {
    chunk = JSON.parse(data) as OpenAIChatCompletionChunk
  } catch {
    return []
  }
  return adapter.accept(chunk)
}

async function safeReadText(response: Response): Promise<string> {
  try {
    return (await response.text()).trim()
  } catch {
    return ''
  }
}
