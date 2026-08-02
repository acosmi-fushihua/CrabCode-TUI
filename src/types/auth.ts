/**
 * Auth-related shared types — 历史命名沿自 hub bridge 时代 (BridgeXxx),
 * M6 (2026-05-02) 后 hub 系统全清, type 定义独立于 IPC 层.
 *
 * 现役语义 = SDK getAuthStatus (services/acosmi/client.ts) 返回形态.
 * utils/auth.ts setBridgeAuthCache / getBridgeAuthCache 用作 cache shape.
 */

export interface BridgeOAuthTokens {
  accessToken: string
  refreshToken?: string
  expiresAt?: number
  scopes: string[]
  clientId?: string
  serverUrl?: string
}

export interface BridgeAuthStatus {
  authorized: boolean
  tokens: BridgeOAuthTokens | null
  hasProfileScope: boolean
  isSubscriber: boolean
}
