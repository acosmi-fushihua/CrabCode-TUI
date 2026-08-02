import type { Command } from '../types/command.js'

/**
 * Shared, renderer-independent predicates for the model-visible skill
 * catalog. Command discovery belongs to the active surface; these helpers only
 * classify the already-discovered command objects.
 */
export function filterSkillToolCommands(
  commands: readonly Command[],
): Command[] {
  return commands.filter(
    command =>
      command.type === 'prompt' &&
      !command.disableModelInvocation &&
      command.source !== 'builtin' &&
      (command.loadedFrom === 'bundled' ||
        command.loadedFrom === 'skills' ||
        command.loadedFrom === 'commands_DEPRECATED' ||
        command.hasUserSpecifiedDescription ||
        command.whenToUse),
  )
}

export function filterMcpSkillCommands(
  commands: readonly Command[],
): Command[] {
  return commands.filter(
    command =>
      command.type === 'prompt' &&
      command.loadedFrom === 'mcp' &&
      !command.disableModelInvocation,
  )
}
