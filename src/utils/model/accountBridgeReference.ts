/**
 * Account Bridge runtime-reference syntax.
 *
 * The value after `account:` is a CrabCode-owned opaque route id. It is never
 * a provider id, model id, email address, or other upstream identity. Route
 * producers use an HMAC-SHA256 digest encoded as unpadded base64url, yielding
 * exactly 43 characters without exposing the bound account or model.
 */

export const ACCOUNT_BRIDGE_REFERENCE_PREFIX = 'account:'

const OPAQUE_ACCOUNT_ROUTE_ID_PATTERN = /^[A-Za-z0-9_-]{43}$/

function normalizeOpaqueAccountRouteId(
  routeId: string | null | undefined,
): string | null {
  if (typeof routeId !== 'string') return null
  const trimmed = routeId.trim()
  if (!OPAQUE_ACCOUNT_ROUTE_ID_PATTERN.test(trimmed)) return null
  return trimmed
}

/** Parse a canonical `account:<opaque-route-id>` reference. */
export function parseAccountBridgeReference(
  input: string | null | undefined,
): string | null {
  if (typeof input !== 'string') return null
  const trimmed = input.trim()
  if (!trimmed.startsWith(ACCOUNT_BRIDGE_REFERENCE_PREFIX)) return null
  return normalizeOpaqueAccountRouteId(
    trimmed.slice(ACCOUNT_BRIDGE_REFERENCE_PREFIX.length),
  )
}

/** True only for a well-formed Account Bridge runtime reference. */
export function isAccountBridgeReference(
  input: string | null | undefined,
): boolean {
  return parseAccountBridgeReference(input) !== null
}

/** Render an opaque route id without exposing provider/account/model data. */
export function renderAccountBridgeReference(routeId: string): string {
  const normalized = normalizeOpaqueAccountRouteId(routeId)
  if (!normalized) {
    throw new TypeError(
      'Account Bridge route id must be a 43-character opaque base64url value',
    )
  }
  return `${ACCOUNT_BRIDGE_REFERENCE_PREFIX}${normalized}`
}
