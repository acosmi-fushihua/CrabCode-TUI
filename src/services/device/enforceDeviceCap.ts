// W-DEVICE-ACCOUNT-CAP 2026-06-29 — 客户端设备账号上限强制（网关权威）。
//
// 登录拿到 desktop OAuth token 后、**激活凭据前**调用网关
// `POST /oauth/desktop/devices/bind {device_id}`（Bearer 该 token）。网关校验本设备
// 账号上限：
//   - 超限 → HTTP 409 → 抛 DeviceAccountCapError（带 deviceId）→ 登录中止、丢弃刚拿
//     的 token、上层弹拦截（微信二维码 + deviceId）。
//   - 200 / 其它 → 放行。
//
// **Fail-open**：网络错误 / 5xx / 网关不可达 → 放行（仅 logError），绝不把正常用户
// 锁在登录外。网关是权威真源、离线是少数；硬强制只认显式 409。

import axios from 'axios'

import { getOauthConfig } from '../../constants/oauth.js'
import { logError } from '../../utils/log.js'
import { getDeviceId } from './deviceId.js'

/** TUI 据此识别「设备账号上限」错误。错误 message = `${MARKER}|${deviceId}`。 */
export const DEVICE_ACCOUNT_CAP_MARKER = 'device_account_limit_exceeded'

export class DeviceAccountCapError extends Error {
  readonly deviceId: string
  constructor(deviceId: string) {
    super(`${DEVICE_ACCOUNT_CAP_MARKER}|${deviceId}`)
    this.name = 'DeviceAccountCapError'
    this.deviceId = deviceId
  }
}

/**
 * 从一个可能跨进程传递的错误 message 解析出 deviceId。
 * 非「设备账号上限」错误返回 null。
 */
export function parseDeviceCapError(
  message: string | null | undefined,
): { deviceId: string } | null {
  if (!message) return null
  const idx = message.indexOf(DEVICE_ACCOUNT_CAP_MARKER)
  if (idx === -1) return null
  const rest = message.slice(idx + DEVICE_ACCOUNT_CAP_MARKER.length)
  const deviceId = rest.startsWith('|') ? rest.slice(1).trim() : ''
  return { deviceId }
}

/**
 * 强制本设备账号上限。超限抛 DeviceAccountCapError；否则（含 fail-open）正常返回。
 */
export async function enforceDeviceAccountCap(accessToken: string): Promise<void> {
  const deviceId = getDeviceId()
  const endpoint = `${getOauthConfig().BASE_API_URL}/oauth/desktop/devices/bind`
  try {
    await axios.post(
      endpoint,
      { device_id: deviceId },
      {
        headers: {
          Authorization: `Bearer ${accessToken}`,
          'Content-Type': 'application/json',
        },
        timeout: 10000,
      },
    )
    // 200 → 已绑定（或网关 fail-open soft）→ 放行。
  } catch (error) {
    const status =
      typeof error === 'object' && error !== null
        ? (error as { response?: { status?: number } }).response?.status
        : undefined
    if (status === 409) {
      throw new DeviceAccountCapError(deviceId)
    }
    // 网络 / 5xx / 其它 → fail-open（网关权威；不锁正常用户）。
    logError(error as Error)
  }
}
