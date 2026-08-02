import memoize from 'lodash-es/memoize.js'
import { homedir } from 'os'
import { join } from 'path'
import { resolveCrabCodeConfigHomeDir } from './configFilePath.js'

// Memoized: 150+ callers, many on hot paths. Keyed off BOTH CRABCODE_CONFIG_DIR
// and CRABCODE_HOME so tests/releases that change either env get a fresh value
// without explicit cache.clear.
//
// Resolution (single canonical isolation variable, aligned with the Rust side
// acosmi-config paths::resolve_home_dir):
//   1. CRABCODE_CONFIG_DIR — explicit FULL config-dir override, highest priority
//      (existing tests rely on this exact precedence).
//   2. CRABCODE_HOME — the home BASE; config/sessions/oauth/keychain live under
//      <CRABCODE_HOME>/.crabcode. Previously this var was ignored here while
//      socket/state code honored it → split-brain: an isolation run that set
//      only CRABCODE_HOME leaked the main config dir into the real ~/.crabcode.
//   3. os homedir — default <home>/.crabcode.
export const getCrabCodeConfigHomeDir = memoize(
  resolveCrabCodeConfigHomeDir,
  () =>
    `${process.env.CRABCODE_CONFIG_DIR ?? ''}\u0000${
      process.env.CRABCODE_HOME ?? ''
    }`,
)

/**
 * Daemon 运行期文件（cron / app-server 的 socket·pid·lock·log、outbox cursor、
 * pending notification、local-models 等）的 home 解析。
 *
 * 与 `getCrabCodeConfigHomeDir` 使用同一隔离语义：`CRABCODE_CONFIG_DIR`
 * 是完整覆盖，`CRABCODE_HOME` 是 home base。cron / app-server 的 socket
 * 路径是 TS client ↔ Rust daemon 的对称契约，两端必须完全一致。
 *
 * 解析优先级（与 Rust daemon-launcher + memory.rs 对齐）：
 *   1. `CRABCODE_CONFIG_DIR` — 显式全量覆盖，最高优先；直接当 state root
 *      （已是 `.crabcode` 等价目录）。**此前 cron/outbox/pending 三处只读
 *      `CRABCODE_HOME` 漏了它** → 仅设 `CRABCODE_CONFIG_DIR` 的隔离运行中
 *      cron.sock / outbox / pending 仍落真实 `~/.crabcode`，测试进程连上
 *      **生产** cron daemon、改写生产 cursor（§4 隔离泄漏）。
 *   2. `CRABCODE_HOME` — home base，state root 为 `<base>/.crabcode`。
 *   3. `homedir()/.crabcode` — 默认。
 *
 * 不做 NFC normalize（与 Rust `paths.rs::home_dir` 一致——socket 路径需双端
 * 字节级相同，normalize 会让含非 ASCII 路径的两端分叉）。
 */
export function getCrabCodeRuntimeHomeDir(): string {
  const configDir = process.env.CRABCODE_CONFIG_DIR
  if (configDir && configDir.length > 0) {
    return configDir
  }
  const home = process.env.CRABCODE_HOME
  if (home && home.length > 0) {
    return join(home, '.crabcode')
  }
  return join(homedir(), '.crabcode')
}

export function getTeamsDir(): string {
  return join(getCrabCodeConfigHomeDir(), 'teams')
}

/**
 * Check if NODE_OPTIONS contains a specific flag.
 * Splits on whitespace and checks for exact match to avoid false positives.
 */
export function hasNodeOption(flag: string): boolean {
  const nodeOptions = process.env.NODE_OPTIONS
  if (!nodeOptions) {
    return false
  }
  return nodeOptions.split(/\s+/).includes(flag)
}

export function isEnvTruthy(envVar: string | boolean | undefined): boolean {
  if (!envVar) return false
  if (typeof envVar === 'boolean') return envVar
  const normalizedValue = envVar.toLowerCase().trim()
  return ['1', 'true', 'yes', 'on'].includes(normalizedValue)
}

export function isEnvDefinedFalsy(
  envVar: string | boolean | undefined,
): boolean {
  if (envVar === undefined) return false
  if (typeof envVar === 'boolean') return !envVar
  if (!envVar) return false
  const normalizedValue = envVar.toLowerCase().trim()
  return ['0', 'false', 'no', 'off'].includes(normalizedValue)
}

/**
 * --bare / CRABCODE_SIMPLE — skip hooks, LSP, plugin sync, skill dir-walk,
 * attribution, background prefetches, and ALL keychain/credential reads.
 * Auth is strictly ACOSMI_API_KEY env or apiKeyHelper from --settings.
 * Explicit CLI flags (--plugin-dir, --add-dir, --mcp-config) still honored.
 * ~30 gates across the codebase.
 *
 * Checks argv directly (in addition to the env var) because several gates
 * run before main.tsx's action handler sets CRABCODE_SIMPLE=1 from --bare
 * — notably startKeychainPrefetch() at main.tsx top-level.
 */
export function isBareMode(): boolean {
  return (
    isEnvTruthy(process.env.CRABCODE_SIMPLE) ||
    process.argv.includes('--bare')
  )
}

/**
 * Parses an array of environment variable strings into a key-value object
 * @param envVars Array of strings in KEY=VALUE format
 * @returns Object with key-value pairs
 */
export function parseEnvVars(
  rawEnvArgs: string[] | undefined,
): Record<string, string> {
  const parsedEnv: Record<string, string> = {}

  // Parse individual env vars
  if (rawEnvArgs) {
    for (const envStr of rawEnvArgs) {
      const [key, ...valueParts] = envStr.split('=')
      if (!key || valueParts.length === 0) {
        throw new Error(
          `Invalid environment variable format: ${envStr}, environment variables should be added as: -e KEY1=value1 -e KEY2=value2`,
        )
      }
      parsedEnv[key] = valueParts.join('=')
    }
  }
  return parsedEnv
}

/**
 * Get the AWS region with fallback to default
 * Matches the Acosmi Bedrock SDK's region behavior
 */
export function getAWSRegion(): string {
  return process.env.AWS_REGION || process.env.AWS_DEFAULT_REGION || 'us-east-1'
}

/**
 * Get the default Vertex AI region
 */
export function getDefaultVertexRegion(): string {
  return process.env.CLOUD_ML_REGION || 'us-east5'
}

/**
 * Check if bash commands should maintain project working directory (reset to original after each command)
 * @returns true if CRABCODE_BASH_MAINTAIN_PROJECT_WORKING_DIR is set to a truthy value
 */
export function shouldMaintainProjectWorkingDir(): boolean {
  return isEnvTruthy(process.env.CRABCODE_BASH_MAINTAIN_PROJECT_WORKING_DIR)
}

/**
 * Check if running on Homespace (ant-internal cloud environment)
 */
export function isRunningOnHomespace(): boolean {
  return (
    process.env.USER_TYPE === 'ant' &&
    isEnvTruthy(process.env.COO_RUNNING_ON_HOMESPACE)
  )
}

/**
 * Conservative check for whether CrabCode is running inside a protected
 * (privileged or ASL3+) COO namespace or cluster.
 *
 * Conservative means: when signals are ambiguous, assume protected. We would
 * rather over-report protected usage than miss it. Unprotected environments
 * are homespace, namespaces on the open allowlist, and no k8s/COO signals
 * at all (laptop/local dev).
 *
 * Used for telemetry to measure auto-mode usage in sensitive environments.
 */
export function isInProtectedNamespace(): boolean {
  // USER_TYPE is build-time --define'd; in external builds this block is
  // DCE'd so the require() and namespace allowlist never appear in the bundle.
  if (process.env.USER_TYPE === 'ant') {
    /* eslint-disable @typescript-eslint/no-require-imports */
    return (
      require('./protectedNamespace.js') as typeof import('./protectedNamespace.js')
    ).checkProtectedNamespace()
    /* eslint-enable @typescript-eslint/no-require-imports */
  }
  return false
}

/**
 * Get the Vertex AI region for a specific model.
 *
 * Per-model region overrides are routed through env vars named
 * `VERTEX_REGION_${SDK_MODEL_ID}` (uppercased, with non-alphanumeric chars
 * normalised to `_`). Falls back to CLOUD_ML_REGION / 'us-east5' default
 * when the per-model var is unset. CrabCode's primary path is the acosmi
 * provider — Vertex routing is a 3P passthrough, so SDK model ids drive
 * the var lookup directly without any hardcoded prefix mapping.
 */
export function getVertexRegionForModel(
  model: string | undefined,
): string | undefined {
  if (model) {
    const normalised = model
      .toUpperCase()
      .replace(/[^A-Z0-9]+/g, '_')
      .replace(/^_+|_+$/g, '')
    if (normalised) {
      const override = process.env[`VERTEX_REGION_${normalised}`]
      if (override) return override
    }
  }
  return getDefaultVertexRegion()
}
