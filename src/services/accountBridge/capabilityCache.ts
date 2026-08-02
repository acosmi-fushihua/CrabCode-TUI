import { parseAccountBridgeReference } from '../../utils/model/accountBridgeReference.js'
import {
  parseAccountBridgeModelRouteView,
  type AccountBridgeModelRouteView,
} from './types.js'

const routesById = new Map<string, AccountBridgeModelRouteView>()

export function replaceAccountBridgeRouteCapabilities(
  routes: readonly AccountBridgeModelRouteView[],
): void {
  const parsed = routes.map(parseAccountBridgeModelRouteView)
  routesById.clear()
  for (const route of parsed) routesById.set(route.routeId, route)
}

export function cacheAccountBridgeRouteCapability(
  route: AccountBridgeModelRouteView,
): void {
  const parsed = parseAccountBridgeModelRouteView(route)
  routesById.set(parsed.routeId, parsed)
}

export function clearAccountBridgeRouteCapabilities(): void {
  routesById.clear()
}

/**
 * `undefined` means “not an account reference”; `null` means account route
 * capability is currently unknown. Callers must never borrow managed-model
 * fallback capability for either account result.
 */
export function getAccountBridgeRouteCapability(
  modelReference: string,
): AccountBridgeModelRouteView | null | undefined {
  const routeId = parseAccountBridgeReference(modelReference)
  if (!routeId) {
    return modelReference.startsWith('account:') ? null : undefined
  }
  return routesById.get(routeId) ?? null
}
