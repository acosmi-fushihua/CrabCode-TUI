import { getGlobalConfig, saveGlobalConfig } from './config.js'
import { envDynamic } from './envDynamic.js'

function currentTerminalIdentity(): string {
  return envDynamic.terminal || 'unknown'
}

export function hasIdeOnboardingDialogBeenShown(): boolean {
  const config = getGlobalConfig()
  return config.hasIdeOnboardingBeenShown?.[currentTerminalIdentity()] === true
}

export function markIdeOnboardingDialogAsShown(): void {
  if (hasIdeOnboardingDialogBeenShown()) return
  const terminal = currentTerminalIdentity()
  saveGlobalConfig(current => ({
    ...current,
    hasIdeOnboardingBeenShown: {
      ...current.hasIdeOnboardingBeenShown,
      [terminal]: true,
    },
  }))
}
