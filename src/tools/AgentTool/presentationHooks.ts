import type { ToolUseContext } from '../../Tool.js'

type SetToolJSX = NonNullable<ToolUseContext['setToolJSX']>

let showBackgroundHint: ((setToolJSX: SetToolJSX) => void) | undefined

/**
 * Installs the legacy Ink-only progress hint. The process-owned Agent tool
 * calls this seam without importing React or the old renderer.
 */
export function installAgentBackgroundHint(
  presenter: (setToolJSX: SetToolJSX) => void,
): void {
  showBackgroundHint = presenter
}

export function presentAgentBackgroundHint(setToolJSX: SetToolJSX): void {
  showBackgroundHint?.(setToolJSX)
}
