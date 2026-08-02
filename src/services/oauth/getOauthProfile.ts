import axios from 'axios'
import { getOauthConfig } from 'src/constants/oauth.js'
import type { OAuthProfileResponse } from 'src/services/oauth/types.js'
import { getAcosmiApiKey } from 'src/utils/auth.js'
import { getGlobalConfig } from 'src/utils/config.js'
import { logError } from 'src/utils/log.js'
export async function getOauthProfileFromApiKey(): Promise<
  OAuthProfileResponse | undefined
> {
  // Assumes interactive session
  const config = getGlobalConfig()
  const accountUuid = config.oauthAccount?.accountUuid
  const apiKey = getAcosmiApiKey()

  // Need both account UUID and API key to check
  if (!accountUuid || !apiKey) {
    return
  }
  const endpoint = `${getOauthConfig().BASE_API_URL}/api/acosmi_cli_profile`
  try {
    const response = await axios.get<OAuthProfileResponse>(endpoint, {
      headers: {
        'x-api-key': apiKey,
      },
      params: {
        account_uuid: accountUuid,
      },
      timeout: 10000,
    })
    return response.data
  } catch (error) {
    logError(error as Error)
  }
}

export async function getOauthProfileFromOauthToken(
  accessToken: string,
): Promise<OAuthProfileResponse | undefined> {
  const endpoint = `${getOauthConfig().BASE_API_URL}/api/oauth/profile`
  try {
    const response = await axios.get<OAuthProfileResponse>(endpoint, {
      headers: {
        Authorization: `Bearer ${accessToken}`,
        'Content-Type': 'application/json',
      },
      timeout: 10000,
    })
    return response.data
  } catch (error) {
    logError(error as Error)
  }
}

/**
 * V116.1 P1-5 (2026-07-24):登录态零计费探针。
 *
 * 与 getOauthProfileFromOauthToken 的区别:那个吞掉一切错误返回 undefined
 * (登录流的容错语义),本函数保留错误分类 —— verifyApiKey 需要区分
 * 「令牌失效」与「网关暂不可用」,后者绝不能呈现为登录失败。
 *
 * 分类:
 *   - 2xx                                  → ok
 *   - 401 / 403(authentication_error)      → auth-expired(令牌真失效)
 *   - 其余(403 权限、404 老网关无路由、5xx、网络/超时) → unavailable
 */
export type OauthProfileProbeResult =
  | { kind: 'ok' }
  | { kind: 'auth-expired' }
  | { kind: 'unavailable'; status: number | null }

export async function probeOauthProfile(
  accessToken: string,
): Promise<OauthProfileProbeResult> {
  const endpoint = `${getOauthConfig().BASE_API_URL}/api/oauth/profile`
  try {
    await axios.get(endpoint, {
      headers: {
        Authorization: `Bearer ${accessToken}`,
        'Content-Type': 'application/json',
      },
      timeout: 10000,
    })
    return { kind: 'ok' }
  } catch (error) {
    const status = axios.isAxiosError(error)
      ? (error.response?.status ?? null)
      : null
    if (status === 401) return { kind: 'auth-expired' }
    if (status === 403) {
      const body = axios.isAxiosError(error)
        ? (error.response?.data as
            | { error?: { type?: string } }
            | undefined)
        : undefined
      if (body?.error?.type === 'authentication_error') {
        return { kind: 'auth-expired' }
      }
    }
    return { kind: 'unavailable', status }
  }
}
