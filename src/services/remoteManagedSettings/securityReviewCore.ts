import {
  DANGEROUS_SHELL_SETTINGS,
  requiresManagedSettingsSecurityReview,
} from '../../utils/managedEnvConstants.js'
import type { SettingsJson } from '../../utils/settings/types.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import {
  installManagedSettingsSecurityReview,
  type SecurityCheckResult,
} from './index.js'

type DangerousShellSetting = (typeof DANGEROUS_SHELL_SETTINGS)[number]

export type DangerousSettings = {
  shellSettings: Partial<Record<DangerousShellSetting, string>>
  envVars: Record<string, string>
  hasHooks: boolean
  hooks?: unknown
}

export function extractDangerousSettings(
  settings: SettingsJson | null | undefined,
): DangerousSettings {
  if (!settings) {
    return {
      shellSettings: {},
      envVars: {},
      hasHooks: false,
    }
  }

  const shellSettings: Partial<Record<DangerousShellSetting, string>> = {}
  for (const key of DANGEROUS_SHELL_SETTINGS) {
    const value = settings[key]
    if (typeof value === 'string' && value.length > 0) {
      shellSettings[key] = value
    }
  }

  const envVars: Record<string, string> = {}
  if (settings.env && typeof settings.env === 'object') {
    for (const [key, value] of Object.entries(settings.env)) {
      if (
        typeof value === 'string' &&
        value.length > 0 &&
        requiresManagedSettingsSecurityReview(key)
      ) {
        envVars[key] = value
      }
    }
  }

  const hasHooks =
    settings.hooks !== undefined &&
    settings.hooks !== null &&
    typeof settings.hooks === 'object' &&
    Object.keys(settings.hooks).length > 0

  return {
    shellSettings,
    envVars,
    hasHooks,
    hooks: hasHooks ? settings.hooks : undefined,
  }
}

export function hasDangerousSettings(dangerous: DangerousSettings): boolean {
  return (
    Object.keys(dangerous.shellSettings).length > 0 ||
    Object.keys(dangerous.envVars).length > 0 ||
    dangerous.hasHooks
  )
}

export function hasDangerousSettingsChanged(
  oldSettings: SettingsJson | null | undefined,
  newSettings: SettingsJson | null | undefined,
): boolean {
  const oldDangerous = extractDangerousSettings(oldSettings)
  const newDangerous = extractDangerousSettings(newSettings)
  if (!hasDangerousSettings(newDangerous)) return false
  if (!hasDangerousSettings(oldDangerous)) return true

  return (
    jsonStringify({
      shellSettings: oldDangerous.shellSettings,
      envVars: oldDangerous.envVars,
      hooks: oldDangerous.hooks,
    }) !==
    jsonStringify({
      shellSettings: newDangerous.shellSettings,
      envVars: newDangerous.envVars,
      hooks: newDangerous.hooks,
    })
  )
}

export function formatDangerousSettingsList(
  dangerous: DangerousSettings,
): string[] {
  return [
    ...Object.keys(dangerous.shellSettings),
    ...Object.keys(dangerous.envVars),
    ...(dangerous.hasHooks ? ['hooks'] : []),
  ]
}

/**
 * The private native-TUI protocol does not yet expose a managed-policy review
 * dialog. Never inherit the generic headless no-op: reject changed
 * shell/env/hook policy without persisting it, while preserving the established
 * backend behavior of continuing with the previous cache (or null).
 */
export function installDirectTuiManagedSettingsSecurityReview(): void {
  installManagedSettingsSecurityReview({
    async check(cachedSettings, newSettings): Promise<SecurityCheckResult> {
      if (
        !newSettings ||
        !hasDangerousSettings(extractDangerousSettings(newSettings)) ||
        !hasDangerousSettingsChanged(cachedSettings, newSettings)
      ) {
        return 'no_check_needed'
      }
      return 'rejected'
    },
    handle(result) {
      if (result !== 'rejected') return true
      process.stderr.write(
        'Security: changed remote-managed shell, environment, or hook settings were rejected; continuing with the previous settings.\n',
      )
      return false
    },
  })
}
