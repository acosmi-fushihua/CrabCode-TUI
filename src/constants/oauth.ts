import { getGlobalConfigOauthSuffix } from '../utils/configFilePath.js'

export const fileSuffixForOauthConfig = getGlobalConfigOauthSuffix

type OauthConfigType = 'prod' | 'staging' | 'local'

function getOauthConfigType(): OauthConfigType {
  switch (getGlobalConfigOauthSuffix()) {
    case '-local-oauth':
      return 'local'
    case '-staging-oauth':
      return 'staging'
    default:
      return 'prod'
  }
}

// ── Acosmi Desktop OAuth Scopes（来自 /.well-known/oauth-authorization-server/desktop）──
export const ACOSMI_SCOPES = [
  'ai',       // 模型调用 + 流量包 + 权益
  'skills',   // 技能商店 + 工具
  'account',  // 个人资料 + 钱包
] as const

// ── 兼容层：旧代码引用的常量映射到新 scope ──
export const ACOSMI_INFERENCE_SCOPE = 'ai' as const
export const ACOSMI_PROFILE_SCOPE = 'account' as const
export const OAUTH_BETA_HEADER = 'oauth-2025-04-20' as const

export const CONSOLE_OAUTH_SCOPES = ACOSMI_SCOPES
export const ACOSMI_OAUTH_SCOPES = ACOSMI_SCOPES
export const ALL_OAUTH_SCOPES = [...ACOSMI_SCOPES]

type OauthConfig = {
  BASE_API_URL: string
  CONSOLE_AUTHORIZE_URL: string
  ACOSMI_AUTHORIZE_URL: string
  /**
   * The acosmi.com web origin. Separate from ACOSMI_AUTHORIZE_URL because
   * that now routes through acosmi.com/cai/* for attribution — deriving
   * .origin from it would give acosmi.com, breaking links to /code,
   * /settings/connectors, and other acosmi.com web pages.
   */
  ACOSMI_ORIGIN: string
  TOKEN_URL: string
  API_KEY_URL: string
  ROLES_URL: string
  CONSOLE_SUCCESS_URL: string
  ACOSMI_SUCCESS_URL: string
  MANUAL_REDIRECT_URL: string
  CLIENT_ID: string
  OAUTH_FILE_SUFFIX: string
  MCP_PROXY_URL: string
  MCP_PROXY_PATH: string
}

// Production OAuth configuration — Acosmi platform
// OAuth endpoint layout:
//   - OAuth flow 端点在 /oauth/desktop/* (CONSOLE_AUTHORIZE_URL / TOKEN_URL); 成功页
//     /oauth/code/success (CONSOLE_SUCCESS_URL) 由网关 :8009 服务。
//   - 网关业务主体在 /api/v4/* (SDK @acosmi/sdk-ts 的 apiURL() 自动加 /api/v4 前缀,
//     managed-models 等走它故正常)。
//   - 本端 BASE_API_URL=root host 直拼的 /api/oauth/profile 与
//     /api/organization/crab_code_first_token_date 此前网关未实现, 被 nginx 兜底转
//     Java tk-dist (:48080) → 401。网关 (Go :8009) 提供
//     这两个端点, nginx 用 `location =` 精确路由到 :8009 → 现可正常返回 200。
//   - ⚠️ oauth/roles (ROLES_URL) / oauth/create_api_key (API_KEY_URL) 仍未在网关实现,
//     如有调用同样会经兜底 401/404, 待网关按需补实后再依赖。
//
// 历史: BASE_API_URL 曾被错改为 'https://acosmi.com/api/v4', 导致所有
// `${BASE_API_URL}/api/...` callers 拼成 /api/v4/api/... → 404。
// 根因修 2026-05-05: BASE_API_URL 改回 root host (本端口直拼 /api/... 是对的);
// 真正缺的是网关侧端点实现, 已于 2026-06-16 补齐 (见上)。
const PROD_OAUTH_CONFIG = {
  BASE_API_URL: 'https://acosmi.com',
  CONSOLE_AUTHORIZE_URL: 'https://acosmi.com/oauth/desktop/authorize',
  ACOSMI_AUTHORIZE_URL: 'https://acosmi.com/oauth/desktop/authorize',
  ACOSMI_ORIGIN: 'https://acosmi.com',
  TOKEN_URL: 'https://acosmi.com/oauth/desktop/token',
  API_KEY_URL: 'https://acosmi.com/api/oauth/create_api_key',
  ROLES_URL: 'https://acosmi.com/api/oauth/roles',
  CONSOLE_SUCCESS_URL:
    'https://acosmi.com/oauth/code/success?app=crabcode',
  ACOSMI_SUCCESS_URL:
    'https://acosmi.com/oauth/code/success?app=crabcode',
  MANUAL_REDIRECT_URL: 'https://acosmi.com/oauth/code/callback',
  CLIENT_ID: 'crabcode-cli',  // 动态注册时会覆盖，此为 fallback
  OAUTH_FILE_SUFFIX: '',
  MCP_PROXY_URL: 'https://acosmi.com',
  // TODO 2026-05-05: 同 BASE_API_URL 修复一并审视。强推断应为 '/api/mcp/{server_id}'
  // (网关上不存在 /api/v4/ 层级, 见同文件 PROD_OAUTH_CONFIG 注释)。
  // 但本次未直接验证 (需真实 server_id), 保留原值留作 follow-up; MCP 调用走此路径
  // 时若 404 见此注释。
  MCP_PROXY_PATH: '/api/v4/mcp/{server_id}',
} as const

/**
 * Client ID Metadata Document URL for MCP OAuth (CIMD / SEP-991).
 * When an MCP auth server advertises client_id_metadata_document_supported: true,
 * CrabCode uses this URL as its client_id instead of Dynamic Client Registration.
 * The URL must point to a JSON document hosted by Acosmi.
 * See: https://datatracker.ietf.org/doc/html/draft-ietf-oauth-client-id-metadata-document-00
 */
export const MCP_CLIENT_METADATA_URL =
  'https://acosmi.com/oauth/crabcode-client-metadata'

// Staging OAuth configuration - only included in ant builds with staging flag
// Uses literal check for dead code elimination
const STAGING_OAUTH_CONFIG =
  process.env.USER_TYPE === 'ant'
    ? ({
        BASE_API_URL: 'https://staging.acosmi.com/api/v4',
        CONSOLE_AUTHORIZE_URL:
          'https://staging.acosmi.com/api/v4/oauth/authorize',
        ACOSMI_AUTHORIZE_URL:
          'https://staging.acosmi.com/api/v4/oauth/authorize',
        ACOSMI_ORIGIN: 'https://staging.acosmi.com',
        TOKEN_URL: 'https://staging.acosmi.com/api/v4/oauth/token',
        API_KEY_URL:
          'https://staging.acosmi.com/api/v4/oauth/create_api_key',
        ROLES_URL:
          'https://staging.acosmi.com/api/v4/oauth/roles',
        CONSOLE_SUCCESS_URL:
          'https://staging.acosmi.com/oauth/code/success?app=crabcode',
        ACOSMI_SUCCESS_URL:
          'https://staging.acosmi.com/oauth/code/success?app=crabcode',
        MANUAL_REDIRECT_URL:
          'https://staging.acosmi.com/oauth/code/callback',
        CLIENT_ID: '22422756-60c9-4084-8eb7-27705fd5cf9a',
        OAUTH_FILE_SUFFIX: '-staging-oauth',
        MCP_PROXY_URL: 'https://staging.acosmi.com',
        MCP_PROXY_PATH: '/api/v4/mcp/{server_id}',
      } as const)
    : undefined

// Three local dev servers: :8000 api-proxy (`api dev start -g ccr`),
// :4000 acosmi.com frontend, :3000 Console frontend. Env vars let
// scripts/crabcode-localhost override if your layout differs.
function getLocalOauthConfig(): OauthConfig {
  const api =
    process.env.CRABCODE_LOCAL_OAUTH_API_BASE?.replace(/\/$/, '') ??
    'http://localhost:8000'
  const apps =
    process.env.CRABCODE_LOCAL_OAUTH_APPS_BASE?.replace(/\/$/, '') ??
    'http://localhost:4000'
  const consoleBase =
    process.env.CRABCODE_LOCAL_OAUTH_CONSOLE_BASE?.replace(/\/$/, '') ??
    'http://localhost:3000'
  return {
    BASE_API_URL: api,
    CONSOLE_AUTHORIZE_URL: `${consoleBase}/oauth/authorize`,
    ACOSMI_AUTHORIZE_URL: `${apps}/oauth/authorize`,
    ACOSMI_ORIGIN: apps,
    TOKEN_URL: `${api}/v1/oauth/token`,
    API_KEY_URL: `${api}/api/oauth/crabcode_cli/create_api_key`,
    ROLES_URL: `${api}/api/oauth/crabcode_cli/roles`,
    CONSOLE_SUCCESS_URL: `${consoleBase}/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dcrabcode`,
    ACOSMI_SUCCESS_URL: `${consoleBase}/oauth/code/success?app=crabcode`,
    MANUAL_REDIRECT_URL: `${consoleBase}/oauth/code/callback`,
    CLIENT_ID: '22422756-60c9-4084-8eb7-27705fd5cf9a',
    OAUTH_FILE_SUFFIX: '-local-oauth',
    MCP_PROXY_URL: 'http://localhost:8205',
    MCP_PROXY_PATH: '/v1/toolbox/shttp/mcp/{server_id}',
  }
}

// Allowed base URLs for CRABCODE_CUSTOM_OAUTH_URL override.
// Only FedStart/PubSec deployments are permitted to prevent OAuth tokens
// from being sent to arbitrary endpoints.
const ALLOWED_OAUTH_BASE_URLS = [
  'https://beacon.staging.acosmi.com',
  'https://crabcode.fedstart.com',
  'https://crabcode-staging.fedstart.com',
]

// Default to prod config, override with test/staging if enabled
export function getOauthConfig(): OauthConfig {
  let config: OauthConfig = (() => {
    switch (getOauthConfigType()) {
      case 'local':
        return getLocalOauthConfig()
      case 'staging':
        return STAGING_OAUTH_CONFIG ?? PROD_OAUTH_CONFIG
      case 'prod':
        return PROD_OAUTH_CONFIG
    }
  })()

  // Allow overriding all OAuth URLs to point to an approved FedStart deployment.
  // Only allowlisted base URLs are accepted to prevent credential leakage.
  const oauthBaseUrl = process.env.CRABCODE_CUSTOM_OAUTH_URL
  if (oauthBaseUrl) {
    const base = oauthBaseUrl.replace(/\/$/, '')
    if (!ALLOWED_OAUTH_BASE_URLS.includes(base)) {
      throw new Error(
        'CRABCODE_CUSTOM_OAUTH_URL is not an approved endpoint.',
      )
    }
    config = {
      ...config,
      BASE_API_URL: base,
      CONSOLE_AUTHORIZE_URL: `${base}/oauth/authorize`,
      ACOSMI_AUTHORIZE_URL: `${base}/oauth/authorize`,
      ACOSMI_ORIGIN: base,
      TOKEN_URL: `${base}/v1/oauth/token`,
      API_KEY_URL: `${base}/api/oauth/crabcode_cli/create_api_key`,
      ROLES_URL: `${base}/api/oauth/crabcode_cli/roles`,
      CONSOLE_SUCCESS_URL: `${base}/oauth/code/success?app=crabcode`,
      ACOSMI_SUCCESS_URL: `${base}/oauth/code/success?app=crabcode`,
      MANUAL_REDIRECT_URL: `${base}/oauth/code/callback`,
      OAUTH_FILE_SUFFIX: '-custom-oauth',
    }
  }

  // Allow CLIENT_ID override via environment variable (e.g., for Xcode integration)
  const clientIdOverride = process.env.CRABCODE_OAUTH_CLIENT_ID
  if (clientIdOverride) {
    config = {
      ...config,
      CLIENT_ID: clientIdOverride,
    }
  }

  return config
}

/**
 * Path under the Acosmi web origin for the membership upgrade / purchase page.
 *
 * Points at the all-tiers pricing / subscription page (`/zh/pricing`) — the
 * canonical destination used by account upgrade actions. The former
 * `/upgrade/max` was a dead route: acosmi.com locale-redirects it to
 * `/zh/upgrade/max`, which 404s (confirmed on prod 2026-07-05). `/pricing` is
 * the real page listing every plan.
 *
 * Kept separate from the URL so callers that already have an origin (or a
 * differently-hosted deployment) can compose their own link, but the canonical
 * full URL is {@link getUpgradeUrl}.
 */
export const ACOSMI_UPGRADE_PATH = '/zh/pricing' as const

/**
 * Canonical membership upgrade / purchase URL.
 *
 * Derived from {@link getOauthConfig}'s `ACOSMI_ORIGIN` so it automatically
 * follows the active deployment (prod / staging / approved FedStart custom
 * origin) instead of hard-coding `acosmi.com`. This is the single source of
 * truth for the upgrade link used by `/upgrade`, the model-selection gate
 * login/upgrade guidance, and any other "go buy a plan" affordance.
 *
 * `acosmi.com` is the product's own brand, not a third-party model brand, so
 * surfacing it here does not violate the model-brand zero-tolerance rule.
 */
export function getUpgradeUrl(): string {
  return `${getOauthConfig().ACOSMI_ORIGIN}${ACOSMI_UPGRADE_PATH}`
}
