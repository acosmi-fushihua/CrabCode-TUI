import type { Tool } from '../Tool.js'

export type ToolPresentationKey =
  | 'renderToolResultMessage'
  | 'renderToolUseMessage'
  | 'renderToolUseTag'
  | 'renderToolUseProgressMessage'
  | 'renderToolUseQueuedMessage'
  | 'renderToolUseRejectedMessage'
  | 'renderToolUseErrorMessage'
  | 'renderGroupedToolUse'

type ToolPresentation = Partial<Pick<Tool, ToolPresentationKey>>

const presentations = new Map<string, ToolPresentation>()

/**
 * Register legacy interactive presentation functions without making them a
 * dependency of the process-owned tool implementation. The native Rust TUI
 * never installs this registry; it receives structured tool events instead.
 */
export function registerToolPresentation(
  toolName: string,
  presentation: ToolPresentation,
): void {
  presentations.set(toolName, presentation)
}

/**
 * Build only the delegates a tool historically exposed. Preserving the exact
 * key set matters because the interactive renderer uses property presence to
 * decide whether to fall back to its generic rejected/error presentation.
 */
export function createToolPresentationDelegates<
  K extends ToolPresentationKey,
>(toolName: string, keys: readonly K[]): Pick<Tool, K> {
  const delegates: Partial<Record<ToolPresentationKey, unknown>> = {}
  for (const key of keys) {
    delegates[key] = (...args: unknown[]) => {
      const presentation = presentations.get(toolName)
      const handler = presentation?.[key]
      if (typeof handler !== 'function') return null
      return (handler as (...values: unknown[]) => unknown)(...args)
    }
  }
  return delegates as Pick<Tool, K>
}
