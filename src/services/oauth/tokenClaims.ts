import type { AccountInfo } from '../../utils/config.js'

type JwtClaims = Record<string, unknown>

function decodeBase64UrlJson(segment: string): JwtClaims | null {
  try {
    const normalized = segment.replace(/-/g, '+').replace(/_/g, '/')
    const padded = normalized.padEnd(
      normalized.length + ((4 - (normalized.length % 4)) % 4),
      '=',
    )
    const parsed = JSON.parse(Buffer.from(padded, 'base64').toString('utf8'))
    return typeof parsed === 'object' && parsed !== null ? parsed : null
  } catch {
    return null
  }
}

function readString(claims: JwtClaims, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = claims[key]
    if (typeof value !== 'string') continue
    const trimmed = value.trim()
    if (trimmed.length > 0) return trimmed
  }
  return undefined
}

function readHttpUrl(claims: JwtClaims, ...keys: string[]): string | undefined {
  const raw = readString(claims, ...keys)
  if (!raw) return undefined
  return /^https?:\/\//i.test(raw) ? raw : undefined
}

/**
 * Best-effort local projection for Acosmi OAuth tokens whose profile endpoint
 * is unavailable. This does not validate the JWT signature; callers must only
 * use it after the token has already been accepted as a local Acosmi credential.
 */
export function deriveAccountInfoFromOAuthTokenClaims(
  accessToken: string,
): AccountInfo | null {
  const [, payload] = accessToken.split('.')
  if (!payload) return null

  const claims = decodeBase64UrlJson(payload)
  if (!claims) return null

  const emailAddress = readString(claims, 'email', 'email_address')
  const accountUuid = readString(
    claims,
    'userId',
    'user_id',
    'accountUuid',
    'account_uuid',
    'sub',
  )
  // [DESKTOP-OAUTH-401-FIX 2026-06-16] 仅 accountUuid 为硬必需。email 不再硬要求:
  // 大量 C 端用户为手机号注册、access token 本就无 email claim, 旧逻辑 `!emailAddress`
  // 直接返回 null 会让 /api/oauth/profile 不可达时的 account/read 兜底失效。
  // account 对象只要带 accountUuid 即可解锁; emailAddress 缺失退化为空串 (类型仍为 string)。
  if (!accountUuid) return null

  const organizationUuid = readString(
    claims,
    'tenantId',
    'tenant_id',
    'organizationUuid',
    'organization_uuid',
    'orgUuid',
    'org_uuid',
  )
  const displayName = readString(
    claims,
    'displayName',
    'display_name',
    'name',
    'preferred_username',
  )
  const avatarUrl = readHttpUrl(claims, 'avatarUrl', 'avatar_url', 'picture')
  const imageUrl = readHttpUrl(claims, 'imageUrl', 'image_url')

  return {
    accountUuid,
    emailAddress: emailAddress ?? '',
    organizationUuid,
    ...(displayName ? { displayName } : {}),
    ...(avatarUrl ? { avatarUrl } : {}),
    ...(imageUrl ? { imageUrl } : {}),
  }
}
