import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import { accountBridgeChatStreamAdapter } from '../../src/services/accountBridge/accountBridgeChatStream.js'
import { UnsupportedThinkingModeError } from '../../src/services/accountBridge/thinking.js'
import type {
  AccountBridgeRuntimeAccess,
  CrabCodeThinkingMode,
} from '../../src/services/accountBridge/types.js'
import type { BetaMessageStreamParams } from '../../src/types/api-types.js'

const routeId = 'A'.repeat(43)
const inferenceKey = 'K'.repeat(43)
const baseParams = {
  model: `account:${routeId}`,
  max_tokens: 256,
  messages: [{ role: 'user', content: 'hello' }],
  stream: true,
} as unknown as BetaMessageStreamParams

function access(
  supportedThinkingModes: CrabCodeThinkingMode[] = [
    'auto',
    'off',
    'standard',
    'deep',
  ],
): AccountBridgeRuntimeAccess {
  return {
    endpoint: 'http://127.0.0.1:43123/v1/messages',
    inferenceKey,
    route: {
      routeId,
      accountId: 'acct-1',
      connectorId: 'connector-1',
      modelId: 'provider-model-id',
      displayName: null,
      connectorLabel: 'Connector',
      accountLabel: 'Account',
      chatRuntimeSupported: true,
      supportsTools: true,
      supportsThinking: true,
      supportsAdaptiveThinking: true,
      supportsEffort: true,
      supportsMaxEffort: true,
      supportsVision: null,
      supportsJsonMode: null,
      supportedThinkingModes,
      defaultThinkingMode: 'auto',
      contextWindow: null,
      maxOutputTokens: null,
    },
  }
}

async function requestForMode(mode: CrabCodeThinkingMode) {
  let capturedUrl = ''
  let capturedInit: RequestInit | undefined
  const stream = accountBridgeChatStreamAdapter(access(), baseParams, mode, {
    fetch: async (url, init) => {
      capturedUrl = String(url)
      capturedInit = init
      return new Response('data: {"type":"message_stop"}\n\n', { status: 200 })
    },
  })
  for await (const _event of stream) {
    // Drain the adapter without touching any real network.
  }
  return {
    url: capturedUrl,
    headers: capturedInit?.headers as Record<string, string>,
    body: JSON.parse(String(capturedInit?.body)) as Record<string, unknown>,
  }
}

describe('accountBridgeChatStreamAdapter', () => {
  test('uses only the control-worker loopback endpoint and binds the opaque route', async () => {
    const request = await requestForMode('auto')
    expect(request.url).toBe('http://127.0.0.1:43123/v1/messages')
    expect(request.headers['X-Account-Route-Id']).toBe(routeId)
    expect(request.headers.authorization).toBe(`Bearer ${inferenceKey}`)
    expect(request.headers['x-api-key']).toBeUndefined()
    expect(request.body.model).toBe('provider-model-id')
    expect('thinking' in request.body).toBe(false)
    expect('output_config' in request.body).toBe(false)
  })

  test('maps off/standard/deep to the exact Claude Messages boundary', async () => {
    expect((await requestForMode('off')).body).toMatchObject({
      thinking: { type: 'disabled' },
    })
    expect((await requestForMode('standard')).body).toMatchObject({
      thinking: { type: 'adaptive' },
      output_config: { effort: 'high' },
    })
    expect((await requestForMode('deep')).body).toMatchObject({
      thinking: { type: 'adaptive' },
      output_config: { effort: 'max' },
    })
  })

  test('fails loud on capability drift before any fetch', () => {
    let calls = 0
    expect(() =>
      accountBridgeChatStreamAdapter(access(['auto']), baseParams, 'deep', {
        fetch: async () => {
          calls += 1
          return new Response(null, { status: 200 })
        },
      }),
    ).toThrow(UnsupportedThinkingModeError)
    expect(calls).toBe(0)
    try {
      accountBridgeChatStreamAdapter(access(['auto']), baseParams, 'deep')
    } catch (error) {
      expect(error).toMatchObject({ code: 'unsupported_thinking_mode' })
    }
  })

  test('rejects any request reference that is not exactly the bound route', () => {
    expect(() =>
      accountBridgeChatStreamAdapter(
        access(),
        { ...baseParams, model: `account:${'B'.repeat(43)}` },
        'auto',
      ),
    ).toThrow(/does not match the bound route/)
  })

  test('controller cancellation aborts the shared fetch and ends quietly', async () => {
    let observedSignal: AbortSignal | undefined
    let markFetchReady: (() => void) | undefined
    const fetchReady = new Promise<void>(resolve => {
      markFetchReady = resolve
    })
    const stream = accountBridgeChatStreamAdapter(access(), baseParams, 'auto', {
      fetch: async (_url, init) => {
        observedSignal = init?.signal ?? undefined
        markFetchReady?.()
        return new Response(
          new ReadableStream({
            start(controller) {
              observedSignal?.addEventListener(
                'abort',
                () => controller.error(new Error('cancelled by test')),
                { once: true },
              )
            },
          }),
          { status: 200 },
        )
      },
    })
    const drained = (async () => {
      for await (const _event of stream) {
        // No events are expected before cancellation.
      }
    })()
    await fetchReady
    stream.controller.abort()
    await expect(drained).resolves.toBeUndefined()
    expect(observedSignal?.aborted).toBe(true)
  })

  test('source routing order is local → custom → account → dangling → gateway', async () => {
    const source = await readFile(
      new URL('../../src/services/api/queryModel.ts', import.meta.url),
      'utf8',
    )
    const order = [
      'if (isLocalModelReference(params.model))',
      'const customRuntime = resolveCustomModelRuntime(params.model)',
      'const accountRouteId = parseAccountBridgeReference(params.model)',
      'if (isNonGatewayModelReference(params.model))',
      'return chatStreamAdapter(',
    ].map(needle => source.indexOf(needle, source.indexOf('const generator = withRetry')))
    expect(order.every(index => index >= 0)).toBe(true)
    expect(order).toEqual([...order].sort((a, b) => a - b))
    expect(source).toContain(
      'fallbackModel: parseAccountBridgeReference(options.model)',
    )
  })

  test('custom and account wrappers both use the neutral compatible adapter', async () => {
    const custom = await readFile(
      new URL(
        '../../src/services/customModel/customModelChatStream.ts',
        import.meta.url,
      ),
      'utf8',
    )
    const account = await readFile(
      new URL(
        '../../src/services/accountBridge/accountBridgeChatStream.ts',
        import.meta.url,
      ),
      'utf8',
    )
    expect(custom).toContain('compatibleEndpointChatStreamAdapter(')
    expect(account).toContain('compatibleEndpointChatStreamAdapter(')
  })
})
