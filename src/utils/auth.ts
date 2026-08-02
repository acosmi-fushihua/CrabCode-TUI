import chalk from 'chalk'
import { execa } from 'execa'
import { statSync } from 'fs'
import { mkdir, stat } from 'fs/promises'
import memoize from 'lodash-es/memoize.js'
import { join } from 'path'
import { ACOSMI_PROFILE_SCOPE } from 'src/constants/oauth.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'
import { isOfficialProvider } from 'src/utils/model/providers.js'
import {
  getIsNonInteractiveSession,
  preferThirdPartyAuthentication,
} from '../bootstrap/state.js'
import {
  isOAuthTokenExpired,
  refreshOAuthToken,
  shouldUseAcosmiAuth,
} from '../services/oauth/client.js'
import { getOauthProfileFromOauthToken } from '../services/oauth/getOauthProfile.js'
import type { OAuthTokens, SubscriptionType } from '../services/oauth/types.js'
import {
  getApiKeyFromFileDescriptor,
  getOAuthTokenFromFileDescriptor,
} from './authFileDescriptor.js'
import {
  maybeRemoveApiKeyFromMacOSKeychainThrows,
  normalizeApiKeyForConfig,
} from './authPortable.js'
import { clearBetasCaches } from './betas.js'
import {
  type AccountInfo,
  checkHasTrustDialogAccepted,
  getGlobalConfig,
  saveGlobalConfig,
} from './config.js'
import { logAntError, logForDebugging } from './debug.js'
import {
  getCrabCodeConfigHomeDir,
  isBareMode,
  isEnvTruthy,
  isRunningOnHomespace,
} from './envUtils.js'
import { errorMessage } from './errors.js'
import { execSyncWithDefaults_DEPRECATED } from './execFileNoThrow.js'
import * as lockfile from './lockfile.js'
import { logError } from './log.js'
import { getSecureStorage } from './secureStorage/index.js'
import {
  clearLegacyApiKeyPrefetch,
  getLegacyApiKeyPrefetchResult,
} from './secureStorage/keychainPrefetch.js'
import { SECURITY_STDIN_LINE_LIMIT } from './secureStorage/keychainChunking.js'
import {
  clearKeychainCache,
  getMacOsKeychainStorageServiceName,
  getUsername,
} from './secureStorage/macOsKeychainHelpers.js'
import type {
  SecureStorageData,
  SecureStorageWriteResult,
} from './secureStorage/types.js'
import { writeFileSyncAtomicNoFallback } from './file.js'

export { clearKeychainCache }
import {
  getSettings_DEPRECATED,
  getSettingsForSource,
} from './settings/settings.js'
import { sleep } from './sleep.js'
import { jsonParse } from './slowOperations.js'
import { clearToolSchemaCache } from './toolSchemaCache.js'

// ---------------------------------------------------------------------------
// Bridge auth cache — filled by startup prefetch, consumed by isAcosmiSubscriber
// ---------------------------------------------------------------------------

import type { BridgeAuthStatus } from '../types/auth.js'

let _bridgeAuthCache: BridgeAuthStatus | null = null

export function setBridgeAuthCache(status: BridgeAuthStatus): void {
  _bridgeAuthCache = status
}

export function getBridgeAuthCache(): BridgeAuthStatus | null {
  return _bridgeAuthCache
}

/** Default TTL for API key helper cache in milliseconds (5 minutes) */
const DEFAULT_API_KEY_HELPER_TTL = 5 * 60 * 1000

/**
 * CCR and CrabCode Desktop spawn the CLI with OAuth and should never fall back
 * to the user's ~/.crabcode/settings.json API-key config (apiKeyHelper,
 * env.ACOSMI_API_KEY, env.ACOSMI_AUTH_TOKEN). Those settings exist for
 * the user's terminal CLI, not managed sessions. Without this guard, a user
 * who runs `crabcode` in their terminal with an API key sees every CCD session
 * also use that key — and fail if it's stale/wrong-org.
 */
function isManagedOAuthContext(): boolean {
  return (
    isEnvTruthy(process.env.CRABCODE_REMOTE) ||
    process.env.CRABCODE_ENTRYPOINT === 'crabcode-desktop'
  )
}

/** Whether we are supporting direct 1P auth. */
// this code is closely related to getAuthTokenSource
export function isAcosmiAuthEnabled(): boolean {
  // --bare: API-key-only, never OAuth.
  if (isBareMode()) return false

  // `crabcode ssh` remote: ACOSMI_UNIX_SOCKET tunnels API calls through a
  // local auth-injecting proxy. The launcher sets CRABCODE_OAUTH_TOKEN as a
  // placeholder iff the local side is a subscriber (so the remote includes the
  // oauth-2025 beta header to match what the proxy will inject). The remote's
  // ~/.crabcode settings (apiKeyHelper, settings.env.ACOSMI_API_KEY) MUST NOT
  // flip this — they'd cause a header mismatch with the proxy and a bogus
  // "invalid x-api-key" from the API. See src/ssh/sshAuthProxy.ts.
  if (process.env.ACOSMI_UNIX_SOCKET) {
    return !!process.env.CRABCODE_OAUTH_TOKEN
  }

  // Check if user has configured an external API key source
  // This allows externally-provided API keys to work (without requiring proxy configuration)
  const settings = getSettings_DEPRECATED() || {}
  const apiKeyHelper = settings.apiKeyHelper
  const hasExternalAuthToken =
    process.env.ACOSMI_AUTH_TOKEN ||
    apiKeyHelper ||
    process.env.CRABCODE_API_KEY_FILE_DESCRIPTOR

  // Check if API key is from an external source (not managed by /login)
  const { source: apiKeySource } = getAcosmiApiKeyWithSource({
    skipRetrievingKeyFromApiKeyHelper: true,
  })
  const hasExternalApiKey =
    apiKeySource === 'ACOSMI_API_KEY' || apiKeySource === 'apiKeyHelper'

  // Disable Acosmi auth if:
  // 1. User has an external API key (regardless of proxy configuration)
  // 2. User has an external auth token (regardless of proxy configuration)
  // this may cause issues if users have complex proxy / gateway "client-side creds" auth scenarios,
  // e.g. if they want to set X-Api-Key to a gateway key but use Acosmi OAuth for the Authorization
  // if we get reports of that, we should probably add an env var to force OAuth enablement
  const shouldDisableAuth =
    (hasExternalAuthToken && !isManagedOAuthContext()) ||
    (hasExternalApiKey && !isManagedOAuthContext())

  return !shouldDisableAuth
}

/** Where the auth token is being sourced from, if any. */
// this code is closely related to isAcosmiAuthEnabled
export function getAuthTokenSource() {
  // --bare: API-key-only. apiKeyHelper (from --settings) is the only
  // bearer-token-shaped source allowed. OAuth env vars, FD tokens, and
  // keychain are ignored.
  if (isBareMode()) {
    if (getConfiguredApiKeyHelper()) {
      return { source: 'apiKeyHelper' as const, hasToken: true }
    }
    return { source: 'none' as const, hasToken: false }
  }

  if (process.env.ACOSMI_AUTH_TOKEN && !isManagedOAuthContext()) {
    return { source: 'ACOSMI_AUTH_TOKEN' as const, hasToken: true }
  }

  if (process.env.CRABCODE_OAUTH_TOKEN) {
    return { source: 'CRABCODE_OAUTH_TOKEN' as const, hasToken: true }
  }

  // Check for OAuth token from file descriptor (or its CCR disk fallback)
  const oauthTokenFromFd = getOAuthTokenFromFileDescriptor()
  if (oauthTokenFromFd) {
    // getOAuthTokenFromFileDescriptor has a disk fallback for CCR subprocesses
    // that can't inherit the pipe FD. Distinguish by env var presence so the
    // org-mismatch message doesn't tell the user to unset a variable that
    // doesn't exist. Call sites fall through correctly — the new source is
    // !== 'none' (cli/handlers/auth.ts → oauth_token) and not in the
    // isEnvVarToken set (auth.ts:1844 → generic re-login message).
    if (process.env.CRABCODE_OAUTH_TOKEN_FILE_DESCRIPTOR) {
      return {
        source: 'CRABCODE_OAUTH_TOKEN_FILE_DESCRIPTOR' as const,
        hasToken: true,
      }
    }
    return {
      source: 'CCR_OAUTH_TOKEN_FILE' as const,
      hasToken: true,
    }
  }

  // Check if apiKeyHelper is configured without executing it
  // This prevents security issues where arbitrary code could execute before trust is established
  const apiKeyHelper = getConfiguredApiKeyHelper()
  if (apiKeyHelper && !isManagedOAuthContext()) {
    return { source: 'apiKeyHelper' as const, hasToken: true }
  }

  const oauthTokens = getAcosmiOAuthTokens()
  if (shouldUseAcosmiAuth(oauthTokens?.scopes) && oauthTokens?.accessToken) {
    return { source: 'acosmi.com' as const, hasToken: true }
  }

  return { source: 'none' as const, hasToken: false }
}

export type ApiKeySource =
  | 'ACOSMI_API_KEY'
  | 'apiKeyHelper'
  | '/login managed key'
  | 'none'

export function getAcosmiApiKey(): null | string {
  const { key } = getAcosmiApiKeyWithSource()
  return key
}

export function hasAcosmiApiKeyAuth(): boolean {
  const { key, source } = getAcosmiApiKeyWithSource({
    skipRetrievingKeyFromApiKeyHelper: true,
  })
  return key !== null && source !== 'none'
}

export function getAcosmiApiKeyWithSource(
  opts: { skipRetrievingKeyFromApiKeyHelper?: boolean } = {},
): {
  key: null | string
  source: ApiKeySource
} {
  // --bare: hermetic auth. Only ACOSMI_API_KEY env or apiKeyHelper from
  // the --settings flag. Never touches keychain, config file, or approval
  // lists. 3P (Bedrock/Vertex/Foundry) uses provider creds, not this path.
  if (isBareMode()) {
    if (process.env.ACOSMI_API_KEY) {
      return { key: process.env.ACOSMI_API_KEY, source: 'ACOSMI_API_KEY' }
    }
    if (getConfiguredApiKeyHelper()) {
      return {
        key: opts.skipRetrievingKeyFromApiKeyHelper
          ? null
          : getApiKeyFromApiKeyHelperCached(),
        source: 'apiKeyHelper',
      }
    }
    return { key: null, source: 'none' }
  }

  // On homespace, don't use ACOSMI_API_KEY (use Console key instead)
  // https://acosmi.slack.com/archives/C08428WSLKV/p1747331773214779
  const apiKeyEnv = isRunningOnHomespace()
    ? undefined
    : process.env.ACOSMI_API_KEY

  // Always check for direct environment variable when the user ran crabcode --print.
  // This is useful for CI, etc.
  if (preferThirdPartyAuthentication() && apiKeyEnv) {
    return {
      key: apiKeyEnv,
      source: 'ACOSMI_API_KEY',
    }
  }

  if (isEnvTruthy(process.env.CI) || process.env.NODE_ENV === 'test') {
    // Check for API key from file descriptor first
    const apiKeyFromFd = getApiKeyFromFileDescriptor()
    if (apiKeyFromFd) {
      return {
        key: apiKeyFromFd,
        source: 'ACOSMI_API_KEY',
      }
    }

    if (
      !apiKeyEnv &&
      !process.env.CRABCODE_OAUTH_TOKEN &&
      !process.env.CRABCODE_OAUTH_TOKEN_FILE_DESCRIPTOR
    ) {
      throw new Error(
        'ACOSMI_API_KEY or CRABCODE_OAUTH_TOKEN env var is required',
      )
    }

    if (apiKeyEnv) {
      return {
        key: apiKeyEnv,
        source: 'ACOSMI_API_KEY',
      }
    }

    // OAuth token is present but this function returns API keys only
    return {
      key: null,
      source: 'none',
    }
  }
  // Check for ACOSMI_API_KEY before checking the apiKeyHelper or /login-managed key
  if (
    apiKeyEnv &&
    getGlobalConfig().customApiKeyResponses?.approved?.includes(
      normalizeApiKeyForConfig(apiKeyEnv),
    )
  ) {
    return {
      key: apiKeyEnv,
      source: 'ACOSMI_API_KEY',
    }
  }

  // Check for API key from file descriptor
  const apiKeyFromFd = getApiKeyFromFileDescriptor()
  if (apiKeyFromFd) {
    return {
      key: apiKeyFromFd,
      source: 'ACOSMI_API_KEY',
    }
  }

  // Check for apiKeyHelper — use sync cache, never block
  const apiKeyHelperCommand = getConfiguredApiKeyHelper()
  if (apiKeyHelperCommand) {
    if (opts.skipRetrievingKeyFromApiKeyHelper) {
      return {
        key: null,
        source: 'apiKeyHelper',
      }
    }
    // Cache may be cold (helper hasn't finished yet). Return null with
    // source='apiKeyHelper' rather than falling through to keychain —
    // apiKeyHelper must win. Callers needing a real key must await
    // getApiKeyFromApiKeyHelper() first (client.ts, useApiKeyVerification do).
    return {
      key: getApiKeyFromApiKeyHelperCached(),
      source: 'apiKeyHelper',
    }
  }

  const apiKeyFromConfigOrMacOSKeychain = getApiKeyFromConfigOrMacOSKeychain()
  if (apiKeyFromConfigOrMacOSKeychain) {
    return apiKeyFromConfigOrMacOSKeychain
  }

  return {
    key: null,
    source: 'none',
  }
}

/**
 * Get the configured apiKeyHelper from settings.
 * In bare mode, only the --settings flag source is consulted — apiKeyHelper
 * from ~/.crabcode/settings.json or project settings is ignored.
 */
export function getConfiguredApiKeyHelper(): string | undefined {
  if (isBareMode()) {
    return getSettingsForSource('flagSettings')?.apiKeyHelper
  }
  const mergedSettings = getSettings_DEPRECATED() || {}
  return mergedSettings.apiKeyHelper
}

/**
 * Check if the configured apiKeyHelper comes from project settings (projectSettings or localSettings)
 */
function isApiKeyHelperFromProjectOrLocalSettings(): boolean {
  const apiKeyHelper = getConfiguredApiKeyHelper()
  if (!apiKeyHelper) {
    return false
  }

  const projectSettings = getSettingsForSource('projectSettings')
  const localSettings = getSettingsForSource('localSettings')
  return (
    projectSettings?.apiKeyHelper === apiKeyHelper ||
    localSettings?.apiKeyHelper === apiKeyHelper
  )
}

/**
 * Calculate TTL in milliseconds for the API key helper cache
 * Uses CRABCODE_API_KEY_HELPER_TTL_MS env var if set and valid,
 * otherwise defaults to 5 minutes
 */
export function calculateApiKeyHelperTTL(): number {
  const envTtl = process.env.CRABCODE_API_KEY_HELPER_TTL_MS

  if (envTtl) {
    const parsed = parseInt(envTtl, 10)
    if (!Number.isNaN(parsed) && parsed >= 0) {
      return parsed
    }
    logForDebugging(
      `Found CRABCODE_API_KEY_HELPER_TTL_MS env var, but it was not a valid number. Got ${envTtl}`,
      { level: 'error' },
    )
  }

  return DEFAULT_API_KEY_HELPER_TTL
}

// Async API key helper with sync cache for non-blocking reads.
// Epoch bumps on clearApiKeyHelperCache() — orphaned executions check their
// captured epoch before touching module state so a settings-change or 401-retry
// mid-flight can't clobber the newer cache/inflight.
let _apiKeyHelperCache: { value: string; timestamp: number } | null = null
let _apiKeyHelperInflight: {
  promise: Promise<string | null>
  // Only set on cold launches (user is waiting); null for SWR background refreshes.
  startedAt: number | null
} | null = null
let _apiKeyHelperEpoch = 0

export function getApiKeyHelperElapsedMs(): number {
  const startedAt = _apiKeyHelperInflight?.startedAt
  return startedAt ? Date.now() - startedAt : 0
}

export async function getApiKeyFromApiKeyHelper(
  isNonInteractiveSession: boolean,
): Promise<string | null> {
  if (!getConfiguredApiKeyHelper()) return null
  const ttl = calculateApiKeyHelperTTL()
  if (_apiKeyHelperCache) {
    if (Date.now() - _apiKeyHelperCache.timestamp < ttl) {
      return _apiKeyHelperCache.value
    }
    // Stale — return stale value now, refresh in the background.
    // `??=` banned here by eslint no-nullish-assign-object-call (bun bug).
    if (!_apiKeyHelperInflight) {
      _apiKeyHelperInflight = {
        promise: _runAndCache(
          isNonInteractiveSession,
          false,
          _apiKeyHelperEpoch,
        ),
        startedAt: null,
      }
    }
    return _apiKeyHelperCache.value
  }
  // Cold cache — deduplicate concurrent calls
  if (_apiKeyHelperInflight) return _apiKeyHelperInflight.promise
  _apiKeyHelperInflight = {
    promise: _runAndCache(isNonInteractiveSession, true, _apiKeyHelperEpoch),
    startedAt: Date.now(),
  }
  return _apiKeyHelperInflight.promise
}

async function _runAndCache(
  isNonInteractiveSession: boolean,
  isCold: boolean,
  epoch: number,
): Promise<string | null> {
  try {
    const value = await _executeApiKeyHelper(isNonInteractiveSession)
    if (epoch !== _apiKeyHelperEpoch) return value
    if (value !== null) {
      _apiKeyHelperCache = { value, timestamp: Date.now() }
    }
    return value
  } catch (e) {
    if (epoch !== _apiKeyHelperEpoch) return ' '
    const detail = e instanceof Error ? e.message : String(e)
    console.error(chalk.red(`apiKeyHelper failed: ${detail}`))
    logForDebugging(`Error getting API key from apiKeyHelper: ${detail}`, {
      level: 'error',
    })
    // SWR path: a transient failure shouldn't replace a working key with
    // the ' ' sentinel — keep serving the stale value and bump timestamp
    // so we don't hammer-retry every call.
    if (!isCold && _apiKeyHelperCache && _apiKeyHelperCache.value !== ' ') {
      _apiKeyHelperCache = { ..._apiKeyHelperCache, timestamp: Date.now() }
      return _apiKeyHelperCache.value
    }
    // Cold cache or prior error — cache ' ' so callers don't fall back to OAuth
    _apiKeyHelperCache = { value: ' ', timestamp: Date.now() }
    return ' '
  } finally {
    if (epoch === _apiKeyHelperEpoch) {
      _apiKeyHelperInflight = null
    }
  }
}

async function _executeApiKeyHelper(
  isNonInteractiveSession: boolean,
): Promise<string | null> {
  const apiKeyHelper = getConfiguredApiKeyHelper()
  if (!apiKeyHelper) {
    return null
  }

  if (isApiKeyHelperFromProjectOrLocalSettings()) {
    const hasTrust = checkHasTrustDialogAccepted()
    if (!hasTrust && !isNonInteractiveSession) {
      const error = new Error(
        `Security: apiKeyHelper executed before workspace trust is confirmed. If you see this message, post in ${MACRO.FEEDBACK_CHANNEL}.`,
      )
      logAntError('apiKeyHelper invoked before trust check', error)
      logEvent('tengu_apiKeyHelper_missing_trust11', {})
      return null
    }
  }

  const result = await execa(apiKeyHelper, {
    shell: true,
    timeout: 10 * 60 * 1000,
    reject: false,
  })
  if (result.failed) {
    // reject:false — execa resolves on exit≠0/timeout, stderr is on result
    const why = result.timedOut ? 'timed out' : `exited ${result.exitCode}`
    const stderr = result.stderr?.trim()
    throw new Error(stderr ? `${why}: ${stderr}` : why)
  }
  const stdout = result.stdout?.trim()
  if (!stdout) {
    throw new Error('did not return a value')
  }
  return stdout
}

/**
 * Sync cache reader — returns the last fetched apiKeyHelper value without executing.
 * Returns stale values to match SWR semantics of the async reader.
 * Returns null only if the async fetch hasn't completed yet.
 */
export function getApiKeyFromApiKeyHelperCached(): string | null {
  return _apiKeyHelperCache?.value ?? null
}

export function clearApiKeyHelperCache(): void {
  _apiKeyHelperEpoch++
  _apiKeyHelperCache = null
  _apiKeyHelperInflight = null
}

export function prefetchApiKeyFromApiKeyHelperIfSafe(
  isNonInteractiveSession: boolean,
): void {
  // Skip if trust not yet accepted — the inner _executeApiKeyHelper check
  // would catch this too, but would fire a false-positive analytics event.
  if (
    isApiKeyHelperFromProjectOrLocalSettings() &&
    !checkHasTrustDialogAccepted()
  ) {
    return
  }
  void getApiKeyFromApiKeyHelper(isNonInteractiveSession)
}

/** @private Use {@link getAcosmiApiKey} or {@link getAcosmiApiKeyWithSource} */
export const getApiKeyFromConfigOrMacOSKeychain = memoize(
  (): { key: string; source: ApiKeySource } | null => {
    if (isBareMode()) return null
    // KNOWN_LIMITATION: 凭据读取未迁移到 SecureStorage — 需要 SecureStorage 跨平台完成，跟踪于全局审计 G-10A
    if (process.platform === 'darwin') {
      // keychainPrefetch.ts fires this read at main.tsx top-level in parallel
      // with module imports. If it completed, use that instead of spawning a
      // sync `security` subprocess here (~33ms).
      const prefetch = getLegacyApiKeyPrefetchResult()
      if (prefetch) {
        if (prefetch.stdout) {
          return { key: prefetch.stdout, source: '/login managed key' }
        }
        // Prefetch completed with no key — fall through to config, not keychain.
      } else {
        const storageServiceName = getMacOsKeychainStorageServiceName()
        try {
          const result = execSyncWithDefaults_DEPRECATED(
            `security find-generic-password -a $USER -w -s "${storageServiceName}"`,
          )
          if (result) {
            return { key: result, source: '/login managed key' }
          }
        } catch (e) {
          logError(e)
        }
      }
    }

    const config = getGlobalConfig()
    if (!config.primaryApiKey) {
      return null
    }

    return { key: config.primaryApiKey, source: '/login managed key' }
  },
)

function isValidApiKey(apiKey: string): boolean {
  // Only allow alphanumeric characters, dashes, and underscores
  return /^[a-zA-Z0-9-_]+$/.test(apiKey)
}

export async function saveApiKey(apiKey: string): Promise<void> {
  if (!isValidApiKey(apiKey)) {
    throw new Error(
      'Invalid API key format. API key must contain only alphanumeric characters, dashes, and underscores.',
    )
  }

  // Store as primary API key
  await maybeRemoveApiKeyFromMacOSKeychain()
  let savedToKeychain = false
  if (process.platform === 'darwin') {
    try {
      // KNOWN_LIMITATION: 凭据写入未迁移到 SecureStorage — 需要 SecureStorage 跨平台完成，跟踪于全局审计 G-10A
      const storageServiceName = getMacOsKeychainStorageServiceName()
      const username = getUsername()

      // Convert to hexadecimal to avoid any escaping issues
      const hexValue = Buffer.from(apiKey, 'utf-8').toString('hex')

      // Use security's interactive mode (-i) with -X (hexadecimal) option
      // This ensures credentials never appear in process command-line arguments
      // Process monitors only see "security -i", not the password
      const command = `add-generic-password -U -a "${username}" -s "${storageServiceName}" -X "${hexValue}"\n`
      if (command.length > SECURITY_STDIN_LINE_LIMIT) {
        throw new Error(
          `API key keychain write command (${command.length}B) exceeds stdin line limit`,
        )
      }

      const result = await execa('security', ['-i'], {
        input: command,
        reject: false,
      })
      if (result.exitCode !== 0 || result.failed) {
        throw new Error('Failed to save API key to macOS Keychain')
      }

      logEvent('tengu_api_key_saved_to_keychain', {})
      savedToKeychain = true
    } catch (e) {
      logError(e)
      logEvent('tengu_api_key_keychain_error', {
        error: errorMessage(
          e,
        ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })
      logEvent('tengu_api_key_saved_to_config', {})
    }
  } else {
    logEvent('tengu_api_key_saved_to_config', {})
  }

  const normalizedKey = normalizeApiKeyForConfig(apiKey)

  // Save config with all updates
  saveGlobalConfig(current => {
    const approved = current.customApiKeyResponses?.approved ?? []
    return {
      ...current,
      // Only save to config if keychain save failed or not on darwin
      primaryApiKey: savedToKeychain ? current.primaryApiKey : apiKey,
      customApiKeyResponses: {
        ...current.customApiKeyResponses,
        approved: approved.includes(normalizedKey)
          ? approved
          : [...approved, normalizedKey],
        rejected: current.customApiKeyResponses?.rejected ?? [],
      },
    }
  })

  // Clear memo cache
  getApiKeyFromConfigOrMacOSKeychain.cache.clear?.()
  clearLegacyApiKeyPrefetch()
}

export function isCustomApiKeyApproved(apiKey: string): boolean {
  const config = getGlobalConfig()
  const normalizedKey = normalizeApiKeyForConfig(apiKey)
  return (
    config.customApiKeyResponses?.approved?.includes(normalizedKey) ?? false
  )
}

export async function removeApiKey(): Promise<void> {
  await maybeRemoveApiKeyFromMacOSKeychain()

  // Also remove from config instead of returning early, for older clients
  // that set keys before we supported keychain.
  saveGlobalConfig(current => ({
    ...current,
    primaryApiKey: undefined,
  }))

  // Clear memo cache
  getApiKeyFromConfigOrMacOSKeychain.cache.clear?.()
  clearLegacyApiKeyPrefetch()
}

async function maybeRemoveApiKeyFromMacOSKeychain(): Promise<void> {
  try {
    await maybeRemoveApiKeyFromMacOSKeychainThrows()
  } catch (e) {
    logError(e)
  }
}

// Function to store OAuth tokens in secure storage.
//
// T6-P1-AUTH-CALLERS — async + mutateAsync. The previous sync read()+update()
// pair read the whole credential record, replaced the `acosmiOauth` key, and
// wrote it back; a concurrent writer (MCP token save, another /login) could
// interleave and clobber it (lost update — audit §4.2). mutateAsync runs the
// merge inside a process-level serialized critical section, so only the
// `acosmiOauth` key is touched and every other key (subscription, rateLimit,
// mcpOAuth, ...) is preserved exactly (RFC §2.2).
export async function saveOAuthTokensIfNeeded(
  tokens: OAuthTokens,
): Promise<SecureStorageWriteResult> {
  if (!shouldUseAcosmiAuth(tokens.scopes)) {
    logEvent('tengu_oauth_tokens_not_acosmi', {})
    return { success: true }
  }

  // Skip saving inference-only tokens (they come from env vars)
  if (!tokens.refreshToken || !tokens.expiresAt) {
    logEvent('tengu_oauth_tokens_inference_only', {})
    return { success: true }
  }

  const secureStorage = getSecureStorage()
  const storageBackend =
    secureStorage.name as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS

  try {
    const mutation = (current: SecureStorageData): SecureStorageData => {
      const existingOauth = current.acosmiOauth as OAuthTokens | undefined
      const next: SecureStorageData = {
        ...current,
        acosmiOauth: {
          accessToken: tokens.accessToken,
          refreshToken: tokens.refreshToken,
          expiresAt: tokens.expiresAt,
          scopes: tokens.scopes,
          clientId: tokens.clientId ?? existingOauth?.clientId,
          serverUrl: tokens.serverUrl ?? existingOauth?.serverUrl,
          // Profile fetch in refreshOAuthToken swallows errors and returns null
          // on transient failures (network, 5xx, rate limit). Don't clobber a
          // valid stored subscription with null — fall back to the existing
          // value.
          subscriptionType:
            tokens.subscriptionType ?? existingOauth?.subscriptionType ?? null,
          rateLimitTier:
            tokens.rateLimitTier ?? existingOauth?.rateLimitTier ?? null,
          // Authoritative membership flag. Like subscriptionType, the refresh
          // path passes through existing values rather than re-fetching, and
          // syncMembershipActive may fail-soft to null on transient errors —
          // so fall back to the stored value to keep paying members unlocked.
          membershipActive:
            tokens.membershipActive ?? existingOauth?.membershipActive ?? null,
          // Display-only plan metadata (see OAuthTokens docs) — same preserve
          // semantics: a write that doesn't carry them must not clobber.
          membershipPlanCode:
            tokens.membershipPlanCode ??
            existingOauth?.membershipPlanCode ??
            null,
          membershipPlanName:
            tokens.membershipPlanName ??
            existingOauth?.membershipPlanName ??
            null,
        },
      }
      return next
    }
    // The old bridge path copied the full Keychain record into plaintext and
    // leaked custom/plugin/MCP secrets. Keychain-success now scrubs only the
    // stale plaintext OAuth projection under the same cross-process credential
    // lock; a non-secret marker provides cross-process cache freshness instead.
    const updateStatus =
      process.platform === 'darwin'
        ? await (
            await import('./secureStorage/index.js')
          ).mutateSecureStorageWithPrimaryPlaintextCleanup(
            mutation,
            current => {
              const { acosmiOauth: _drop, ...rest } = current
              return rest
            },
          )
        : await secureStorage.mutateAsync(mutation)

    if (updateStatus.success || updateStatus.committed) {
      await markAuthStateChanged()
    }
    if (updateStatus.success) {
      logEvent('tengu_oauth_tokens_saved', { storageBackend })
    } else {
      logEvent('tengu_oauth_tokens_save_failed', { storageBackend })
    }

    getAcosmiOAuthTokens.cache?.clear?.()
    clearBetasCaches()
    clearToolSchemaCache()
    return updateStatus
  } catch (error) {
    logError(error)
    logEvent('tengu_oauth_tokens_save_exception', {
      storageBackend,
      error: errorMessage(
        error,
      ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    })
    return { success: false, warning: 'Failed to save OAuth tokens' }
  }
}

// Timestamp captured at module load — used to detect credentials.json updates
// that happened after this process started (e.g. user logged in via CrabClaw UI).
const _processStartMs = Date.now()
const AUTH_STATE_MARKER_FILE = '.auth-state-generation'

function authStateMarkerPath(): string {
  return join(getCrabCodeConfigHomeDir(), AUTH_STATE_MARKER_FILE)
}

/** Publish cross-process auth freshness without persisting any credential. */
export async function markAuthStateChanged(): Promise<void> {
  if (
    process.env.CRABCODE_TEST === '1' &&
    process.env.CRABCODE_TEST_ALLOW_AUTH_MARKER !== '1'
  ) {
    return
  }
  try {
    await mkdir(getCrabCodeConfigHomeDir(), { recursive: true })
    writeFileSyncAtomicNoFallback(
      authStateMarkerPath(),
      `${Date.now()}:${process.pid}\n`,
      { encoding: 'utf8', mode: 0o600 },
    )
  } catch {
    logForDebugging('[auth] failed to update auth state marker', {
      level: 'warn',
    })
  }
}

function latestAuthStateMtimeSync(): number {
  let latest = 0
  for (const path of [
    authStateMarkerPath(),
    join(getCrabCodeConfigHomeDir(), '.credentials.json'),
  ]) {
    try {
      latest = Math.max(latest, statSync(path).mtimeMs)
    } catch {
      // Absent compatibility surface.
    }
  }
  return latest
}

/**
 * Synchronous OAuth token getter.
 *
 * T6-P1-AUTH-CALLERS — deliberately kept on the sync `read()` (RFC §1.2 S2/S3
 * exemption): this is a memoized sync API consumed across hundreds of call
 * sites, and the keychain backend it reads is primed by startKeychainPrefetch()
 * at startup + a 30s TTL cache, so the read is a proven cache hit on the
 * common path. The runtime-standard async entry point is
 * `getAcosmiOAuthTokensAsync()`; agent-loop and 401 handling use that. This
 * sync sibling exists for sync call sites only.
 */
const _getAcosmiOAuthTokensMemoized = memoize((): OAuthTokens | null => {
  // --bare: API-key-only. No OAuth env tokens, no keychain, no credentials file.
  if (isBareMode()) return null

  // Check for force-set OAuth token from environment variable.
  // An environment token injected at process spawn may become
  // stale if the user logs in after the session was created.  When a fresher
  // credentials.json exists on disk we skip the env var and fall through to
  // secure storage so the TS side picks up the same token the upstream gateway uses.
  if (process.env.CRABCODE_OAUTH_TOKEN) {
    const credentialsNewer = latestAuthStateMtimeSync() > _processStartMs
    if (!credentialsNewer) {
      // Return an inference-only token (unknown refresh and expiry)
      return {
        accessToken: process.env.CRABCODE_OAUTH_TOKEN,
        refreshToken: null,
        expiresAt: null,
        scopes: ['ai'],
        subscriptionType: null,
        rateLimitTier: null,
        membershipActive: null,
      }
    }
    // The auth marker / legacy credentials file is fresher — fall through to
    // secure storage below.
  }

  // Check for OAuth token from file descriptor
  const oauthTokenFromFd = getOAuthTokenFromFileDescriptor()
  if (oauthTokenFromFd) {
    // Return an inference-only token (unknown refresh and expiry)
    return {
      accessToken: oauthTokenFromFd,
      refreshToken: null,
      expiresAt: null,
      scopes: ['ai'],
      subscriptionType: null,
      rateLimitTier: null,
      membershipActive: null,
    }
  }

  try {
    const secureStorage = getSecureStorage()
    const storageData = secureStorage.read()
    const oauthData = storageData?.acosmiOauth as OAuthTokens | undefined

    if (!oauthData?.accessToken) {
      return null
    }

    return oauthData
  } catch (error) {
    logError(error)
    return null
  }
})

/**
 * Public sync OAuth token getter. Wraps the lodash-memoized implementation but
 * NEVER persists a `null` result in the cache.
 *
 * RC-1 / S6 (audit 2026-06-29): a *transient* empty read at startup — e.g.
 * credentials written by a different login process after this one began,
 * or a keychain ACL not yet primed — used to be memoized for the whole process
 * lifetime. That latched `isAcosmiSubscriber()` to false, and an entire class
 * of subscriber-gated commands (`/usage`, `/upgrade`, `/voice`, …) silently
 * vanished until the user restarted. The async sibling self-heals via
 * `invalidateOAuthCacheIfDiskChanged()` (mtime watch), but this sync path —
 * the one command gating actually calls — had no such recovery.
 *
 * Fix: drop a just-computed `null` from the cache so the very next call
 * re-reads secure storage. A *non-null* token is still cached permanently
 * (hot path, hundreds of call sites). `.cache` is re-exposed so the 9 existing
 * `getAcosmiOAuthTokens.cache?.clear?.()` call sites keep working unchanged.
 */
export const getAcosmiOAuthTokens: (() => OAuthTokens | null) & {
  cache?: { clear?: () => void; delete?: (key: unknown) => boolean }
} = Object.assign(
  (): OAuthTokens | null => {
    const tokens = _getAcosmiOAuthTokensMemoized()
    if (tokens === null) {
      // Zero-arg lodash memoize keys on `undefined`; evict so we re-read next time.
      _getAcosmiOAuthTokensMemoized.cache?.delete?.(undefined)
    }
    return tokens
  },
  { cache: _getAcosmiOAuthTokensMemoized.cache },
)

/**
 * Clears all OAuth token caches. Call this on 401 errors to ensure
 * the next token read comes from secure storage, not stale in-memory caches.
 * This handles the case where the local expiration check disagrees with the
 * server (e.g., due to clock corrections after token was issued).
 */
export function clearOAuthTokenCache(): void {
  getAcosmiOAuthTokens.cache?.clear?.()
  clearKeychainCache()
}

let lastAuthStateMtimeMs = 0

// Cross-process staleness: another CC instance may write fresh tokens to
// disk (refresh or /login), but this process's memoize caches forever.
// Without this, terminal 1's /login fixes terminal 1; terminal 2's /login
// then revokes terminal 1 server-side, and terminal 1's memoize never
// re-reads — infinite /login regress (CC-1096, GH#24317).
async function invalidateOAuthCacheIfDiskChanged(): Promise<void> {
  let latestMtimeMs = 0
  for (const path of [
    authStateMarkerPath(),
    join(getCrabCodeConfigHomeDir(), '.credentials.json'),
  ]) {
    try {
      const { mtimeMs } = await stat(path)
      latestMtimeMs = Math.max(latestMtimeMs, mtimeMs)
    } catch {
      // Absent compatibility surface.
    }
  }
  if (latestMtimeMs > 0) {
    if (latestMtimeMs !== lastAuthStateMtimeMs) {
      lastAuthStateMtimeMs = latestMtimeMs
      clearOAuthTokenCache()
    }
  } else {
    // ENOENT — macOS keychain path (file deleted on migration). Clear only
    // the memoize so it delegates to the keychain cache's 30s TTL instead
    // of caching forever on top. `security find-generic-password` is
    // ~15ms; bounded to once per 30s by the keychain cache.
    getAcosmiOAuthTokens.cache?.clear?.()
  }
}

/**
 * OAuth token refresh result. Callers should branch on this to decide whether
 * to retry (temporary) or force re-login (permanent).
 *
 * - `'refreshed'`          — Token refresh succeeded; caller should retry with the new token.
 * - `'permanent_failure'`  — Token is permanently invalid (HTTP 400/401 from the
 *                            token endpoint = revoked/expired refresh token). Caller
 *                            should NOT retry and should force re-login.
 * - `'temporary_failure'`  — Transient error (network, lock contention, etc.). Caller
 *                            MAY retry after backoff.
 */
export type OAuthRefreshResult =
  | 'refreshed'
  | 'permanent_failure'
  | 'temporary_failure'

// In-flight dedup: when N acosmi.com proxy connectors hit 401 with the same
// token simultaneously (common at startup — #20930), only one should clear
// caches and re-read the keychain. Without this, each call's clearOAuthTokenCache()
// nukes readInFlight in macOsKeychainStorage and triggers a fresh spawn —
// sync spawns stacked to 800ms+ of blocked render frames.
const pending401Handlers = new Map<string, Promise<OAuthRefreshResult>>()

/**
 * Handle a 401 "OAuth token has expired" error from the API.
 *
 * This function forces a token refresh when the server says the token is expired,
 * even if our local expiration check disagrees (which can happen due to clock
 * issues when the token was issued).
 *
 * Safety: We compare the failed token with what's in keychain. If another tab
 * already refreshed (different token in keychain), we use that instead of
 * refreshing again. Concurrent calls with the same failedAccessToken are
 * deduplicated to a single keychain read.
 *
 * @param failedAccessToken - The access token that was rejected with 401
 * @returns `OAuthRefreshResult` — `'refreshed'` if we now have a valid token,
 *          `'permanent_failure'` if the refresh token is permanently invalid,
 *          `'temporary_failure'` for transient/network errors.
 */
export function handleOAuth401Error(
  failedAccessToken: string,
): Promise<OAuthRefreshResult> {
  const pending = pending401Handlers.get(failedAccessToken)
  if (pending) return pending

  const promise = handleOAuth401ErrorImpl(failedAccessToken).finally(() => {
    pending401Handlers.delete(failedAccessToken)
  })
  pending401Handlers.set(failedAccessToken, promise)
  return promise
}

async function handleOAuth401ErrorImpl(
  failedAccessToken: string,
): Promise<OAuthRefreshResult> {
  // Clear caches and re-read from keychain (async — sync read blocks ~100ms/call)
  clearOAuthTokenCache()
  const currentTokens = await getAcosmiOAuthTokensAsync()

  if (!currentTokens?.refreshToken) {
    return 'permanent_failure'
  }

  // If keychain has a different token, another tab already refreshed - use it
  if (currentTokens.accessToken !== failedAccessToken) {
    logEvent('tengu_oauth_401_recovered_from_keychain', {})
    return 'refreshed'
  }

  // Same token that failed - force refresh, bypassing local expiration check
  return checkAndRefreshOAuthTokenIfNeeded(0, true)
}

/**
 * Reads OAuth tokens asynchronously, avoiding blocking keychain reads.
 * Delegates to the sync memoized version for env var / file descriptor tokens
 * (which don't hit the keychain), and only uses async for storage reads.
 */
export async function getAcosmiOAuthTokensAsync(): Promise<OAuthTokens | null> {
  if (isBareMode()) return null

  // Env var and FD tokens are sync and don't hit the keychain
  if (
    process.env.CRABCODE_OAUTH_TOKEN ||
    getOAuthTokenFromFileDescriptor()
  ) {
    return getAcosmiOAuthTokens()
  }

  try {
    const secureStorage = getSecureStorage()
    const storageData = await secureStorage.readAsync()
    const oauthData = storageData?.acosmiOauth as OAuthTokens | undefined
    if (!oauthData?.accessToken) {
      return null
    }
    return oauthData
  } catch (error) {
    logError(error)
    return null
  }
}

// In-flight promise for deduplicating concurrent calls
let pendingRefreshCheck: Promise<OAuthRefreshResult> | null = null

export function checkAndRefreshOAuthTokenIfNeeded(
  retryCount = 0,
  force = false,
): Promise<OAuthRefreshResult> {
  // Deduplicate concurrent non-retry, non-force calls
  if (retryCount === 0 && !force) {
    if (pendingRefreshCheck) {
      return pendingRefreshCheck
    }

    const promise = checkAndRefreshOAuthTokenIfNeededImpl(retryCount, force)
    pendingRefreshCheck = promise.finally(() => {
      pendingRefreshCheck = null
    })
    return pendingRefreshCheck
  }

  return checkAndRefreshOAuthTokenIfNeededImpl(retryCount, force)
}

async function checkAndRefreshOAuthTokenIfNeededImpl(
  retryCount: number,
  force: boolean,
): Promise<OAuthRefreshResult> {
  const MAX_RETRIES = 5

  await invalidateOAuthCacheIfDiskChanged()

  // First check if token is expired with cached value
  // Skip this check if force=true (server already told us token is bad)
  const tokens = getAcosmiOAuthTokens()
  if (!force) {
    if (!tokens?.refreshToken || !isOAuthTokenExpired(tokens.expiresAt)) {
      // Token not expired — no refresh needed. This is not an error; callers
      // that only care about success/failure treat non-'refreshed' as "no new
      // token", which is correct here (token is still valid).
      return 'temporary_failure'
    }
  }

  if (!tokens?.refreshToken) {
    return 'permanent_failure'
  }

  if (!shouldUseAcosmiAuth(tokens.scopes)) {
    return 'permanent_failure'
  }

  // Re-read tokens async to check if they're still expired
  // Another process might have refreshed them
  getAcosmiOAuthTokens.cache?.clear?.()
  clearKeychainCache()
  const freshTokens = await getAcosmiOAuthTokensAsync()
  if (
    !freshTokens?.refreshToken ||
    !isOAuthTokenExpired(freshTokens.expiresAt)
  ) {
    // Another process refreshed — token is valid now. Not an error, but we
    // didn't refresh ourselves. Treat as temporary (callers already handle
    // this path: withRetry re-creates the client which reads the new token).
    return 'temporary_failure'
  }

  // Tokens are still expired, try to acquire lock and refresh
  const crabCodeDir = getCrabCodeConfigHomeDir()
  await mkdir(crabCodeDir, { recursive: true })

  let release
  try {
    logEvent('tengu_oauth_token_refresh_lock_acquiring', {})
    release = await lockfile.lock(crabCodeDir)
    logEvent('tengu_oauth_token_refresh_lock_acquired', {})
  } catch (err) {
    if ((err as { code?: string }).code === 'ELOCKED') {
      // Another process has the lock, let's retry if we haven't exceeded max retries
      if (retryCount < MAX_RETRIES) {
        logEvent('tengu_oauth_token_refresh_lock_retry', {
          retryCount: retryCount + 1,
        })
        // Wait a bit before retrying
        await sleep(1000 + Math.random() * 1000)
        return checkAndRefreshOAuthTokenIfNeededImpl(retryCount + 1, force)
      }
      logEvent('tengu_oauth_token_refresh_lock_retry_limit_reached', {
        maxRetries: MAX_RETRIES,
      })
      return 'temporary_failure'
    }
    logError(err)
    logEvent('tengu_oauth_token_refresh_lock_error', {
      error: errorMessage(
        err,
      ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    })
    return 'temporary_failure'
  }
  try {
    // Check one more time after acquiring lock
    getAcosmiOAuthTokens.cache?.clear?.()
    clearKeychainCache()
    const lockedTokens = await getAcosmiOAuthTokensAsync()
    if (
      !lockedTokens?.refreshToken ||
      !isOAuthTokenExpired(lockedTokens.expiresAt)
    ) {
      logEvent('tengu_oauth_token_refresh_race_resolved', {})
      // Another process won the race and refreshed; token is now valid.
      return 'refreshed'
    }

    logEvent('tengu_oauth_token_refresh_starting', {})
    const refreshedTokens = await refreshOAuthToken(lockedTokens.refreshToken, {
      // For Acosmi subscribers, omit scopes so the default
      // ACOSMI_OAUTH_SCOPES applies — this allows scope expansion
      // (e.g. adding user:file_upload) on refresh without re-login.
      scopes: shouldUseAcosmiAuth(lockedTokens.scopes)
        ? undefined
        : lockedTokens.scopes,
    })
    const storageResult = await saveOAuthTokensIfNeeded(refreshedTokens)
    if (!storageResult.success && !storageResult.committed) {
      throw new Error(
        storageResult.warning ?? 'Failed to persist refreshed OAuth tokens',
      )
    }

    // Clear the cache after refreshing token
    getAcosmiOAuthTokens.cache?.clear?.()
    clearKeychainCache()
    return 'refreshed'
  } catch (error) {
    logError(error)

    getAcosmiOAuthTokens.cache?.clear?.()
    clearKeychainCache()
    const currentTokens = await getAcosmiOAuthTokensAsync()
    if (currentTokens && !isOAuthTokenExpired(currentTokens.expiresAt)) {
      logEvent('tengu_oauth_token_refresh_race_recovered', {})
      return 'refreshed'
    }

    // Distinguish permanent auth failure (HTTP 400/401 = token revoked) from
    // temporary network errors. Permanent failures should not be retried.
    const httpStatus = (error as { response?: { status?: number } })?.response?.status
    if (httpStatus === 400 || httpStatus === 401) {
      logEvent('tengu_oauth_token_refresh_permanent_failure', {})
      return 'permanent_failure'
    }

    return 'temporary_failure'
  } finally {
    logEvent('tengu_oauth_token_refresh_lock_releasing', {})
    await release()
    logEvent('tengu_oauth_token_refresh_lock_released', {})
  }
}

export function isAcosmiSubscriber(): boolean {
  // Bridge auth cache takes highest priority — upstream gateway SDK auth state is authoritative.
  // Checked before isAcosmiAuthEnabled() because the bridge may know about
  // subscription status even when local OAuth env/config is not set up.
  if (_bridgeAuthCache?.isSubscriber) {
    return true
  }

  if (!isAcosmiAuthEnabled()) {
    return false
  }

  return shouldUseAcosmiAuth(getAcosmiOAuthTokens()?.scopes)
}

export const isAcosmiAuthorized = isAcosmiSubscriber

/**
 * Check if the current OAuth token has the user:profile scope.
 *
 * Real /login tokens always include this scope. Env-var and file-descriptor
 * tokens (service keys) hardcode scopes to ['user:inference'] only. Use this
 * to gate calls to profile-scoped endpoints so service key sessions don't
 * generate 403 storms against /api/oauth/profile, bootstrap, etc.
 */
export function hasProfileScope(): boolean {
  return (
    getAcosmiOAuthTokens()?.scopes?.includes(ACOSMI_PROFILE_SCOPE) ?? false
  )
}

export function is1PApiCustomer(): boolean {
  // 1P API customers are users who are NOT Acosmi subscribers (Max, Pro, Enterprise, Team)

  // Exclude Acosmi subscribers
  if (isAcosmiSubscriber()) {
    return false
  }

  // Everyone else is an API customer (OAuth API customers, direct API key users, etc.)
  return true
}

/**
 * Gets OAuth account information when Acosmi auth is enabled.
 * Returns undefined when using external API keys or third-party services.
 */
export function getOauthAccountInfo(): AccountInfo | undefined {
  return isAcosmiAuthEnabled() ? getGlobalConfig().oauthAccount : undefined
}

/**
 * Checks if overage/extra usage provisioning is allowed for this organization.
 * This mirrors the logic in apps/crabcode-ai `useIsOverageProvisioningAllowed` hook as closely as possible.
 */
export function isOverageProvisioningAllowed(): boolean {
  const accountInfo = getOauthAccountInfo()
  const billingType = accountInfo?.billingType

  // Must be a CrabCode subscriber with a supported subscription type
  if (!isAcosmiSubscriber() || !billingType) {
    return false
  }

  // only allow Stripe and mobile billing types to purchase extra usage
  if (
    billingType !== 'stripe_subscription' &&
    billingType !== 'stripe_subscription_contracted' &&
    billingType !== 'apple_subscription' &&
    billingType !== 'google_play_subscription'
  ) {
    return false
  }

  return true
}

export function getSubscriptionType(): SubscriptionType | null {
  if (!isAcosmiAuthEnabled()) {
    return null
  }
  const oauthTokens = getAcosmiOAuthTokens()
  if (!oauthTokens) {
    return null
  }

  return oauthTokens.subscriptionType ?? null
}

export function isMaxSubscriber(): boolean {
  return getSubscriptionType() === 'max'
}

export function isTeamSubscriber(): boolean {
  return getSubscriptionType() === 'team'
}

export function isTeamPremiumSubscriber(): boolean {
  return (
    getSubscriptionType() === 'team' &&
    getRateLimitTier() === 'default_crabcode_max_5x'
  )
}

export function isEnterpriseSubscriber(): boolean {
  return getSubscriptionType() === 'enterprise'
}

export function isProSubscriber(): boolean {
  return getSubscriptionType() === 'pro'
}

export function getRateLimitTier(): string | null {
  if (!isAcosmiAuthEnabled()) {
    return null
  }
  const oauthTokens = getAcosmiOAuthTokens()
  if (!oauthTokens) {
    return null
  }

  return oauthTokens.rateLimitTier ?? null
}

/**
 * Authoritative Acosmi membership flag (`getMembership().hasActive`), read
 * synchronously from the persisted OAuth tokens. `true` = active paid member,
 * `false` = free tier, `null` = unknown (signed out, inference-only token, or
 * membership not yet fetched). This is the single source the local/custom-model
 * entitlement gate reads — never `getSubscriptionType()` (legacy tier field).
 */
export function getMembershipActive(): boolean | null {
  if (!isAcosmiAuthEnabled()) {
    return null
  }
  const oauthTokens = getAcosmiOAuthTokens()
  if (!oauthTokens) {
    return null
  }

  return oauthTokens.membershipActive ?? null
}

/**
 * Persisted membership plan code (`getMembership().planCode`, e.g. "PRO_MAX",
 * yearly variants "..._YEARLY"), read synchronously from the stored OAuth
 * tokens. `null` = unknown (signed out, inference-only token, or persisted
 * before the plan metadata existed — `syncMembershipActive` self-heals on the
 * next successful fetch). Consumed by the tier-aware custom/local-model gate
 * (W-CUSTOM-MODEL-PLUS-GATE 2026-07-04) via `planTier.ts`.
 */
export function getMembershipPlanCode(): string | null {
  if (!isAcosmiAuthEnabled()) {
    return null
  }
  const planCode = getAcosmiOAuthTokens()?.membershipPlanCode
  return typeof planCode === 'string' && planCode.trim().length > 0
    ? planCode
    : null
}

/**
 * Snapshot of both membership gate signals in exactly the shape the
 * custom/local-model entitlement resolvers take — the one-liner every gate
 * call site passes (or spreads into `validateModelSelection` input).
 */
export function getMembershipGateInput(): {
  membershipActive: boolean | null
  membershipPlanCode: string | null
} {
  return {
    membershipActive: getMembershipActive(),
    membershipPlanCode: getMembershipPlanCode(),
  }
}

/**
 * Humanize an Acosmi membership plan code for display: split on `_`, then
 * Title-Case each segment (`PRO_MAX` → `Pro Max`, `ULTRA` → `Ultra`).
 * Deliberately NOT an enum lookup — unknown future plan codes must still
 * render without a hardcoded plan/tier table.
 */
export function humanizePlanCode(planCode: string): string {
  return planCode
    .split('_')
    .filter(segment => segment.length > 0)
    .map(segment => segment.charAt(0).toUpperCase() + segment.slice(1).toLowerCase())
    .join(' ')
}

export function getSubscriptionName(): string {
  const subscriptionType = getSubscriptionType()

  switch (subscriptionType) {
    case 'enterprise':
      return 'CrabCode Enterprise'
    case 'team':
      return 'CrabCode Team'
    case 'max':
      return 'CrabCode Max'
    case 'pro':
      return 'CrabCode Pro'
    default: {
      // Acosmi membership branch — legacy subscriptionType is null by design
      // for Acosmi OAuth, so derive the tier label from the persisted
      // membership plan. Display-only; gated on the authoritative
      // membershipActive === true (never render a plan for a non-member).
      if (
        (subscriptionType === null || subscriptionType === undefined) &&
        getMembershipActive() === true
      ) {
        const planCode = getAcosmiOAuthTokens()?.membershipPlanCode
        if (typeof planCode === 'string' && planCode.trim().length > 0) {
          return `CrabCode ${humanizePlanCode(planCode)}`
        }
      }
      return 'CrabCode API'
    }
  }
}

/** Check if using third-party services. Always returns false (vendor integrations removed). */
export function isUsing3PServices(): boolean {
  return false
}

/**
 * Get the configured otelHeadersHelper from settings
 */
function getConfiguredOtelHeadersHelper(): string | undefined {
  const mergedSettings = getSettings_DEPRECATED() || {}
  return mergedSettings.otelHeadersHelper
}

/**
 * Check if the configured otelHeadersHelper comes from project settings (projectSettings or localSettings)
 */
export function isOtelHeadersHelperFromProjectOrLocalSettings(): boolean {
  const otelHeadersHelper = getConfiguredOtelHeadersHelper()
  if (!otelHeadersHelper) {
    return false
  }

  const projectSettings = getSettingsForSource('projectSettings')
  const localSettings = getSettingsForSource('localSettings')
  return (
    projectSettings?.otelHeadersHelper === otelHeadersHelper ||
    localSettings?.otelHeadersHelper === otelHeadersHelper
  )
}

// Cache for debouncing otelHeadersHelper calls
let cachedOtelHeaders: Record<string, string> | null = null
let cachedOtelHeadersTimestamp = 0
const DEFAULT_OTEL_HEADERS_DEBOUNCE_MS = 29 * 60 * 1000 // 29 minutes

export function getOtelHeadersFromHelper(): Record<string, string> {
  const otelHeadersHelper = getConfiguredOtelHeadersHelper()

  if (!otelHeadersHelper) {
    return {}
  }

  // Return cached headers if still valid (debounce)
  const debounceMs = parseInt(
    process.env.CRABCODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS ||
      DEFAULT_OTEL_HEADERS_DEBOUNCE_MS.toString(),
  )
  if (
    cachedOtelHeaders &&
    Date.now() - cachedOtelHeadersTimestamp < debounceMs
  ) {
    return cachedOtelHeaders
  }

  if (isOtelHeadersHelperFromProjectOrLocalSettings()) {
    // Check if trust has been established for this project
    const hasTrust = checkHasTrustDialogAccepted()
    if (!hasTrust) {
      return {}
    }
  }

  try {
    const result = execSyncWithDefaults_DEPRECATED(otelHeadersHelper, {
      timeout: 30000, // 30 seconds - allows for auth service latency
    })
      ?.toString()
      .trim()
    if (!result) {
      throw new Error('otelHeadersHelper did not return a valid value')
    }

    const headers = jsonParse(result)
    if (
      typeof headers !== 'object' ||
      headers === null ||
      Array.isArray(headers)
    ) {
      throw new Error(
        'otelHeadersHelper must return a JSON object with string key-value pairs',
      )
    }

    // Validate all values are strings
    for (const [key, value] of Object.entries(headers)) {
      if (typeof value !== 'string') {
        throw new Error(
          `otelHeadersHelper returned non-string value for key "${key}": ${typeof value}`,
        )
      }
    }

    // Cache the result
    cachedOtelHeaders = headers as Record<string, string>
    cachedOtelHeadersTimestamp = Date.now()

    return cachedOtelHeaders
  } catch (error) {
    logError(
      new Error(
        `Error getting OpenTelemetry headers from otelHeadersHelper (in settings): ${errorMessage(error)}`,
      ),
    )
    throw error
  }
}

function isConsumerPlan(plan: SubscriptionType): plan is 'max' | 'pro' {
  return plan === 'max' || plan === 'pro'
}

export function isConsumerSubscriber(): boolean {
  const subscriptionType = getSubscriptionType()
  return (
    isAcosmiSubscriber() &&
    subscriptionType !== null &&
    isConsumerPlan(subscriptionType)
  )
}

export type UserAccountInfo = {
  subscription?: string
  tokenSource?: string
  apiKeySource?: ApiKeySource
  organization?: string
  email?: string
}

export function getAccountInformation() {
  // Only provide account info for official providers (firstParty + acosmi)
  if (!isOfficialProvider()) {
    return undefined
  }
  const { source: authTokenSource } = getAuthTokenSource()
  const accountInfo: UserAccountInfo = {}
  if (
    authTokenSource === 'CRABCODE_OAUTH_TOKEN' ||
    authTokenSource === 'CRABCODE_OAUTH_TOKEN_FILE_DESCRIPTOR'
  ) {
    accountInfo.tokenSource = authTokenSource
  } else if (isAcosmiSubscriber()) {
    accountInfo.subscription = getSubscriptionName()
  } else {
    accountInfo.tokenSource = authTokenSource
  }
  const { key: apiKey, source: apiKeySource } = getAcosmiApiKeyWithSource()
  if (apiKey) {
    accountInfo.apiKeySource = apiKeySource
  }

  // We don't know the organization if we're relying on an external API key or auth token
  if (
    authTokenSource === 'acosmi.com' ||
    apiKeySource === '/login managed key'
  ) {
    // Get organization name from OAuth account info
    const orgName = getOauthAccountInfo()?.organizationName
    if (orgName) {
      accountInfo.organization = orgName
    }
  }
  const email = getOauthAccountInfo()?.emailAddress
  if (
    (authTokenSource === 'acosmi.com' ||
      apiKeySource === '/login managed key') &&
    email
  ) {
    accountInfo.email = email
  }
  return accountInfo
}

/**
 * Result of org validation — either success or a descriptive error.
 */
export type OrgValidationResult =
  | { valid: true }
  | { valid: false; message: string }

/**
 * Validate that the active OAuth token belongs to the organization required
 * by `forceLoginOrgUUID` in managed settings. Returns a result object
 * rather than throwing so callers can choose how to surface the error.
 *
 * Fails closed: if `forceLoginOrgUUID` is set and we cannot determine the
 * token's org (network error, missing profile data), validation fails.
 */
export async function validateForceLoginOrg(): Promise<OrgValidationResult> {
  // `crabcode ssh` remote: real auth lives on the local machine and is injected
  // by the proxy. The placeholder token can't be validated against the profile
  // endpoint. The local side already ran this check before establishing the session.
  if (process.env.ACOSMI_UNIX_SOCKET) {
    return { valid: true }
  }

  if (!isAcosmiAuthEnabled()) {
    return { valid: true }
  }

  const requiredOrgUuid =
    getSettingsForSource('policySettings')?.forceLoginOrgUUID
  if (!requiredOrgUuid) {
    return { valid: true }
  }

  // Ensure the access token is fresh before hitting the profile endpoint.
  // No-op for env-var tokens (refreshToken is null).
  await checkAndRefreshOAuthTokenIfNeeded()

  const tokens = getAcosmiOAuthTokens()
  if (!tokens) {
    return { valid: true }
  }

  // Always fetch the authoritative org UUID from the profile endpoint.
  // Even keychain-sourced tokens verify server-side: the cached org UUID
  // in ~/.crabcode.json is user-writable and cannot be trusted.
  const { source } = getAuthTokenSource()
  const isEnvVarToken =
    source === 'CRABCODE_OAUTH_TOKEN' ||
    source === 'CRABCODE_OAUTH_TOKEN_FILE_DESCRIPTOR'

  const profile = await getOauthProfileFromOauthToken(tokens.accessToken)
  if (!profile) {
    // Fail closed — we can't verify the org
    return {
      valid: false,
      message:
        `Unable to verify organization for the current authentication token.\n` +
        `This machine requires organization ${requiredOrgUuid} but the profile could not be fetched.\n` +
        `This may be a network error, or the token may lack the user:profile scope required for\n` +
        `verification (tokens from 'crabcode setup-token' do not include this scope).\n` +
        `Try again, or obtain a full-scope token via 'crabcode auth login'.`,
    }
  }

  const tokenOrgUuid = profile.organization.uuid
  if (tokenOrgUuid === requiredOrgUuid) {
    return { valid: true }
  }

  if (isEnvVarToken) {
    const envVarName =
      source === 'CRABCODE_OAUTH_TOKEN'
        ? 'CRABCODE_OAUTH_TOKEN'
        : 'CRABCODE_OAUTH_TOKEN_FILE_DESCRIPTOR'
    return {
      valid: false,
      message:
        `The ${envVarName} environment variable provides a token for a\n` +
        `different organization than required by this machine's managed settings.\n\n` +
        `Required organization: ${requiredOrgUuid}\n` +
        `Token organization:   ${tokenOrgUuid}\n\n` +
        `Remove the environment variable or obtain a token for the correct organization.`,
    }
  }

  return {
    valid: false,
    message:
      `Your authentication token belongs to organization ${tokenOrgUuid},\n` +
      `but this machine requires organization ${requiredOrgUuid}.\n\n` +
      `Please log in with the correct organization: crabcode auth login`,
  }
}
