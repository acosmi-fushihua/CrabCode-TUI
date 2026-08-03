import {
  Client,
  classifySourcesEvent,
  EventComplete,
  EventError,
  discover,
  getAdapterForModel,
  revokeToken,
  // SDK 真值源 type 名 → 本地 alias rename。SDK 端 export 名按上游约定保留；
  // 本仓内 caller 一律使用 AcosmiResponse。
  type AnthropicResponse as AcosmiResponse,
  type ChatRequest,
  type LoginEvent,
  type LoginOptions,
  type ManagedModel,
  type ServerMetadata,
  type StreamEvent,
  type TokenSet,
  type WSConfig,
} from '@acosmi/sdk-ts'
import type { NormalizedAcosmiChatStreamEvent } from '../../types/api-types.js'
import { assertRuntimeModel } from '../../utils/model/runtimeModelResolution.js'
import { isNonGatewayModelReference } from '../../utils/model/nonGatewayModelReference.js'
import { getOauthConfig } from '../../constants/oauth.js'
import { PROVIDER_BOUND_CHAT_TIMEOUT_MS } from '../providerBoundRequestPolicy.js'

export type { AcosmiResponse }
export type { ChatRequest, LoginEvent, LoginOptions } from '@acosmi/sdk-ts'

let _clientPromise: Promise<Client> | null = null

export function getAcosmiClient(): Promise<Client> {
  if (!_clientPromise) {
    _clientPromise = Client.create()
  }
  return _clientPromise
}

// ---------------------------------------------------------------------------
// W-WORKER-SDK-URL-WIRE (2026-05-27) — SDK Client serverURL override.
//
// 立项目标: PR-7-A 已把 `sdk_url` 通过 protocol → dispatcher → worker
// 通路 LANDED 到 `TurnRuntimeSessionContext`; worker 当前仅 `console.warn`
// log 观察值 ("no SDK Client URL-override hook today — declare-now-emit-later")。
//
// 真根因更新 (2026-05-27): SDK @acosmi/sdk-ts 1.0.2 的 `Client.create(cfg?)`
// 已支持 `Config.serverURL?: string`（SDK d.ts interface Config）。
// worker 注释 "no public URL-override hook today" 是过时认知。正确解法是在
// 此模块顶层 singleton `_clientPromise` 首次构造之前注入 cfg，所有现有
// caller (7+ 处) 透明继承 with-URL client，**caller 0 改**。
//
// per-turn Bun child 进程模型 (`@crabcode/agent-worker` spawn per turn)
// 保证 singleton lifecycle = turn lifecycle，无跨 turn URL 切换语义需考虑。
// ---------------------------------------------------------------------------

/**
 * 初始化 Acosmi SDK Client，注入 serverURL override。
 *
 * 必须在首次 `getAcosmiClient()` 调用之前调。一旦 `_clientPromise` 已
 * 初始化（无论 with-cfg 还是 without-cfg），此函数抛错防止 silent
 * override miss。
 *
 * 设计选择 — caller 兼容性：7+ 处现有 `getAcosmiClient()` caller 保持
 * 0-arg 不变；只在 worker bootstrap 早期调本函数注入 URL。per-turn Bun
 * child 进程模型保证 singleton lifecycle = turn lifecycle，无跨 turn
 * URL 切换语义需考虑。
 *
 * @param serverURL nexus-v4 API 根地址（SDK 自动追加 `/api/v4`；空字符串
 *                  视为未提供，由 caller 在调用前过滤）
 * @throws Error 当 `_clientPromise` 已存在（`getAcosmiClient` 或本函数
 *               之前已调）— silent override miss 防御
 */
export async function initAcosmiClientWithServerUrl(
  serverURL: string,
): Promise<Client> {
  if (_clientPromise) {
    throw new Error(
      `initAcosmiClientWithServerUrl: Acosmi client already initialized; serverURL '${serverURL}' cannot be set after first use`,
    )
  }
  _clientPromise = Client.create({ serverURL })
  return _clientPromise
}

/** 仅供单测使用 — 重置 _clientPromise singleton 允许重新 init. */
export function _resetAcosmiClientForTesting(): void {
  _clientPromise = null
}

/**
 * 仅供单测使用 — 直接注入一个替身 client(与 _resetAcosmiClientForTesting 配对)。
 *
 * Why: 需要驱动 `getAcosmiClient()` 消费方(如 refreshModelCapabilities)的测试,
 * 若改用 `mock.module('services/acosmi/client.js')` 会是**进程级**替换且跨文件
 * 存活 —— 一旦本模块被提前实例化, 后续依赖 mock `@acosmi/sdk-ts` 的测试
 * (initAcosmiClientWithServerUrl 的 Client.create 计数) 就再也拿不到自己的
 * 替身。经此接缝注入只动本模块 singleton, 用完 reset 即净, 不污染任何其他文件。
 */
export function _setAcosmiClientForTesting(client: unknown): void {
  _clientPromise = client as Promise<Client>
}

// ---------------------------------------------------------------------------
// W-BUG-C SDK Reload (2026-05-14) — agent-worker token sync from disk.
//
// Bug 现象: 用户 OAuth 登录成功, tokens.json 已写盘, 但 agent-worker (per-turn
// Bun child) 进程内的 SDK Client 永远卡在 null token, 发消息 100% 报
// "API Error: not authorized, call login() first".
//
// 根因: SDK `Client.create()` 只在构造时 (index.mjs:2226-2236) 调一次 `store.load()`.
// 此后 `ensureToken()` (index.mjs:2418-2424) 看到 `this.tokens == null` 直接 throw,
// 不会 fall back 到 `syncFromDisk()`. agent-worker spawn 时机比 OAuth 完成早,
// tokens.json 写盘晚约 10 秒 (race), per-turn worker 没有任何 reload 机制.
//
// SDK 1.0.2 暴露了 `syncFromDisk()` (index.mjs:2532-2543) 但只在内部 refresh /
// forceRefresh 路径自调; 这里把它显式暴露给 agent-worker 入口 + 每次 turn 前调用,
// best-effort 失败吞掉. ≥1s cooldown 防 hammer disk.
//
// 不动 `_clientPromise` 单例 (Client 构造有 WebSocket 副作用, 重建副作用难预测).
// 不订阅 `account/login/completed` notification (per-turn worker 不在 control-plane
// broadcast 下游).
// ---------------------------------------------------------------------------

const SYNC_COOLDOWN_MS = 1_000
let lastSyncAt = 0

/** 单测用 — 注入 fake client + fake clock 替代真 SDK Client 副作用.
 *
 * SDK d.ts (index.d.ts:438) 把 `syncFromDisk` 声明为 private — 实例上 runtime 方法
 * 真实存在 (index.mjs:2532-2543) 但 TS 视角不可见. 这里在抽象层把 client 类型
 * 收成只有 `syncFromDisk` 的最小 shape, 真 SDK Client 通过 unknown-cast 适配进来,
 * 单测 fake 自然匹配该 shape；外部 SDK 内部行为保持不变。 */
interface SyncableClient {
  syncFromDisk: () => Promise<void>
}

interface SyncDeps {
  getClient: () => Promise<SyncableClient>
  now: () => number
}

const defaultDeps: SyncDeps = {
  getClient: async () => {
    const client = await getAcosmiClient()
    // SDK Client.syncFromDisk 在 d.ts 是 private (但 runtime 存在). 借
    // unknown 双跳穿透 TS 私有性闸门 — 仍是 type-safe SyncableClient 形态.
    return client as unknown as SyncableClient
  },
  now: () => Date.now(),
}

/**
 * Best-effort 从磁盘 (~/.acosmi/tokens.json) 同步 SDK Client 的 token 状态.
 *
 * 调用形态:
 *   - agent-worker bootstrap 后立即调一次 (覆盖 worker spawn 早于 OAuth 写盘的 race).
 *   - 每个 turn 开始前调一次 (覆盖 long-lived worker 在 OAuth 之后才接到第一个 turn).
 *
 * 防御:
 *   - ≥1s cooldown — caller 反复触发不会真打盘.
 *   - syncFromDisk() 抛错吞掉 — best-effort 不传播失败.
 *   - 不重建 `_clientPromise` — 保留 WebSocket 等副作用.
 *
 * 返回 true = 实际触发了 sync; false = 命中 cooldown 跳过.
 *
 * 测试钩子:
 *   - `_syncAcosmiClientFromDiskForTesting(deps)` 允许注入 fake client + clock.
 */
export async function syncAcosmiClientFromDisk(): Promise<boolean> {
  return runSync(defaultDeps)
}

/** 仅供单测使用 — 注入 fake client / clock 跑 syncAcosmiClientFromDisk 内逻辑. */
export async function _syncAcosmiClientFromDiskForTesting(deps: {
  getClient: () => Promise<SyncableClient>
  now: () => number
}): Promise<boolean> {
  return runSync(deps)
}

async function runSync(deps: SyncDeps): Promise<boolean> {
  const now = deps.now()
  if (now - lastSyncAt < SYNC_COOLDOWN_MS) {
    return false
  }
  lastSyncAt = now
  try {
    const client = await deps.getClient()
    await client.syncFromDisk()
  } catch (err) {
    // best-effort — 失败不传播; 写一行 debug log 留痕.
    void import('../../utils/debug.js')
      .then(m =>
        m.logForDebugging(
          `[acosmi-client] syncAcosmiClientFromDisk failed: ${
            err instanceof Error ? err.message : String(err)
          }`,
        ),
      )
      .catch(() => {})
  }
  return true
}

/** 仅供单测使用 — 重置 cooldown timestamp. */
export function _resetSyncCooldownForTesting(): void {
  lastSyncAt = 0
}

/** 仅供单测使用 — 读取当前 cooldown timestamp. */
export function _getLastSyncAtForTesting(): number {
  return lastSyncAt
}

// ---------------------------------------------------------------------------
// Auth — Y 路径 step 8 (2026-05-01): SDK Client 直调替代原 hub auth.* 3 RPC.
// loginStream / logout / getAuthStatus 是 7 caller 共享基础设施.
// M5/M6 (2026-05-02) 后原 bridgeAuth* IPC 中转层全删, 此处保留 caller-facing
// API 形态 (返回值结构对齐 BridgeAuthStatus, 见 src/types/auth.ts) 仅为
// 避免 caller 端改用法, 与 hub bridge 已无任何 IPC 依赖.
// ---------------------------------------------------------------------------

const APP_NAME = 'crabcode'

/**
 * SDK Client.loginWithHandler 是 callback-based; 包成 AsyncGenerator
 * 让 caller 继续用 `for await (const event of loginStream(...))` 形态.
 *
 * Event shape 与原 BridgeLoginEvent 完全对齐:
 *   { type: 'auth_url' | 'complete' | 'error', url?, err_code?, error? }
 */
export async function* loginStream(
  scopes: string[],
  options?: LoginOptions,
  signal?: AbortSignal,
): AsyncGenerator<LoginEvent, void, undefined> {
  const client = await getAcosmiClient()

  // Bounded queue + waiting promise = handler→generator bridge
  const queue: LoginEvent[] = []
  let waiter: ((e: LoginEvent | null) => void) | null = null
  let finished = false
  let loginError: unknown = null

  function deliver(event: LoginEvent | null): void {
    if (waiter) {
      const w = waiter
      waiter = null
      w(event)
    } else if (event) {
      queue.push(event)
    }
  }

  const handler = (event: LoginEvent): void => {
    deliver(event)
  }

  // 桌面 loopback 授权成功后, 让浏览器 302 到品牌成功页 (acosmi.com /oauth/code/success),
  // 替代默认在 127.0.0.1 本地回环渲染的"授权成功"HTML —— 地址栏显示业务域名而非裸 IP。
  // 用 ACOSMI_SUCCESS_URL (acosmi.com 域) 而非 CONSOLE_SUCCESS_URL (prod 二者同值, 但
  // 语义上成功页归 acosmi.com web)。授权码仍先回投本地回环, 安全模型不变; SDK 在 URL
  // 缺失/非法时自动回退本地 HTML (零回归)。caller 已显式给 successRedirectURL 则尊重其值。
  const loginOptions: LoginOptions = {
    successRedirectURL: getOauthConfig().ACOSMI_SUCCESS_URL,
    ...options,
  }

  const loginPromise = client
    .loginWithHandler(APP_NAME, scopes, handler, loginOptions, signal)
    .catch((err: unknown) => {
      loginError = err
    })
    .finally(() => {
      finished = true
      deliver(null)
    })

  try {
    while (true) {
      if (queue.length > 0) {
        const next = queue.shift()!
        yield next
        if (next.type === EventComplete) {
          // Side effect: open SDK WS subscription (替代 Go hub WSClient.Connect).
          // best-effort — 失败不影响 login 结果.
          void connectWebSocketSilently(['balance', 'system'])
        }
        continue
      }
      if (finished) break
      const event = await new Promise<LoginEvent | null>(resolve => {
        waiter = resolve
      })
      if (event === null) break
      yield event
      if (event.type === EventComplete) {
        void connectWebSocketSilently(['balance', 'system'])
      }
      if (event.type === EventError) {
        // continue draining — error event is informational, login may still settle
      }
    }
  } finally {
    await loginPromise
  }

  if (loginError) throw loginError
}

async function connectWebSocketSilently(topics: string[]): Promise<void> {
  try {
    const client = await getAcosmiClient()
    const cfg: WSConfig = { topics, autoReconnect: true }
    await client.connect(cfg)
  } catch {
    // best-effort — push events 在过渡期可继续走 hub 路径
  }
}

/**
 * SDK Client.logout — 撤销 token + 清本地 SDK 内 tokens.
 * 同步 disconnect WS (登录期 connectWebSocketSilently 的对偶).
 */
export async function logout(signal?: AbortSignal): Promise<void> {
  const client = await getAcosmiClient()
  await logoutClientWithParallelRevoke(client, signal, defaultLogoutDeps)
}

type LogoutClient = Pick<
  Client,
  'disconnect' | 'logout' | 'getTokenSet' | 'serverURL' | 'meta' | 'tokens'
>

interface LogoutDeps {
  discover: typeof discover
  revokeToken: typeof revokeToken
}

const defaultLogoutDeps: LogoutDeps = { discover, revokeToken }

/**
 * PR-5 SDK/network follow-up — SDK `Client.logout()` revokes access then
 * refresh token serially. The SDK now publicly exports `discover` and
 * `revokeToken`, so we can keep SDK-owned local clear/reset semantics while
 * parallelizing the two independent revoke calls:
 *
 * 1. capture the current token/meta;
 * 2. set `client.tokens = null` before delegating to `client.logout()` so the
 *    SDK skips its serial revoke branch but still resets tokenReady/meta/store;
 * 3. best-effort revoke captured access/refresh tokens in parallel.
 */
async function logoutClientWithParallelRevoke(
  client: LogoutClient,
  signal: AbortSignal | undefined,
  deps: LogoutDeps,
): Promise<void> {
  try {
    await client.disconnect()
  } catch {
    // WS 可能未连; ignore
  }

  const tokens = client.getTokenSet()
  if (!tokens) {
    await client.logout(signal)
    return
  }

  const meta = client.meta
  client.tokens = null

  await Promise.all([
    client.logout(signal),
    revokeCapturedTokens(client.serverURL, tokens, meta, signal, deps),
  ])
}

async function revokeCapturedTokens(
  serverURL: string,
  tokens: TokenSet,
  meta: ServerMetadata | null,
  signal: AbortSignal | undefined,
  deps: LogoutDeps,
): Promise<void> {
  let revokeMeta = meta
  if (!revokeMeta) {
    try {
      revokeMeta = await deps.discover(tokens.server_url || serverURL, signal)
    } catch (error) {
      console.warn(
        `[acosmi-client] warning: discover for revocation failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      )
      return
    }
  }

  await Promise.allSettled([
    deps.revokeToken(revokeMeta, tokens.access_token, signal),
    deps.revokeToken(revokeMeta, tokens.refresh_token, signal),
  ])
}

export async function _logoutClientWithParallelRevokeForTesting(
  client: LogoutClient,
  deps: LogoutDeps,
  signal?: AbortSignal,
): Promise<void> {
  await logoutClientWithParallelRevoke(client, signal, deps)
}

// ---------------------------------------------------------------------------
// getAuthStatus — 形态对齐 BridgeAuthStatus (src/types/auth.ts).
// SDK TokenSet 是 snake_case + scope 单 string + expires_at ISO 8601;
// BridgeOAuthTokens 是 camelCase + scopes string[] + expiresAt unix ms; 此处映射.
// 结构等价 = utils/auth.ts setBridgeAuthCache 直接接受 AuthStatus 结构.
// ---------------------------------------------------------------------------

export interface AuthStatusTokens {
  accessToken: string
  refreshToken?: string
  expiresAt?: number
  scopes: string[]
  clientId?: string
  serverUrl?: string
}

export interface AuthStatus {
  authorized: boolean
  tokens: AuthStatusTokens | null
  hasProfileScope: boolean
  isSubscriber: boolean
}

/**
 * Recover the effective scopes for one completed OAuth transaction.
 *
 * RFC 6749 §5.1 permits a token response to omit `scope` when the granted
 * scopes are identical to the request. SDK 2.15.0 represents that omission as
 * an empty TokenSet scope string, so getAuthStatus() projects an empty array.
 * The direct login adapters still own the exact scopes they requested and may
 * restore only that known value; a non-empty server response remains
 * authoritative.
 */
export function resolveOAuthCompletionScopes(
  tokenScopes: readonly string[],
  requestedScopes: readonly string[],
): string[] {
  return tokenScopes.length > 0 ? [...tokenScopes] : [...requestedScopes]
}

export async function getAuthStatus(): Promise<AuthStatus | null> {
  const client = await getAcosmiClient()
  const authorized = client.isAuthorized()
  const tokenSet = client.getTokenSet()

  if (!tokenSet) {
    return {
      authorized,
      tokens: null,
      hasProfileScope: false,
      isSubscriber: false,
    }
  }

  const scopes = (tokenSet.scope ?? '').split(/\s+/).filter(Boolean)
  const expiresAtMs = parseExpiresAt(tokenSet.expires_at)

  return {
    authorized,
    tokens: {
      accessToken: tokenSet.access_token,
      refreshToken: tokenSet.refresh_token || undefined,
      expiresAt: expiresAtMs,
      scopes,
      clientId: tokenSet.client_id || undefined,
      serverUrl: tokenSet.server_url || undefined,
    },
    hasProfileScope: scopes.includes('account'),
    // SDK Client 暂不返回 entitlement 级 isSubscriber; Go hub 端是查 quota summary 推算.
    // 暂返 false; utils/auth.ts isAcosmiSubscriber 仍可由其他来源 (oauthAccount.billingType) 判断.
    isSubscriber: false,
  }
}

function parseExpiresAt(iso: string | undefined): number | undefined {
  if (!iso) return undefined
  const ms = Date.parse(iso)
  return Number.isFinite(ms) ? ms : undefined
}

// ---------------------------------------------------------------------------
// Chat — Y 路径 step 9 (2026-05-01): SDK Client 直调替代 hub chat.* 2 RPC.
// chatComplete / chatStreamAdapter / systemToString 共享基础设施;
// AcosmiResponse 已与 BetaMessage 形态对齐 — caller 端类型断言即可消费.
// ---------------------------------------------------------------------------

/**
 * PR-2 (2026-07-01 audit finding 2) boundary invariant: a `custom:`/`local:`
 * model reference must NEVER be sent to the Acosmi gateway — those requests
 * carry user conversation content destined for a user-owned endpoint. This is
 * the single-definition backstop covering every present and future
 * `chatComplete`/`chatStreamAdapter` call site (queryModel fallback,
 * verifyApiKey, sideQuery, tokenEstimation, …); dedicated call-site guards
 * degrade gracefully BEFORE reaching this hard wall.
 */
function assertNotNonGatewayModelReference(
  modelID: string,
  source: string,
): void {
  if (isNonGatewayModelReference(modelID)) {
    throw new Error(
      `${source}: refusing to send non-gateway model reference "${modelID}" to the Acosmi gateway — such requests must go through their dedicated adapters`,
    )
  }
}

/**
 * Exact catalog destination approved before a privacy-sensitive model call.
 *
 * `provider` is deliberately carried separately from `modelId`: the gateway
 * request URL contains only the model id, while the SDK catalog is the source
 * of truth for the provider route selected for that id. Callers must never
 * infer or case-fold either value.
 */
export type ExactChatDestination = Readonly<{
  provider: string
  modelId: string
  /**
   * Generic media-sidecar calls bind the recipient to the provider of this
   * exact live main model. Desktop visual calls already have an explicit user
   * destination consent and may omit it.
   *
   * 2026-07-27: a chat sidecar routing ACROSS providers omits it on the same
   * grounds — it only ever routes cross-provider when an exact user-approved
   * `(provider, modelId)` binding exists, which is the very condition this
   * field stands in for. Same-provider chat sidecar calls still set it.
   */
  mainModelId?: string
  /**
   * Opt out of the `locked` half of the eligibility check — and ONLY that half.
   *
   * `locked` is an ENTITLEMENT signal, not a privacy one: the property this
   * guard exists to enforce is "pixels reach exactly the approved provider and
   * model", and that is carried by the provider/model equality checks, which
   * this flag never touches. 2026-07-27 production forensics established that
   * the gateway sets `locked` on models the account can in fact call (see
   * `entitlements/gatewayLock.ts`), so for an account above the membership
   * floor the flag would veto a destination the user explicitly approved and
   * the server would have served. A wrong guess here costs a 402, not a leak.
   *
   * Absent/false keeps the pre-2026-07-27 behaviour. Only the chat media
   * sidecar sets it, and only after resolving the same membership predicate.
   */
  allowEntitlementLockedRoute?: boolean
}>

/**
 * Synchronous, side-effect-free authorization check invoked at the last
 * possible boundary before the one provider POST. It must re-read canonical
 * consent/policy state; an earlier boolean snapshot is not sufficient.
 */
export type ProviderBoundEgressValidator = () => void

type ChatCompletionClient = Pick<
  Client,
  | 'apiURL'
  | 'buildChatRequest'
  | 'chatMessages'
  | 'doRequest'
  | 'ensureToken'
  | 'isAuthorized'
  | 'listModels'
  | 'withRequestTimeout'
>

/** Test-only seam; production always uses the process-wide SDK client. */
export type ChatCompleteDeps = {
  getClient?: () => Promise<ChatCompletionClient>
}

/**
 * Fail-closed exact-route check against the catalog snapshot that the SDK has
 * just installed into its own model cache. The SDK's subsequent adapter lookup
 * uses the first row matching `id` or `modelId`, so this helper intentionally
 * applies that same lookup rule rather than guessing from another cache.
 */
export function assertExactChatDestination(
  runtimeModelID: string,
  destination: ExactChatDestination,
  catalog: readonly ManagedModel[],
): ManagedModel {
  if (
    destination.modelId.length === 0 ||
    destination.modelId.trim() !== destination.modelId
  ) {
    throw new Error(
      'acosmi.chatComplete: approved destination has an invalid model id',
    )
  }
  if (
    destination.provider.length === 0 ||
    destination.provider.trim() !== destination.provider
  ) {
    throw new Error(
      'acosmi.chatComplete: approved destination has an unknown provider',
    )
  }
  if (runtimeModelID !== destination.modelId) {
    throw new Error(
      `acosmi.chatComplete: runtime model does not match the approved destination (${runtimeModelID} != ${destination.modelId})`,
    )
  }

  const liveRoute = catalog.find(
    model => model.id === runtimeModelID || model.modelId === runtimeModelID,
  )
  if (!liveRoute) {
    throw new Error(
      `acosmi.chatComplete: approved model ${destination.modelId} is absent from the live catalog`,
    )
  }
  const lockedIsDisqualifying = destination.allowEntitlementLockedRoute !== true
  if (
    liveRoute.isEnabled === false ||
    (lockedIsDisqualifying && liveRoute.locked === true) ||
    liveRoute.chatRuntimeSupported === false
  ) {
    throw new Error(
      `acosmi.chatComplete: approved model ${destination.modelId} is not currently chat-eligible`,
    )
  }
  if (liveRoute.modelId !== destination.modelId) {
    throw new Error(
      `acosmi.chatComplete: live catalog model route changed (${liveRoute.modelId} != ${destination.modelId})`,
    )
  }

  const liveProvider = liveRoute.provider?.trim() ?? ''
  if (liveProvider.length === 0) {
    throw new Error(
      `acosmi.chatComplete: live catalog provider is unknown for ${destination.modelId}`,
    )
  }
  if (liveProvider !== destination.provider) {
    throw new Error(
      `acosmi.chatComplete: live catalog provider changed for ${destination.modelId} (${liveProvider} != ${destination.provider})`,
    )
  }
  if (destination.mainModelId !== undefined) {
    if (
      destination.mainModelId.length === 0 ||
      destination.mainModelId.trim() !== destination.mainModelId
    ) {
      throw new Error(
        'acosmi.chatComplete: approved destination has an invalid main model id',
      )
    }
    const liveMainRoute = catalog.find(
      model => model.modelId === destination.mainModelId,
    )
    if (!liveMainRoute) {
      throw new Error(
        `acosmi.chatComplete: main model ${destination.mainModelId} is absent from the live catalog`,
      )
    }
    if (
      liveMainRoute.isEnabled === false ||
      (lockedIsDisqualifying && liveMainRoute.locked === true) ||
      liveMainRoute.chatRuntimeSupported === false
    ) {
      throw new Error(
        `acosmi.chatComplete: main model ${destination.mainModelId} is not currently chat-eligible`,
      )
    }
    const liveMainProvider = liveMainRoute.provider?.trim() ?? ''
    if (
      liveMainProvider.length === 0 ||
      liveMainProvider !== destination.provider
    ) {
      throw new Error(
        `acosmi.chatComplete: main model provider changed for ${destination.mainModelId}`,
      )
    }
  }
  return liveRoute
}

async function providerBoundChatComplete(
  client: ChatCompletionClient,
  runtimeModelID: string,
  req: ChatRequest,
  signal: AbortSignal | undefined,
  destination: ExactChatDestination,
  validateEgress: ProviderBoundEgressValidator,
): Promise<AcosmiResponse> {
  // Build locally first. This may warm the model catalog but does not send the
  // request body (and therefore cannot send pixels) to a provider.
  const prepared = await client.buildChatRequest(
    runtimeModelID,
    { ...req, stream: false },
    signal,
  )
  // Refresh the SDK's catalog without pixels. listModels installs the returned
  // rows into the same SDK cache used for adapter routing.
  const catalog = await client.listModels(signal)
  // Capture the post-catalog token (listModels itself may have refreshed a
  // rejected token). A 401 after this point is surfaced as failure and is
  // never replayed with pixels.
  const token = await client.ensureToken(signal)
  // Everything below is synchronous until the single provider fetch starts,
  // so this is the exact catalog check immediately before pixel egress.
  const liveRoute = assertExactChatDestination(
    runtimeModelID,
    destination,
    catalog,
  )
  const liveAdapter = getAdapterForModel(liveRoute)
  if (
    liveAdapter.endpointSuffix() !== prepared.adapter.endpointSuffix()
  ) {
    throw new Error(
      `acosmi.chatComplete: live catalog request format changed for ${destination.modelId}`,
    )
  }

  const timeout = client.withRequestTimeout(
    PROVIDER_BOUND_CHAT_TIMEOUT_MS,
    signal,
  )
  try {
    // buildChatRequest/listModels/ensureToken above all await. Consent, policy,
    // or a durable audit capability may have been revoked while any of them
    // was pending, so the owner-provided canonical validator belongs here —
    // synchronously adjacent to the sole POST, after every asynchronous prep.
    validateEgress()
    // Intentionally use the SDK's one-shot transport rather than
    // `chatMessages`: chatMessages' internal 401 handler can replay a POST,
    // which would make one audit/consent transaction fan out into two pixel
    // egress attempts. doRequest performs exactly one fetch and still
    // classifies transport failures as SDK NetworkError.
    const response = await client.doRequest(
      {
        method: 'POST',
        url: client.apiURL(
          `/managed-models/${encodeURIComponent(runtimeModelID)}${liveAdapter.endpointSuffix()}`,
        ),
        headers: {
          Authorization: `Bearer ${token}`,
          'Content-Type': 'application/json',
        },
        body: prepared.body,
      },
      timeout.signal,
    )
    if (!response.ok) {
      try {
        await response.body?.cancel()
      } catch {
        // Best effort only; the response body contains no useful sidecar data.
      }
      throw new Error(
        `acosmi.chatComplete: provider-bound request failed with HTTP ${response.status}; request was not replayed`,
      )
    }
    const bytes = new Uint8Array(await response.arrayBuffer())
    return liveAdapter.parseResponse(bytes) as unknown as AcosmiResponse
  } finally {
    timeout.dispose()
  }
}

/**
 * SDK Client.chatMessages — 同步聊天, 返回 SDK 原生格式响应.
 * AcosmiResponse {id, type, role, content, model, stop_reason, usage}
 * 与 Acosmi.Beta.Messages.BetaMessage 字段一致, caller 可直接断言消费.
 */
export async function chatComplete(
  modelID: string,
  req: ChatRequest,
  signal?: AbortSignal,
  exactDestination?: ExactChatDestination,
  validateEgress?: ProviderBoundEgressValidator,
  deps: ChatCompleteDeps = {},
): Promise<AcosmiResponse> {
  assertNotNonGatewayModelReference(modelID, 'acosmi.chatComplete')
  const runtimeModelID = assertRuntimeModel(modelID, {
    source: 'acosmi.chatComplete',
  })
  if (exactDestination && !validateEgress) {
    throw new Error(
      'acosmi.chatComplete: provider-bound egress requires a final canonical validator',
    )
  }
  const client = await (deps.getClient ?? getAcosmiClient)()
  // 取证 2026-05-05 disk-cache 写盘 bug: 标记 chat 路径触发, 与 refreshModelCapabilities
  // 的 [modelCapabilities] log 对照, 看是否 chat 走的也是 listModels (经
  // ensureModelCached) 还是别的端点.
  void import('../../utils/debug.js').then(m => m.logForDebugging(
    `[acosmi-client] chatComplete enter modelID=${runtimeModelID} sdkAuthorized=${client.isAuthorized()}`,
  )).catch(() => {})
  if (exactDestination) {
    return providerBoundChatComplete(
      client,
      runtimeModelID,
      req,
      signal,
      exactDestination,
      validateEgress!,
    )
  }
  return client.chatMessages(runtimeModelID, req, signal)
}

/**
 * SDK Client.chatMessagesStream — 流式聊天 (SSE async iterable).
 * SDK 返回 raw SSE event {event, data}; 此处通过 SDK 2.15 的四态分类器
 * 归一成 NormalizedAcosmiChatStreamEvent，并明确丢弃合法零来源和损坏的
 * 可选来源事件。同时提供 .controller
 * (AbortController) 兼容 queryModel.ts:1313 的 'controller' in stream 检查.
 */
export interface ChatStreamAdapter {
  controller: AbortController
  [Symbol.asyncIterator](): AsyncIterator<unknown>
}

export function chatStreamAdapter(
  modelID: string,
  req: ChatRequest,
): ChatStreamAdapter {
  assertNotNonGatewayModelReference(modelID, 'acosmi.chatStreamAdapter')
  const runtimeModelID = assertRuntimeModel(modelID, {
    source: 'acosmi.chatStreamAdapter',
  })
  const controller = new AbortController()
  return {
    controller,
    async *[Symbol.asyncIterator]() {
      const client = await getAcosmiClient()
      // 取证 2026-05-05 disk-cache 写盘 bug: 与 refreshModelCapabilities log 对照.
      void import('../../utils/debug.js').then(m => m.logForDebugging(
        `[acosmi-client] chatStream enter modelID=${runtimeModelID} sdkAuthorized=${client.isAuthorized()}`,
      )).catch(() => {})
      try {
        const stream = client.chatMessagesStream(runtimeModelID, req, controller.signal)
        for await (const evt of stream) {
          if (controller.signal.aborted) break
          const parsed = normalizeAcosmiChatStreamEvent(evt)
          if (parsed !== undefined) yield parsed
        }
      } catch (err) {
        if (!controller.signal.aborted) throw err
      }
    },
  }
}

/**
 * Preserve ordinary JSON payloads while routing every candidate sources event
 * through the SDK's authoritative four-state classifier. Empty sources are a
 * presentation no-op. Malformed sources are bounded compatibility faults and
 * never reach StructuredIO. Non-sources invalid JSON stops only the turn.
 *
 * This is an application-internal adapter. It does not add a backend, public
 * SDK, or control-protocol event.
 *
 * @internal
 */
export function normalizeAcosmiChatStreamEvent(
  evt: StreamEvent,
): NormalizedAcosmiChatStreamEvent | undefined {
  if (!evt.data || evt.data.trim() === '[DONE]') return undefined

  const sources = classifySourcesEvent(evt)
  switch (sources.kind) {
    case 'empty_sources':
      recordSourcesCompatibilityEvent('sources_empty_noop', evt.data)
      return undefined
    case 'malformed_sources':
      recordSourcesCompatibilityEvent(sources.code, evt.data)
      return undefined
    case 'sources':
      return {
        ...sources.value,
        type: 'sources',
      }
    case 'not_sources':
      break
  }

  try {
    return JSON.parse(evt.data) as NormalizedAcosmiChatStreamEvent
  } catch {
    throw new AcosmiStreamDecodeError(evt.event)
  }
}

export class AcosmiStreamDecodeError extends Error {
  readonly code = 'stream_event_invalid_json'

  readonly eventNameEncodedLength: number

  constructor(eventName: string) {
    super('Acosmi stream event JSON is invalid')
    this.name = 'AcosmiStreamDecodeError'
    this.eventNameEncodedLength = new TextEncoder().encode(eventName).byteLength
  }
}

const sourcesCompatibilityCounts = new Map<string, number>()

function recordSourcesCompatibilityEvent(code: string, data: string): void {
  const count = (sourcesCompatibilityCounts.get(code) ?? 0) + 1
  sourcesCompatibilityCounts.set(code, count)
  if (count !== 1) return
  console.warn(
    `[stream-compat] ${JSON.stringify({
      event_type: 'sources',
      issue_code: code,
      encoded_len: new TextEncoder().encode(data).byteLength,
      suppressed_count: 1,
    })}`,
  )
}

/** @internal test-only reset for the per-process/turn diagnostic limiter. */
export function _resetAcosmiStreamDiagnosticsForTesting(): void {
  sourcesCompatibilityCounts.clear()
}

/**
 * Convert system prompt from SDK block format to a plain string.
 * SDK ChatRequest.system 接受 string|ContentBlock[], 但 caller 端有时
 * 需要先用此 utility 把 block 列表 collapse 成 string (向后兼容 IPC 端
 * gateway.MessageRequest 的 system: string 形态).
 */
export function systemToString(system: unknown): string | undefined {
  if (typeof system === 'string') return system
  if (Array.isArray(system)) {
    return (
      system
        .filter((b: { type?: string }) => b?.type === 'text')
        .map((b: { text?: string }) => b.text ?? '')
        .join('\n\n') || undefined
    )
  }
  return undefined
}
