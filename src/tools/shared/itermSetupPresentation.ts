import type { ToolUseContext } from '../../Tool.js'

export type ItermSetupResult = 'installed' | 'use-tmux' | 'cancelled'
type SetToolJSX = NonNullable<ToolUseContext['setToolJSX']>

let present:
  | ((
      setToolJSX: SetToolJSX,
      tmuxAvailable: boolean,
    ) => Promise<ItermSetupResult>)
  | undefined

export function installItermSetupPresentation(
  presenter: (
    setToolJSX: SetToolJSX,
    tmuxAvailable: boolean,
  ) => Promise<ItermSetupResult>,
): void {
  present = presenter
}

export function presentItermSetup(
  setToolJSX: SetToolJSX,
  tmuxAvailable: boolean,
): Promise<ItermSetupResult> {
  if (!present) {
    return Promise.resolve('cancelled')
  }
  return present(setToolJSX, tmuxAvailable)
}
