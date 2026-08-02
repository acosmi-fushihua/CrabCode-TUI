import { ACCOUNT_BRIDGE_REFERENCE_PREFIX } from './accountBridgeReference.js'
import { resolveCustomModelRuntime } from './customModelResolver.js'
import { CUSTOM_MODEL_REFERENCE_PREFIX } from './customModelReference.js'
import { LOCAL_MODEL_REFERENCE_PREFIX } from './localModelReference.js'

const RESERVED_NON_GATEWAY_PREFIXES = [
  LOCAL_MODEL_REFERENCE_PREFIX,
  CUSTOM_MODEL_REFERENCE_PREFIX,
  ACCOUNT_BRIDGE_REFERENCE_PREFIX,
] as const

/**
 * Return whether a runtime model value belongs to a non-gateway namespace.
 *
 * Prefix recognition is deliberately fail-closed: even a malformed or
 * dangling reserved reference must never fall through to the managed gateway.
 * The legacy bare custom-model-id path remains covered until that compatibility
 * surface is retired.
 */
export function isNonGatewayModelReference(
  input: string | null | undefined,
): boolean {
  if (typeof input !== 'string') return false
  const trimmed = input.trim()
  if (
    RESERVED_NON_GATEWAY_PREFIXES.some(prefix => trimmed.startsWith(prefix))
  ) {
    return true
  }
  return resolveCustomModelRuntime(trimmed) !== null
}
