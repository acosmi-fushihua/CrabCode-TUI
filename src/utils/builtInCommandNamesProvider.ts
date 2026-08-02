let resolveNames: () => ReadonlySet<string> = () => new Set()

export function installBuiltInCommandNamesProvider(
  provider: () => ReadonlySet<string>,
): void {
  resolveNames = provider
}

export function getActiveBuiltInCommandNames(): ReadonlySet<string> {
  return resolveNames()
}
