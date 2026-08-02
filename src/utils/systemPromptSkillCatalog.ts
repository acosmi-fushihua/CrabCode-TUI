import type { Command } from '../types/command.js'

export type SystemPromptSkillCatalogProvider = (
  cwd: string,
) => Promise<Command[]>

let interactiveProvider: SystemPromptSkillCatalogProvider | undefined

export function installInteractiveSystemPromptSkillCatalog(
  provider: SystemPromptSkillCatalogProvider,
): void {
  interactiveProvider = provider
}

export function getSystemPromptSkillCatalog(
  cwd: string,
  rendererFreeFallback: SystemPromptSkillCatalogProvider,
): Promise<Command[]> {
  return (interactiveProvider ?? rendererFreeFallback)(cwd)
}
