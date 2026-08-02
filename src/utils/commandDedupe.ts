export type NamedCommandGroup<T extends { name: string }> = {
  source: string
  commands: T[]
}

/**
 * Deduplicate commands by their stable machine identity (`cmd.name`).
 *
 * Presentation metadata such as `userFacingName()` is deliberately outside
 * this pure helper: localized labels may collide without making either
 * plugin-prefixed invocation token unreachable. The first stable-name match
 * wins and every shadowed entry is reported through the injected logger.
 */
export function dedupeCommandsByStableName<T extends { name: string }>(
  labeledGroups: NamedCommandGroup<T>[],
  logCollision: (message: string) => void,
): T[] {
  const result: T[] = []
  const keptBy = new Map<string, string>()

  for (const { source, commands } of labeledGroups) {
    for (const command of commands) {
      const stableName = command.name
      const existingSource = keptBy.get(stableName)
      if (existingSource !== undefined) {
        logCollision(
          `Command name collision: "${stableName}" from "${source}" is ` +
            `shadowed by the same-named command from "${existingSource}" ` +
            `(first-wins). The "${source}" entry is unreachable.`,
        )
        continue
      }
      keptBy.set(stableName, source)
      result.push(command)
    }
  }

  return result
}
