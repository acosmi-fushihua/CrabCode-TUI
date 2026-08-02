import type { ToolUseContext } from '../../Tool.js'

type SetToolJSX = NonNullable<ToolUseContext['setToolJSX']>

let showBackgroundHint: ((setToolJSX: SetToolJSX) => void) | undefined

export function installPowerShellBackgroundHint(
  presenter: (setToolJSX: SetToolJSX) => void,
): void {
  showBackgroundHint = presenter
}

export function presentPowerShellBackgroundHint(
  setToolJSX: SetToolJSX,
): void {
  showBackgroundHint?.(setToolJSX)
}
