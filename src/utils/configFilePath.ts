import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

function isTruthy(value: string | undefined): boolean {
  if (!value) return false
  return ['1', 'true', 'yes', 'on'].includes(value.toLowerCase().trim())
}

/**
 * Resolve the modern CrabCode configuration directory without importing the
 * general environment utility module. This primitive is shared back into
 * envUtils.ts so the pre-trust leaf and the initialized runtime cannot drift.
 */
export function resolveCrabCodeConfigHomeDir(): string {
  const configDir = process.env.CRABCODE_CONFIG_DIR
  if (configDir) {
    return configDir.normalize('NFC')
  }
  const homeBase = process.env.CRABCODE_HOME || homedir()
  return join(homeBase, '.crabcode').normalize('NFC')
}

/**
 * Resolve the legacy global-config suffix without importing OAuth clients or
 * authentication state. constants/oauth.ts re-exports this same primitive.
 */
export function getGlobalConfigOauthSuffix(): string {
  if (process.env.CRABCODE_CUSTOM_OAUTH_URL) {
    return '-custom-oauth'
  }
  if (process.env.USER_TYPE === 'ant') {
    if (isTruthy(process.env.USE_LOCAL_OAUTH)) {
      return '-local-oauth'
    }
    if (isTruthy(process.env.USE_STAGING_OAUTH)) {
      return '-staging-oauth'
    }
  }
  return ''
}

/**
 * Single authority for the legacy global-config file path.
 *
 * The explicit .config.json compatibility file retains precedence. Otherwise
 * CRABCODE_CONFIG_DIR is a full root override, CRABCODE_HOME is a home-base
 * override, and the OS home is the final fallback.
 */
export function resolveGlobalCrabCodeConfigFile(): string {
  const configHome = resolveCrabCodeConfigHomeDir()
  const compatibilityFile = join(configHome, '.config.json')
  if (existsSync(compatibilityFile)) {
    return compatibilityFile
  }
  const base =
    process.env.CRABCODE_CONFIG_DIR || process.env.CRABCODE_HOME || homedir()
  return join(
    base,
    `.crabcode${getGlobalConfigOauthSuffix()}.json`,
  ).normalize('NFC')
}
