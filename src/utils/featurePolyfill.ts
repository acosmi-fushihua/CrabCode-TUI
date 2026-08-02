/**
 * bun:bundle feature() polyfill
 *
 * In the Bun build pipeline, `feature('FLAG')` is a compile-time constant
 * that the bundler replaces with `true` or `false` based on the build
 * manifest. The bundler then performs dead code elimination (DCE) on the
 * resulting constant branches, stripping unreachable code paths and their
 * transitive imports from the output.
 *
 * In non-Bun environments (Node.js, Deno, tsx, vitest, etc.), this polyfill
 * provides runtime-equivalent behavior. Because there is no bundler to
 * perform DCE, conditional `require()` calls guarded by `feature()` will
 * still load the modules — they just won't execute the guarded code paths
 * when the flag is disabled.
 *
 * Configuration source priority:
 *   1. Environment variable: CRABCODE_FEATURE_<FLAG_NAME>=1|0|true|false
 *   2. Config file: ~/.crabcode/features.json  (JSON object { "FLAG": true })
 *   3. Default values from featureFlags.ts (all false)
 *
 * Design constraints:
 *   - Pure function: no side effects, deterministic for a given process lifetime
 *   - Tree-shakeable: only imports from featureFlags.ts (a static data table)
 *   - Config file is read lazily on first call, then cached for process lifetime
 *   - Unknown flag names return false (safe default, matches Bun external build)
 */

import {
  FEATURE_FLAG_DEFAULTS,
  type FeatureFlagName,
} from './featureFlags.js'

// ---------------------------------------------------------------------------
// Config file cache (lazy singleton)
// ---------------------------------------------------------------------------

let _configFileCache: Record<string, boolean> | null = null
let _configFileLoaded = false

/**
 * Attempt to read ~/.crabcode/features.json synchronously.
 *
 * Returns a plain object of flag overrides, or an empty object on any error.
 * The file is read exactly once per process lifetime. This uses synchronous
 * I/O because feature() must be a synchronous function (it's called in
 * module-level const initializers and ternary expressions).
 */
function loadConfigFile(): Record<string, boolean> {
  if (_configFileLoaded) return _configFileCache ?? {}
  _configFileLoaded = true

  try {
    // Dynamic require to avoid bundler issues; this path is only hit in
    // non-Bun runtimes where require('fs') is always available.
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const fs = require('fs') as typeof import('fs')
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const os = require('os') as typeof import('os')
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const path = require('path') as typeof import('path')

    const configPath = path.join(os.homedir(), '.crabcode', 'features.json')
    const raw = fs.readFileSync(configPath, 'utf8')
    const parsed: unknown = JSON.parse(raw)

    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
      const result: Record<string, boolean> = {}
      for (const [key, value] of Object.entries(parsed as Record<string, unknown>)) {
        if (typeof value === 'boolean') {
          result[key] = value
        } else if (value === 1 || value === '1' || value === 'true') {
          result[key] = true
        } else if (value === 0 || value === '0' || value === 'false') {
          result[key] = false
        }
        // Skip unrecognized value types silently
      }
      _configFileCache = result
      return result
    }
  } catch {
    // File not found, parse error, or permission error — all expected.
    // Fall through to empty config.
  }

  _configFileCache = {}
  return {}
}

// ---------------------------------------------------------------------------
// Environment variable lookup cache
// ---------------------------------------------------------------------------

/**
 * Cache for environment variable lookups. Populated lazily per flag.
 * A value of `undefined` means "not checked yet"; `null` means "env var
 * not set". This avoids repeated process.env access.
 */
const _envCache = new Map<string, boolean | null>()

/**
 * Check environment variable CRABCODE_FEATURE_<FLAG_NAME>.
 * Returns true/false if set, or null if not present.
 */
function checkEnvVar(flag: string): boolean | null {
  const cached = _envCache.get(flag)
  if (cached !== undefined) return cached

  const envKey = `CRABCODE_FEATURE_${flag}`
  const envValue = process.env[envKey]

  let result: boolean | null = null
  if (envValue !== undefined && envValue !== '') {
    const lower = envValue.toLowerCase()
    if (lower === '1' || lower === 'true' || lower === 'yes' || lower === 'on') {
      result = true
    } else if (
      lower === '0' ||
      lower === 'false' ||
      lower === 'no' ||
      lower === 'off'
    ) {
      result = false
    }
    // Unrecognized values are treated as "not set" (null)
  }

  _envCache.set(flag, result)
  return result
}

// ---------------------------------------------------------------------------
// Resolved flag cache
// ---------------------------------------------------------------------------

/**
 * Final resolved value cache. Once a flag is resolved through the priority
 * chain (env -> config file -> defaults), the result is cached here for
 * the process lifetime. This makes repeated feature() calls O(1).
 */
const _resolvedCache = new Map<string, boolean>()

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Runtime polyfill for `bun:bundle`'s `feature()` function.
 *
 * Returns a boolean indicating whether the given feature flag is enabled.
 * The result is deterministic for the lifetime of the process.
 *
 * Resolution priority:
 *   1. CRABCODE_FEATURE_<FLAG_NAME> environment variable
 *   2. ~/.crabcode/features.json config file
 *   3. Default value from FEATURE_FLAG_DEFAULTS (all false)
 *
 * Unknown flags always return false.
 *
 * @example
 * ```typescript
 * // Drop-in replacement for: import { feature } from 'bun:bundle'
 * import { feature } from '../utils/featurePolyfill.js'
 *
 * // Same usage patterns as bun:bundle feature():
 * const optionalTool = feature('WEB_BROWSER_TOOL') ? require('./browser.js') : null
 * if (feature('KAIROS')) { ... }
 * feature('PROACTIVE') || feature('KAIROS')
 * ```
 */
export function feature(flag: FeatureFlagName): boolean
export function feature(flag: string): boolean
export function feature(flag: string): boolean {
  // Fast path: already resolved
  const cached = _resolvedCache.get(flag)
  if (cached !== undefined) return cached

  // Priority 1: Environment variable
  const envValue = checkEnvVar(flag)
  if (envValue !== null) {
    _resolvedCache.set(flag, envValue)
    return envValue
  }

  // Priority 2: Config file
  const configFile = loadConfigFile()
  if (flag in configFile) {
    const cfgValue = configFile[flag]!
    _resolvedCache.set(flag, cfgValue)
    return cfgValue
  }

  // Priority 3: Default value
  const defaultValue =
    (flag as FeatureFlagName) in FEATURE_FLAG_DEFAULTS
      ? FEATURE_FLAG_DEFAULTS[flag as FeatureFlagName]
      : false

  _resolvedCache.set(flag, defaultValue)
  return defaultValue
}

// ---------------------------------------------------------------------------
// Test utilities
// ---------------------------------------------------------------------------

/**
 * Reset all caches. For testing only — allows tests to manipulate env vars
 * or config files and have feature() re-evaluate.
 */
export function _resetFeatureCacheForTests(): void {
  _configFileCache = null
  _configFileLoaded = false
  _envCache.clear()
  _resolvedCache.clear()
}

/**
 * Override a single flag value for the current process. For testing only.
 * Bypasses env/config lookup and sets the resolved value directly.
 */
export function _setFeatureOverrideForTests(
  flag: string,
  value: boolean,
): void {
  _resolvedCache.set(flag, value)
}
