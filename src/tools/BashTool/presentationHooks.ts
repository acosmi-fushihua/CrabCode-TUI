import type { SetToolJSXFn } from '../../Tool.js'

let showBackgroundHint: ((setToolJSX: SetToolJSXFn) => void) | undefined

export function installBashBackgroundHint(
  presenter: (setToolJSX: SetToolJSXFn) => void,
): void {
  showBackgroundHint = presenter
}

export function presentBashBackgroundHint(setToolJSX: SetToolJSXFn): void {
  showBackgroundHint?.(setToolJSX)
}
