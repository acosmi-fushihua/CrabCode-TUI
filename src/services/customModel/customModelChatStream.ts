/**
 * Bring-your-own model wrapper around the protocol-neutral compatible
 * endpoint stream implementation. Endpoint resolution and credentials remain
 * custom-model-owned; stream conversion/retry/cancellation is shared with the
 * Account Bridge wrapper.
 */
import type { BetaMessageStreamParams } from '../../types/api-types.js'
import type { ResolvedCustomModelRuntime } from '../../utils/model/customModelResolver.js'
import type { ChatStreamAdapter } from '../acosmi/client.js'
import {
  compatibleEndpointChatStreamAdapter,
  parseCompatibleEndpointRetryAfterMs,
  type CompatibleEndpointChatStreamDeps,
} from '../compatibleEndpoint/compatibleEndpointChatStream.js'
import { resolveCustomModelEndpoint } from './customModelEndpoint.js'

const ANTHROPIC_VERSION = '2023-06-01'

export interface CustomModelChatStreamDeps
  extends CompatibleEndpointChatStreamDeps {}

/** Backward-compatible export retained for existing adapter unit tests/users. */
export const parseRetryAfterMs = parseCompatibleEndpointRetryAfterMs

export function customModelChatStreamAdapter(
  resolved: ResolvedCustomModelRuntime,
  params: BetaMessageStreamParams,
  deps?: Partial<CustomModelChatStreamDeps>,
): ChatStreamAdapter {
  if (!resolved.apiKey) {
    throw new Error(
      'customModelChatStreamAdapter: custom model has no API key — ' +
        'configure one in Settings before selecting this model',
    )
  }
  const isOpenAI = resolved.provider === 'openai-compatible'
  return compatibleEndpointChatStreamAdapter(
    {
      protocol: resolved.provider,
      endpoint: resolveCustomModelEndpoint(resolved.provider, resolved.baseUrl),
      modelId: resolved.modelId,
      adapterLabel: 'customModelChatStreamAdapter',
      debugStreamEnv: 'CRABCODE_CUSTOM_MODEL_DEBUG_STREAM',
      debugLogLabel: 'custom-model-stream',
      anthropicThinkingPolicy: 'custom-explicit',
      headers: isOpenAI
        ? { authorization: `Bearer ${resolved.apiKey}` }
        : {
            'x-api-key': resolved.apiKey,
            'anthropic-version': ANTHROPIC_VERSION,
          },
    },
    params,
    deps,
  )
}
