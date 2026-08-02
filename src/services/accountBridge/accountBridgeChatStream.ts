import type { BetaMessageStreamParams } from '../../types/api-types.js'
import { parseAccountBridgeReference } from '../../utils/model/accountBridgeReference.js'
import type { ChatStreamAdapter } from '../acosmi/client.js'
import {
  compatibleEndpointChatStreamAdapter,
  type CompatibleEndpointChatStreamDeps,
} from '../compatibleEndpoint/compatibleEndpointChatStream.js'
import {
  applyCrabCodeThinkingMode,
  assertAccountBridgeThinkingModeSupported,
} from './thinking.js'
import {
  parseAccountBridgeRuntimeAccess,
  type AccountBridgeRuntimeAccess,
  type CrabCodeThinkingMode,
} from './types.js'

export function accountBridgeChatStreamAdapter(
  runtimeAccess: AccountBridgeRuntimeAccess,
  params: BetaMessageStreamParams,
  thinkingMode: CrabCodeThinkingMode,
  deps?: Partial<CompatibleEndpointChatStreamDeps>,
): ChatStreamAdapter {
  const runtime = parseAccountBridgeRuntimeAccess(runtimeAccess)
  if (parseAccountBridgeReference(params.model) !== runtime.route.routeId) {
    throw new Error(
      'accountBridgeChatStreamAdapter: request reference does not match the bound route',
    )
  }
  if (runtime.route.chatRuntimeSupported !== true) {
    throw new Error(
      `accountBridgeChatStreamAdapter: route ${runtime.route.routeId} is not confirmed for chat runtime`,
    )
  }
  if (runtime.route.supportsTools !== true) {
    throw new Error(
      `accountBridgeChatStreamAdapter: route ${runtime.route.routeId} is not confirmed for tools`,
    )
  }
  assertAccountBridgeThinkingModeSupported(runtime.route, thinkingMode)
  return compatibleEndpointChatStreamAdapter(
    {
      protocol: 'anthropic-compatible',
      endpoint: runtime.endpoint,
      modelId: runtime.route.modelId,
      adapterLabel: 'accountBridgeChatStreamAdapter',
      anthropicThinkingPolicy: 'preserve',
      headers: {
        authorization: `Bearer ${runtime.inferenceKey}`,
        'X-Account-Route-Id': runtime.route.routeId,
      },
    },
    applyCrabCodeThinkingMode(params, thinkingMode),
    deps,
  )
}
