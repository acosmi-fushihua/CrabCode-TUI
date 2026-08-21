import type { Command } from '../../types/command.js'

export type SlashCommandLoader = (cwd: string) => Promise<Command[]>

/**
 * Apply the route-owned slash-command gate to an executable inventory.
 *
 * The gate is intentionally applied after inventories are combined so a
 * later source (notably MCP) cannot re-enable slash dispatch after startup.
 */
export function commandInventoryForRoute(
  slashCommandsEnabled: boolean,
  ...inventories: readonly (readonly Command[])[]
): Command[] {
  return slashCommandsEnabled ? inventories.flat() : []
}

/**
 * Match the direct backend's canonical-name first-wins execution identity
 * before projecting discovery metadata. Standard callers retain their
 * established inventory and perform any historical dedupe at their own read
 * boundary.
 */
export function executableCommandInventoryForRoute(
  slashCommandsEnabled: boolean,
  directQueryEventDelivery: boolean,
  ...inventories: readonly (readonly Command[])[]
): Command[] {
  const inventory = commandInventoryForRoute(
    slashCommandsEnabled,
    ...inventories,
  )
  if (!directQueryEventDelivery) return inventory
  const claimedNames = new Set<string>()
  return inventory.filter(command => {
    if (claimedNames.has(command.name)) return false
    claimedNames.add(command.name)
    return true
  })
}

/**
 * Apply the same persistent gate to every asynchronous catalog reload.
 * Returning an empty loader also avoids doing discovery work for a disabled
 * surface and keeps plugin, skill, reload, and auth producers consistent.
 */
export function commandLoaderForRoute(
  slashCommandsEnabled: boolean,
  loader: SlashCommandLoader,
): SlashCommandLoader {
  return slashCommandsEnabled ? loader : async () => []
}
