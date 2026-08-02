import {
  isHeadlessBuiltInCommandName,
  isHeadlessSubscriberGatedCommandName,
} from '../../cli/headlessCommands.js'
import {
  processSlashCommandCore,
  type SlashCommandRuntime,
} from './processSlashCommandCore.js'

export {
  formatSkillLoadingMetadata,
  looksLikeCommand,
  processPromptSlashCommand,
} from './processSlashCommandCore.js'

const headlessRuntime: SlashCommandRuntime = {
  isBuiltInCommandName: isHeadlessBuiltInCommandName,
  isSubscriberGatedCommandName:
    isHeadlessSubscriberGatedCommandName,
}

export async function processSlashCommand(
  ...args: Parameters<typeof processSlashCommandCore> extends [
    ...infer Head,
    SlashCommandRuntime,
  ]
    ? Head
    : never
): ReturnType<typeof processSlashCommandCore> {
  return processSlashCommandCore(...args, headlessRuntime)
}
