import type { Command } from 'src/types/command.js'
import { parseSlashCommand } from 'src/utils/slashCommandParsing.js'

export type CommandCatalogEntry = {
  name: string
  description: string
  argumentHint: string
}

/**
 * Project the ordered command registry onto the existing three-field control
 * catalog without adding a second routing protocol.
 *
 * The backend resolves slash invocations with `Array.find`, so the first
 * visible command whose canonical name or alias matches a token owns that
 * token. Preserve that order here: canonical name first, then aliases in their
 * declared order, with later collisions omitted. `userFacingName()` remains
 * presentation-only and is deliberately not promoted to a routing token.
 *
 * Callers pass an already stable-name-deduplicated command list. Model-only
 * commands retain the historical initialize/reload behavior and are excluded
 * before visible invocation ownership is assigned.
 */
export function projectCommandCatalogEntries(
  commands: readonly Command[],
  formatDescription: (command: Command) => string,
): CommandCatalogEntry[] {
  const claimedInvocationNames = new Set<string>()
  const entries: CommandCatalogEntry[] = []

  for (const command of commands) {
    if (command.userInvocable === false) continue

    const description = formatDescription(command)
    // Plugin Markdown frontmatter is runtime data even though Command's
    // compile-time contract says string. YAML such as
    // `argument-hint: [project-name]` parses as an array and historically
    // leaked through the truthy fallback below into the SDK initialize
    // response. Keep the command and its routing identity, but discard only
    // malformed presentation metadata so the existing required-string wire
    // contract remains exact.
    const argumentHint =
      typeof command.argumentHint === 'string' ? command.argumentHint : ''
    for (const name of [command.name, ...(command.aliases ?? [])]) {
      // Plugin/frontmatter values are runtime data despite Command's static
      // type. Only publish names that the existing slash parser can roundtrip
      // as one exact invocation with no accidental argument tail. This keeps
      // description-only rows out of the renderer without inventing or
      // rewriting a command token.
      if (typeof name !== 'string') continue
      const parsed = parseSlashCommand('/' + name)
      if (!parsed || parsed.commandName !== name || parsed.args !== '') continue
      if (claimedInvocationNames.has(name)) continue
      claimedInvocationNames.add(name)
      entries.push({ name, description, argumentHint })
    }
  }

  return entries
}
