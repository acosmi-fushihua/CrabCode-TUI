let clearActiveCommandCaches: () => void = () => {}

export function installActiveCommandCacheInvalidator(
  invalidator: () => void,
): void {
  clearActiveCommandCaches = invalidator
}

export function clearActiveSurfaceCommandCaches(): void {
  clearActiveCommandCaches()
}
