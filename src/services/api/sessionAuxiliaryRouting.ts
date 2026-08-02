import {
  parseAccountBridgeRuntimeAccess,
  type AccountBridgeRuntimeAccess,
} from '../accountBridge/types.js'
import {
  ACCOUNT_BRIDGE_REFERENCE_PREFIX,
  parseAccountBridgeReference,
} from '../../utils/model/accountBridgeReference.js'
import { isNonGatewayModelReference } from '../../utils/model/nonGatewayModelReference.js'

export type SessionAuxiliaryRoute =
  | {
      kind: 'ready'
      model: string
      accountBridgeRuntimeAccess?: AccountBridgeRuntimeAccess
    }
  | {
      kind: 'skip'
      reason:
        | 'model-unavailable'
        | 'invalid-account-reference'
        | 'account-runtime-access-unavailable'
    }

/**
 * Resolve the model route for transcript-derived, non-essential LLM work such
 * as title generation, compaction, away recap, and tool-use labels.
 *
 * Privacy invariant: when the active session belongs to a non-gateway
 * namespace, the auxiliary request MUST stay on that exact session route. It
 * must never substitute a managed small/fast model. Managed sessions keep the
 * existing preferred-small-model behaviour.
 *
 * Account Bridge is capability-bound: a syntactically valid `account:*`
 * reference is usable only with a private runtime access object for that exact
 * opaque route. Missing, malformed, mismatched, or capability-ineligible
 * access returns `skip`; callers either fail-soft (title/away/tool label) or
 * fail the compact transaction without writing a summary.
 *
 * Custom/local dangling references intentionally return `ready`. The shared
 * query router owns their authoritative registry/runtime checks and fails them
 * before the managed SDK boundary. Keeping the original reference here is what
 * prevents a deleted/disabled entry from being silently replaced by a gateway
 * model.
 */
export function resolveSessionAuxiliaryRoute(input: {
  sessionModel?: string | null
  preferredAuxiliaryModel?: string | null
  accountBridgeRuntimeAccess?: AccountBridgeRuntimeAccess | null
}): SessionAuxiliaryRoute {
  const sessionModel = input.sessionModel?.trim() ?? ''
  const preferredAuxiliaryModel =
    input.preferredAuxiliaryModel?.trim() ?? ''
  const model =
    sessionModel && isNonGatewayModelReference(sessionModel)
      ? sessionModel
      : preferredAuxiliaryModel || sessionModel

  if (!model) return { kind: 'skip', reason: 'model-unavailable' }

  if (!model.startsWith(ACCOUNT_BRIDGE_REFERENCE_PREFIX)) {
    return { kind: 'ready', model }
  }

  const routeId = parseAccountBridgeReference(model)
  if (!routeId) {
    return { kind: 'skip', reason: 'invalid-account-reference' }
  }

  let runtimeAccess: AccountBridgeRuntimeAccess
  try {
    runtimeAccess = parseAccountBridgeRuntimeAccess(
      input.accountBridgeRuntimeAccess,
    )
  } catch {
    return {
      kind: 'skip',
      reason: 'account-runtime-access-unavailable',
    }
  }
  if (
    runtimeAccess.route.routeId !== routeId ||
    runtimeAccess.route.chatRuntimeSupported !== true ||
    runtimeAccess.route.supportsTools !== true
  ) {
    return {
      kind: 'skip',
      reason: 'account-runtime-access-unavailable',
    }
  }
  return {
    kind: 'ready',
    model,
    accountBridgeRuntimeAccess: runtimeAccess,
  }
}
