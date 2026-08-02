import type { Command } from '../types/command.js'

type ActiveCommandLoader = (cwd: string) => Promise<Command[]>

let activeCommandLoader: ActiveCommandLoader | undefined

/**
 * Install the command inventory for the current process surface.
 *
 * Interactive CLI and the process-owned StructuredIO runtime install their
 * own loaders. Keeping setup() behind this tiny registry prevents a shared
 * backend bootstrap from importing the legacy command/UI tree.
 */
export function installActiveCommandLoader(loader: ActiveCommandLoader): void {
  activeCommandLoader = loader
}

export function prefetchActiveCommands(cwd: string): void {
  if (activeCommandLoader) void activeCommandLoader(cwd)
}
