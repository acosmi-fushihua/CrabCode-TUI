/**
 * Renderer-neutral command compatibility facade.
 *
 * The former module eagerly registered the complete Ink/React command tree.
 * Native TUI execution uses the existing non-interactive command inventory;
 * backend callers keep the same discovery and classification contracts
 * without importing a terminal renderer.
 */
import memoize from 'lodash-es/memoize.js'
import {
  clearHeadlessCommandMemoizationCaches,
  clearHeadlessCommandsCache,
  formatHeadlessCommandDescription,
  getHeadlessBuiltInCommandNames,
  getHeadlessCommands,
  getHeadlessSkillToolCommands,
  getHeadlessSlashCommandToolSkills,
  getHeadlessSubscriberGatedCommandNames,
  meetsHeadlessAvailabilityRequirement,
} from './cli/headlessCommands.js'
import type { Command } from './types/command.js'
import {
  findCommand,
  getCommand,
  getCommandName,
  getComposerExecutionKind,
  hasCommand,
  isCommandEnabled,
  matchesCommandInvocation,
} from './types/command.js'
import { filterMcpSkillCommands } from './utils/skillCommandCatalog.js'
import { dedupeCommandsByStableName } from './utils/commandDedupe.js'
import { logForDebugging } from './utils/debug.js'

export type {
  Command,
  CommandBase,
  CommandResultDisplay,
  ComposerExecutionKind,
  LocalCommandResult,
  LocalJSXCommandContext,
  PromptCommand,
  ResumeEntrypoint,
} from './types/command.js'
export {
  findCommand,
  getCommand,
  getCommandName,
  getComposerExecutionKind,
  hasCommand,
  isCommandEnabled,
  matchesCommandInvocation,
}

export const INTERNAL_ONLY_COMMANDS: readonly Command[] = []

export const builtInCommandNames = memoize(
  (): Set<string> => new Set(getHeadlessBuiltInCommandNames()),
)

export const subscriberGatedCommandNames = memoize(
  (): ReadonlySet<string> =>
    new Set(getHeadlessSubscriberGatedCommandNames()),
)

export function meetsAvailabilityRequirement(command: Command): boolean {
  return meetsHeadlessAvailabilityRequirement(command)
}

export function dedupeCommandsByInvocationName(
  labeledGroups: { source: string; commands: Command[] }[],
): Command[] {
  return dedupeCommandsByStableName(labeledGroups, logForDebugging)
}

export const getCommands = getHeadlessCommands
export const getSkillToolCommands = getHeadlessSkillToolCommands
export const getSlashCommandToolSkills = getHeadlessSlashCommandToolSkills

export function clearCommandMemoizationCaches(): void {
  clearHeadlessCommandMemoizationCaches()
}

export function clearCommandsCache(): void {
  clearHeadlessCommandsCache()
}

export function getMcpSkillCommands(
  mcpCommands: readonly Command[],
): readonly Command[] {
  return filterMcpSkillCommands(mcpCommands)
}

export function formatDescriptionWithSource(command: Command): string {
  return formatHeadlessCommandDescription(command)
}
