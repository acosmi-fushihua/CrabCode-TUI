import type { Command } from '../types/command.js'

type SkillCommandCatalogProvider = {
  getSkillToolCommands(cwd: string): Promise<Command[]>
  getSlashCommandToolSkills(cwd: string): Promise<Command[]>
}

let provider: SkillCommandCatalogProvider | undefined

export function installSkillCommandCatalogProvider(
  value: SkillCommandCatalogProvider,
): void {
  provider = value
}

export function getSkillToolCommandsForActiveSurface(
  cwd: string,
): Promise<Command[]> {
  return provider?.getSkillToolCommands(cwd) ?? Promise.resolve([])
}

export function getSlashCommandToolSkillsForActiveSurface(
  cwd: string,
): Promise<Command[]> {
  return provider?.getSlashCommandToolSkills(cwd) ?? Promise.resolve([])
}
