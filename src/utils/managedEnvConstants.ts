/**
 * Trust phases used while projecting settings-sourced environment variables
 * into the process-wide environment.
 */
export type ManagedEnvPhase = 'pre-trust' | 'post-trust'

/**
 * Sources that can contribute settings to the managed environment projection.
 * `plugin` is included as an explicit deny-only source so a future expansion of
 * plugin settings cannot silently become process-wide env.
 */
export type ManagedEnvSource =
  | 'globalConfig'
  | 'userSettings'
  | 'projectSettings'
  | 'localSettings'
  | 'flagSettings'
  | 'policySettings'
  | 'plugin'

export type ManagedEnvVarClass =
  'pre-trust-safe' | 'post-trust-allowed' | 'sensitive' | 'unknown'

/**
 * Values that a project/local settings file may contribute before workspace
 * trust. Keep this list deliberately small: display/privacy-only controls that
 * cannot select credentials, network destinations, providers or executables.
 */
export const PRE_TRUST_SAFE_ENV_VARS: ReadonlySet<string> = new Set([
  'CRABCODE_DISABLE_NONESSENTIAL_TRAFFIC',
  'CRABCODE_DISABLE_TERMINAL_TITLE',
  'DISABLE_BUG_COMMAND',
  'DISABLE_COST_WARNINGS',
  'DISABLE_ERROR_REPORTING',
  'DISABLE_FEEDBACK_COMMAND',
  'DISABLE_TELEMETRY',
])

/**
 * Known ordinary values that project/local settings may contribute only after
 * workspace trust. Unknown non-sensitive values follow the same post-trust
 * rule, but remain `unknown` so security UIs review them by default.
 *
 * This set is intentionally disjoint from PRE_TRUST_SAFE_ENV_VARS and
 * SENSITIVE_ENV_VARS. Code that needs the union must classify the key instead
 * of merging the sets and losing phase semantics.
 */
export const POST_TRUST_ALLOWED_ENV_VARS: ReadonlySet<string> = new Set([
  'BASH_DEFAULT_TIMEOUT_MS',
  'BASH_MAX_OUTPUT_LENGTH',
  'BASH_MAX_TIMEOUT_MS',
  'CRABCODE_BASH_MAINTAIN_PROJECT_WORKING_DIR',
  'CRABCODE_DISABLE_EXPERIMENTAL_BETAS',
  'CRABCODE_EXPERIMENTAL_AGENT_TEAMS',
  'CRABCODE_IDE_SKIP_AUTO_INSTALL',
  'CRABCODE_MAX_OUTPUT_TOKENS',
  'DISABLE_AUTOUPDATER',
  'ENABLE_TOOL_SEARCH',
  'MAX_MCP_OUTPUT_TOKENS',
  'MAX_THINKING_TOKENS',
  'MCP_TIMEOUT',
  'MCP_TOOL_TIMEOUT',
])

/**
 * Provider/routing variables used by CRABCODE_PROVIDER_MANAGED_BY_HOST.
 * The literal names live once here and are reused by both the provider-owner
 * filter and the broader sensitive classifier.
 *
 * @[MODEL LAUNCH]: VERTEX_REGION_* is prefix-matched. New providers or new
 * endpoint/project/auth/model routing variables belong in this list.
 */
const PROVIDER_MANAGED_ENV_VAR_NAMES = [
  // The flag itself — settings cannot unset it once the host set it.
  'CRABCODE_PROVIDER_MANAGED_BY_HOST',
  // Provider selection.
  'CRABCODE_MODEL_PROVIDER',
  'CRABCODE_USE_BEDROCK',
  'CRABCODE_USE_VERTEX',
  'CRABCODE_USE_FOUNDRY',
  // Endpoint config (base URLs, project/resource identifiers).
  'ACOSMI_BASE_URL',
  'ACOSMI_BEDROCK_BASE_URL',
  'ACOSMI_VERTEX_BASE_URL',
  'ACOSMI_FOUNDRY_BASE_URL',
  'ACOSMI_FOUNDRY_RESOURCE',
  'ACOSMI_VERTEX_PROJECT_ID',
  'CRABCODE_CUSTOM_BASE_URL',
  // Region routing.
  'CLOUD_ML_REGION',
  // Auth.
  'ACOSMI_API_KEY',
  'ACOSMI_AUTH_TOKEN',
  'CRABCODE_OAUTH_TOKEN',
  'AWS_BEARER_TOKEN_BEDROCK',
  'ACOSMI_FOUNDRY_API_KEY',
  'CRABCODE_CUSTOM_API_KEY',
  'CRABCODE_SKIP_BEDROCK_AUTH',
  'CRABCODE_SKIP_VERTEX_AUTH',
  'CRABCODE_SKIP_FOUNDRY_AUTH',
  // Model defaults — often set to provider-specific ID formats.
  'ACOSMI_MODEL',
  'ACOSMI_CUSTOM_MODEL_OPTION',
  'ACOSMI_CUSTOM_MODEL_OPTION_DESCRIPTION',
  'ACOSMI_CUSTOM_MODEL_OPTION_NAME',
  'ACOSMI_DEFAULT_FAST_MODE_MODEL',
  'ACOSMI_DEFAULT_FAST_MODE_MODEL_DESCRIPTION',
  'ACOSMI_DEFAULT_FAST_MODE_MODEL_NAME',
  'ACOSMI_DEFAULT_FAST_MODE_MODEL_SUPPORTED_CAPABILITIES',
  'ACOSMI_DEFAULT_MAX_EFFORT_MODEL',
  'ACOSMI_DEFAULT_MAX_EFFORT_MODEL_DESCRIPTION',
  'ACOSMI_DEFAULT_MAX_EFFORT_MODEL_NAME',
  'ACOSMI_DEFAULT_MAX_EFFORT_MODEL_SUPPORTED_CAPABILITIES',
  'ACOSMI_DEFAULT_MODEL',
  'ACOSMI_DEFAULT_MODEL_DESCRIPTION',
  'ACOSMI_DEFAULT_MODEL_NAME',
  'ACOSMI_DEFAULT_MODEL_SUPPORTED_CAPABILITIES',
  'ACOSMI_SMALL_FAST_MODEL',
  'ACOSMI_SMALL_FAST_MODEL_AWS_REGION',
  'CRABCODE_SUBAGENT_MODEL',
] as const

const PROVIDER_MANAGED_ENV_VARS = new Set<string>(
  PROVIDER_MANAGED_ENV_VAR_NAMES,
)

const PROVIDER_MANAGED_ENV_PREFIXES = [
  // Per-model Vertex region overrides are dynamically named after model ids.
  'VERTEX_REGION_',
] as const

/**
 * Known sensitive values. Project/local/plugin settings never project these
 * process-wide, even after workspace trust; they may only come from a trusted
 * user/flag/policy/global source or the original secure spawn environment.
 *
 * Prefix/name-shape rules below extend this set with fail-closed coverage for
 * future keys. The exported exact set remains useful for audits and contracts.
 */
export const SENSITIVE_ENV_VARS: ReadonlySet<string> = new Set([
  ...PROVIDER_MANAGED_ENV_VAR_NAMES,
  // Supervisor bootstrap and its retired one-shot marker are internal-only;
  // no settings source may project either process-wide.
  'CRABCODE_BOOTSTRAP_CONFIG',
  'CRABCODE_INTERNAL_STRUCTURED_CUSTOM_BRIDGE_KEYS',
  // Request headers and telemetry destinations/credentials.
  'ACOSMI_CUSTOM_HEADERS',
  'OTEL_EXPORTER_OTLP_HEADERS',
  'OTEL_EXPORTER_OTLP_LOGS_HEADERS',
  'OTEL_EXPORTER_OTLP_LOGS_PROTOCOL',
  'OTEL_EXPORTER_OTLP_METRICS_CLIENT_CERTIFICATE',
  'OTEL_EXPORTER_OTLP_METRICS_CLIENT_KEY',
  'OTEL_EXPORTER_OTLP_METRICS_HEADERS',
  'OTEL_EXPORTER_OTLP_METRICS_PROTOCOL',
  'OTEL_EXPORTER_OTLP_PROTOCOL',
  'OTEL_EXPORTER_OTLP_TRACES_HEADERS',
  'OTEL_LOG_TOOL_DETAILS',
  'OTEL_LOG_USER_PROMPTS',
  'OTEL_LOGS_EXPORT_INTERVAL',
  'OTEL_LOGS_EXPORTER',
  'OTEL_METRIC_EXPORT_INTERVAL',
  'OTEL_METRICS_EXPORTER',
  'OTEL_METRICS_INCLUDE_ACCOUNT_UUID',
  'OTEL_METRICS_INCLUDE_SESSION_ID',
  'OTEL_METRICS_INCLUDE_VERSION',
  'OTEL_RESOURCE_ATTRIBUTES',
  // Cloud account context and credentials.
  'AWS_ACCESS_KEY_ID',
  'AWS_DEFAULT_REGION',
  'AWS_PROFILE',
  'AWS_REGION',
  'AWS_SECRET_ACCESS_KEY',
  'AWS_SESSION_TOKEN',
  'GOOGLE_APPLICATION_CREDENTIALS',
  'AZURE_CLIENT_ID',
  'AZURE_CLIENT_SECRET',
  'AZURE_CLIENT_CERTIFICATE_PATH',
  // Proxy/TLS trust and network routing.
  'ALL_PROXY',
  'HTTP_PROXY',
  'HTTPS_PROXY',
  'NO_PROXY',
  'NODE_EXTRA_CA_CERTS',
  'NODE_TLS_REJECT_UNAUTHORIZED',
  'SSL_CERT_DIR',
  'SSL_CERT_FILE',
  'REQUESTS_CA_BUNDLE',
  'CURL_CA_BUNDLE',
  'GIT_SSL_CAINFO',
  // Config/state identity. These must remain spawn/user-owned so
  // CRABCODE_CONFIG_DIR > CRABCODE_HOME cannot be changed by a project.
  'CRABCODE_CONFIG_DIR',
  'CRABCODE_HOME',
  'CRABCODE_STATE_DIR',
  'HOME',
  'USERPROFILE',
  // Dynamic loader and executable selection paths.
  'PATH',
  'PATHEXT',
  'SHELL',
  'COMSPEC',
  'BASH_ENV',
  'BASHOPTS',
  'CDPATH',
  'ENV',
  'IFS',
  'PROMPT_COMMAND',
  'SHELLOPTS',
  'ZDOTDIR',
  'NODE_OPTIONS',
  'NODE_PATH',
  'NODE_ENV',
  'LD_PRELOAD',
  'LD_LIBRARY_PATH',
  'DYLD_INSERT_LIBRARIES',
  'DYLD_LIBRARY_PATH',
  'PYTHONHOME',
  'PYTHONPATH',
  'PYTHONSTARTUP',
  'PYTHONINSPECT',
  'PERL5LIB',
  'PERL5OPT',
  'RUBYLIB',
  'RUBYOPT',
  'LUA_PATH',
  'LUA_CPATH',
  'LUA_INIT',
  'CLASSPATH',
  'JAVA_TOOL_OPTIONS',
  'JDK_JAVA_OPTIONS',
  '_JAVA_OPTIONS',
  'DOTNET_STARTUP_HOOKS',
  'CORECLR_PROFILER_PATH',
  'GIT_EXEC_PATH',
  'GIT_SSH',
  'GIT_SSH_COMMAND',
  'GIT_ASKPASS',
  'GIT_PROXY_COMMAND',
  'GIT_CONFIG_GLOBAL',
  'GIT_CONFIG_SYSTEM',
  'GIT_CONFIG_COUNT',
  'SSH_ASKPASS',
  'NPM_CONFIG_NODE_OPTIONS',
  'NPM_CONFIG_PREFIX',
  'NPM_CONFIG_SCRIPT_SHELL',
  'PNPM_HOME',
  'BUN_INSTALL',
  'ELECTRON_RUN_AS_NODE',
  'EDITOR',
  'VISUAL',
  'PAGER',
  'USE_BUILTIN_RIPGREP',
  // Helpers and security/activation switches that can execute code or relax a
  // gate. Known ordinary CRABCODE_/MCP_ controls are explicitly classified in
  // the pre/post sets above before these namespace defaults apply.
  'CRABCODE_API_KEY_HELPER_TTL_MS',
  'CRABCODE_CLI_BIN',
  'CRABCODE_PLUGIN_CACHE_DIR',
  'CRABCODE_PLUGIN_SEED_DIR',
  'CRABCODE_SHELL_PREFIX',
  'CRABCODE_TRUST_PROJECT',
  'CRABCODE_BROWSER_ALLOW_RISKY',
  'CRABCODE_BROWSER_PERMISSION_MODE',
  'CRABCODE_CHROME_PERMISSION_MODE',
  'DISABLE_LOGIN_COMMAND',
  'DISABLE_LOGOUT_COMMAND',
  'DISABLE_EXTRA_USAGE_COMMAND',
  'DISABLE_INSTALL_GITHUB_APP_COMMAND',
  'ENABLE_ACOSMI_MCP_SERVERS',
  'MCP_CLIENT_SECRET',
  'MCP_OAUTH_CLIENT_METADATA_URL',
  'USER_TYPE',
])

const SENSITIVE_ENV_PREFIXES = [
  ...PROVIDER_MANAGED_ENV_PREFIXES,
  'ACOSMI_',
  'CRABCODE_',
  'OTEL_',
  'AWS_',
  'AZURE_',
  'GOOGLE_',
  'MCP_',
  'XDG_',
  'DYLD_',
  'LD_',
  'GIT_CONFIG_',
  'CORECLR_',
  'COMPLUS_',
] as const

const SENSITIVE_ENV_NAME_PATTERNS = [
  /(?:^|_)(?:API_?KEY|AUTH_?TOKEN|ACCESS_?TOKEN|SECRET|PASSWORD|PRIVATE_?KEY|CLIENT_?KEY|CERTIFICATE|HEADERS?)$/,
  /(?:^|_)(?:API_BASE|BASE_URL|ENDPOINT|PROXY)$/,
  /(?:^|_)(?:MODEL|MODEL_ID|ORG_ID|ORGANIZATION_ID|PROJECT_ID|PROFILE|PROVIDER|REGION)$/,
] as const

export function classifyManagedEnvVar(key: string): ManagedEnvVarClass {
  const upper = key.toUpperCase()
  if (PRE_TRUST_SAFE_ENV_VARS.has(upper)) return 'pre-trust-safe'
  if (POST_TRUST_ALLOWED_ENV_VARS.has(upper)) return 'post-trust-allowed'
  if (
    SENSITIVE_ENV_VARS.has(upper) ||
    SENSITIVE_ENV_PREFIXES.some(prefix => upper.startsWith(prefix)) ||
    SENSITIVE_ENV_NAME_PATTERNS.some(pattern => pattern.test(upper))
  ) {
    return 'sensitive'
  }
  return 'unknown'
}

/** Project/local env outside the pre-trust set must be surfaced for trust. */
export function requiresProjectEnvTrust(key: string): boolean {
  return classifyManagedEnvVar(key) !== 'pre-trust-safe'
}

/**
 * Remote policy is a trusted source, but sensitive and unknown additions still
 * require the managed-settings security review. Known post-trust ordinary
 * controls do not.
 */
export function requiresManagedSettingsSecurityReview(key: string): boolean {
  const classification = classifyManagedEnvVar(key)
  return classification === 'sensitive' || classification === 'unknown'
}

export function isTrustedManagedEnvSource(source: ManagedEnvSource): boolean {
  return (
    source === 'globalConfig' ||
    source === 'userSettings' ||
    source === 'flagSettings' ||
    source === 'policySettings'
  )
}

export function isProviderManagedEnvVar(key: string): boolean {
  const upper = key.toUpperCase()
  return (
    PROVIDER_MANAGED_ENV_VARS.has(upper) ||
    PROVIDER_MANAGED_ENV_PREFIXES.some(prefix => upper.startsWith(prefix))
  )
}

/** Dangerous shell settings that can execute arbitrary shell code. */
export const DANGEROUS_SHELL_SETTINGS = [
  'apiKeyHelper',
  'awsAuthRefresh',
  'awsCredentialExport',
  'gcpAuthRefresh',
  'otelHeadersHelper',
  'statusLine',
] as const
