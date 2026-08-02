import { readFileSync } from 'fs'
import { join } from 'path'
import type { AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS } from '../../services/analytics/index.js'
import { canUseCustomModels } from '../entitlements/customModels.js'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'

export type APIProvider = 'acosmi' | 'firstParty' | 'custom' | 'bedrock' | 'vertex' | 'foundry'

export function getAPIProvider(): APIProvider {
  // 1. Explicit env override
  const envProvider = process.env.CRABCODE_MODEL_PROVIDER
  if (envProvider === 'custom') {
    if (canUseCustomProviderFromCurrentAuth()) {
      return 'custom'
    }
  } else if (
    envProvider === 'firstParty' ||
    envProvider === 'acosmi' ||
    envProvider === 'bedrock' ||
    envProvider === 'vertex' ||
    envProvider === 'foundry'
  ) {
    return envProvider
  }

  // 2. Cloud provider env vars
  if (process.env.CRABCODE_USE_BEDROCK) {
    return 'bedrock'
  }
  if (process.env.CRABCODE_USE_VERTEX) {
    return 'vertex'
  }
  if (process.env.CRABCODE_USE_FOUNDRY) {
    return 'foundry'
  }

  // 3. Custom model configured via env
  if (
    process.env.CRABCODE_CUSTOM_BASE_URL &&
    canUseCustomProviderFromCurrentAuth()
  ) {
    return 'custom'
  }

  // 4. OAuth token in env → acosmi
  if (process.env.ACOSMI_AUTH_TOKEN || process.env.CRABCODE_OAUTH_TOKEN) {
    return 'acosmi'
  }

  // 5. OAuth token in credentials file → acosmi
  if (hasAcosmiOAuthCredentials()) {
    return 'acosmi'
  }

  // 6. API Key (from Acosmi) → firstParty
  if (process.env.ACOSMI_API_KEY) {
    return 'firstParty'
  }

  // 7. Default — CrabCode is an Acosmi product
  return 'acosmi'
}

function canUseCustomProviderFromCurrentAuth(): boolean {
  try {
    // Lazy require avoids a static providers.ts -> auth.ts -> providers.ts cycle.
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { getMembershipGateInput } =
      require('../auth.js') as typeof import('../auth.js')
    return canUseCustomModels(getMembershipGateInput())
  } catch {
    return false
  }
}

// ---------------------------------------------------------------------------
// Lightweight sync check for OAuth credentials file.
// Separate from auth.ts to avoid circular dependency (auth.ts → providers.ts).
// ---------------------------------------------------------------------------
let _credentialsChecked = false
let _hasCredentials = false

function hasAcosmiOAuthCredentials(): boolean {
  if (_credentialsChecked) return _hasCredentials
  _credentialsChecked = true
  try {
    // Canonical isolation-dir resolution (CONFIG_DIR > HOME > homedir);
    // previously re-implemented inline and DROPPED CRABCODE_HOME, so a
    // HOME-only isolation run leaked real ~/.crabcode credentials.
    const configDir = getCrabCodeConfigHomeDir()
    const raw = readFileSync(join(configDir, '.credentials.json'), 'utf-8')
    const data = JSON.parse(raw)
    _hasCredentials = !!data?.acosmiOauth?.accessToken
  } catch {
    _hasCredentials = false
  }
  return _hasCredentials
}

/** Invalidate credentials cache after login/logout so getAPIProvider() re-reads. */
export function invalidateCredentialsCache(): void {
  _credentialsChecked = false
  _hasCredentials = false
}

/**
 * Returns true for official (non-custom) providers: 'firstParty' and 'acosmi'.
 * Use this instead of `getAPIProvider() === 'firstParty'` when a feature
 * should be available for both Acosmi direct API and Acosmi platform users.
 */
export function isOfficialProvider(): boolean {
  const p = getAPIProvider()
  return p === 'firstParty' || p === 'acosmi'
}

export function getAPIProviderForStatsig(): AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS {
  return getAPIProvider() as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS
}

/**
 * Check if ACOSMI_BASE_URL is a first-party Acosmi API URL.
 * Returns true if not set (default API) or points to acosmi.com/api/v4
 * (or api-staging.acosmi.com for ant users).
 */
export function isFirstPartyAcosmiBaseUrl(): boolean {
  return isFirstPartyAcosmiHostUrl(process.env.ACOSMI_BASE_URL)
}

/**
 * Returns true if `url` is unset (→ the default first-party API base) or its
 * host is a first-party Acosmi host (`acosmi.com`, or `api-staging.acosmi.com`
 * for ant users).
 *
 * Unlike `isFirstPartyAcosmiBaseUrl` — which only ever inspects the
 * `ACOSMI_BASE_URL` env — this validates an ARBITRARY caller-supplied URL. Use
 * it to decide whether attaching a first-party Acosmi OAuth bearer to a request
 * is safe: a request whose actual host is NOT first-party must never receive
 * the token (e.g. a `TEAM_MEMORY_SYNC_URL` override pointing at a third party).
 */
export function isFirstPartyAcosmiHostUrl(url: string | undefined): boolean {
  if (!url) {
    return true
  }
  try {
    const host = new URL(url).host
    const allowedHosts = ['acosmi.com']
    if (process.env.USER_TYPE === 'ant') {
      allowedHosts.push('api-staging.acosmi.com')
    }
    return allowedHosts.includes(host)
  } catch {
    return false
  }
}
