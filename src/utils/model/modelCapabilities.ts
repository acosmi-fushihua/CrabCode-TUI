import { createHash } from 'crypto'
import { existsSync, readFileSync, statSync } from 'fs'
import { mkdir, unlink, writeFile } from 'fs/promises'
import isEqual from 'lodash-es/isEqual.js'
import memoize from 'lodash-es/memoize.js'
import { join } from 'path'
import { z } from 'zod/v4'
import { getAcosmiClient } from '../../services/acosmi/client.js'
import { getOauthConfig } from '../../constants/oauth.js'
import { getMembershipGateInput } from '../auth.js'
import { logForDebugging } from '../debug.js'
import { getGlobalConfig } from '../config.js'
import { shouldEnforceGatewayLockedFlag } from '../entitlements/gatewayLock.js'
import { getCrabCodeConfigHomeDir } from '../envUtils.js'
import { safeParseJSON } from '../json.js'
import { lazySchema } from '../lazySchema.js'
import { isEssentialTrafficOnly } from '../privacyLevel.js'
import { jsonStringify } from '../slowOperations.js'
import { isAnthropicCompatibleManagedModel } from './modelFormat.js'
import { getAPIProvider, isFirstPartyAcosmiBaseUrl } from './providers.js'

// .strip() — don't persist internal-only fields (mycro_deployments etc.) to disk
//
// 字段语义 (2026-05-05 UUID→slug 切换):
//   - id:      slug, 例 "deepseek-v4-flash" — 网关 entitlement bucket 索引 key,
//              chat URL 拼接的就是这个; 是 TS 客户端贯穿全栈的稳定标识符
//   - uuid:    UUID, 例 "97a142ec-...", 仅用于旧 settings 反查迁移
//   - bucketInfo: 网关在 listModels 时聚合好的"该模型总额/剩余", picker 直读
//              不再需要自己调 getQuotaSummary 累加 buckets
const ModelCapabilitySchema = lazySchema(() =>
  z
    .object({
      id: z.string(),
      uuid: z.string().optional(),
      name: z.string().optional(),
      // Exact provider identity returned by the SDK ManagedModel catalog.
      // Optional only for backwards compatibility with caches written before
      // this field was preserved; callers must never infer a provider when it
      // is absent.
      provider: z.string().optional(),
      max_input_tokens: z.number().optional(),
      max_tokens: z.number().optional(),
      pricePerMTok: z.number().optional(),
      isDefault: z.boolean().optional(),
      isEnabled: z.boolean().optional(),
      // SDK ManagedModel 已声明这四个权益
      // 字段（@acosmi/sdk-ts index-BrbgHn0E.d.ts:135/137/141/143），但旧 schema
      // .strip() 未声明 → safeParse 丢弃。声明后 picker 才能分「免费区 / 付费订阅区」
      // 并对 locked 模型置灰。全 optional：网关对免费账号默认过滤掉 locked 行（不下发），
      // 仅 includeLocked 全集模式才返 locked:true 行；absence 视为「未知/不锁」，调用方
      // 不得 branch on 缺失 vs false。§硬约束 #1：信号全来自 SDK，无品牌字面。
      locked: z.boolean().optional(),
      freeTier: z.boolean().optional(),
      minPlanTier: z.number().optional(),
      chatRuntimeSupported: z.boolean().optional(),
      // Upstream identity for managed-model gateway. Optional because acosmi
      // ListModels currently does not surface these fields; callers must
      // tolerate `undefined` and not branch on absence vs unknown. Once the
      // gateway populates them, ModelPicker / error UI can show "回源:
      // dashscope (DeepSeek-Pro)" so users can distinguish gateway 30s
      // timeouts from real model errors.
      upstream: z.string().optional(),
      upstreamEndpoint: z.string().optional(),
      bucketInfo: z
        .object({
          quotaEtu: z.number().optional(),
          usedEtu: z.number().optional(),
          remainingEtu: z.number().optional(),
          freeRemainingEtu: z.number().optional(),
          paidRemainingEtu: z.number().optional(),
          expiresAt: z.string().optional().nullable(),
          bucketClass: z.string().optional(),
        })
        .optional(),
      capabilities: z
        .object({
          supports_thinking: z.boolean().optional(),
          supports_adaptive_thinking: z.boolean().optional(),
          supports_isp: z.boolean().optional(),
          supports_web_search: z.boolean().optional(),
          supports_tool_search: z.boolean().optional(),
          supports_structured_output: z.boolean().optional(),
          supports_effort: z.boolean().optional(),
          supports_max_effort: z.boolean().optional(),
          supports_fast_mode: z.boolean().optional(),
          supports_1m_context: z.boolean().optional(),
          supports_prompt_cache: z.boolean().optional(),
          supports_cache_editing: z.boolean().optional(),
          supports_token_efficient: z.boolean().optional(),
          supports_redact_thinking: z.boolean().optional(),
          supports_auto_mode: z.boolean().optional(), // v0.3.0
          max_input_tokens: z.number().optional(),
          max_output_tokens: z.number().optional(),
          // W-DESKTOP-AUTOMATION-PHASE3 (2026-05-18). Set by `@acosmi/
          // sdk-ts` ≥ 1.2.x catalog; SDK 1.0.2 (locally installed)
          // does not surface this yet — field stays optional/undefined
          // and the visual sidecar selector falls back to semantic
          // snapshot. Once SDK upgrade lands, this preserves the
          // catalog signal so `findModelByCapability` can filter for
          // "this model can drive screen-aware desktop automation".
          // Plan §7 selection chain.
          // `@acosmi/sdk-ts` ≥ 2.2.1 (v1.3+
          // catalog) surfaces these on `ModelCapabilities`: a model with
          // `supports_image_generation=true` is an image-generation model
          // (called via `client.generateImage()`, billed per image) and
          // `supports_video_generation=true` a video-generation model
          // (called via `client.generateVideo()` + `pollVideoTask()`, billed
          // per duration). The `capabilities` z.object has NO `.passthrough()`,
          // so undeclared keys are stripped on `safeParse` — these two MUST be
          // declared here or the wholesale `mapped.capabilities = entry.
          // capabilities` copy drops them and the media-generation model
          // selectors read nothing. Catalog-driven, with no hardcoded ids
          // (no hardcoded model ids). Schema-only change — the mapper already
          // copies `capabilities` wholesale so no per-field mapping is added
          // and no invented defaults.
          supports_image_generation: z.boolean().optional(),
          supports_video_generation: z.boolean().optional(),
        })
        .optional(),
      // Mirrors the protocol-layer input-modality string set
      // string set ("text" / "image" / "audio" / ...). Used by the
      // visual sidecar selector + per-call image-block filtering in
      // selectors. Empty array (or absent) means "text only".
      inputModalities: z.array(z.string()).optional(),
      // SDK
      // `ManagedModel.supported_formats` / `preferred_format` — the
      // request-format set the gateway enables for this managed model
      // (values "anthropic" | "openai"). The capability schema used to
      // `.strip()` these, dropping format metadata before the disk
      // cache; persisting them lets `isAnthropicCompatibleManagedModel`
      // (modelFormat.ts) filter OpenAI-only managed models out of the
      // TUI picker (the managed runtime path is Anthropic-compatible).
      // Both optional: SDK 1.0.2 does not always populate them — absence
      // is treated as legacy upstream (conservatively kept). Per
      // The mapper only writes these keys when the
      // source value is present.
      supported_formats: z.array(z.string()).optional(),
      preferred_format: z.string().optional(),
    })
    .strip(),
)

// V116.1 P1-2 (2026-07-24) 磁盘缓存 schema v2 —— 免费用户准入事故修复:
//   - schemaVersion: 2 字面量;旧 v1 信封 {models,timestamp} 读取时整份弃用
//     (触发重刷,与 2026-05-05 UUID→slug 迁移同模式)。
//   - principalHash: sha256(userUuid|serverUrl|organizationUuid) —— 主体切换
//     (换号/换服)后旧缓存不匹配即弃用,防跨用户目录污染驱动本地预检。
//   - filterStatus: 网关 X-Entitlement-Filter-Status;仅 ok 族(权益过滤已
//     生效)才允许落盘;fallback-*/disabled-by-flag 等降级态只存内存会话态。
//   - fetchedAt: 超 24h 视为过期 —— 过期或降级态缓存不得驱动本地预检拦截
//     (isLockedGatewayModel 仅在新鲜权威缓存上启用,否则预检放行交服务端判定)。
const CacheFileSchemaV2 = lazySchema(() =>
  z.object({
    schemaVersion: z.literal(2),
    principalHash: z.string(),
    filterStatus: z.string(),
    fetchedAt: z.number(),
    models: z.array(ModelCapabilitySchema()),
  }),
)

export type ModelCapability = z.infer<ReturnType<typeof ModelCapabilitySchema>>

/** 权益过滤已生效的 status 族(SDK FilterStatus;'' = 老网关未下发,不可证)。 */
export const AUTHORITATIVE_FILTER_STATUSES: readonly string[] = [
  'ok',
  'admin-bypass',
  'internal-bypass',
]

export function isAuthoritativeFilterStatus(status: string): boolean {
  return AUTHORITATIVE_FILTER_STATUSES.includes(status)
}

/** 权威缓存驱动本地预检的新鲜度上限(24h)。 */
const CACHE_FRESH_TTL_MS = 24 * 60 * 60 * 1000

// 模型集未变时也要周期性刷新盘上 fetchedAt(否则长驻会话 24h 后预检静默失效),
// 但绝不能每次刷新都写盘 —— 写盘触发 settings/OSC watchers → React 链回环
//（避免短周期写盘循环）。6h 重写间隔 = 每天至多
// 4 次写盘,远低于任何回环阈值,又保证盘上新鲜度始终 < 24h TTL。
const DISK_REWRITE_MIN_INTERVAL_MS = 6 * 60 * 60 * 1000

/**
 * 当前主体指纹。serverUrl 取 OAuth 根host(BASE_API_URL —— 与 profile/登录同源,
 * 网关 serverURL override 仅存在于 per-turn worker 进程,而目录刷新只发生在
 * TUI 主进程);未登录时各分量为空串,与任何已登录时写下的缓存天然不匹配。
 */
function currentPrincipalHash(): string {
  const account = getGlobalConfig().oauthAccount
  const input = `${account?.accountUuid ?? ''}|${getOauthConfig().BASE_API_URL}|${account?.organizationUuid ?? ''}`
  return createHash('sha256').update(input).digest('hex')
}

/**
 * 当前主体指纹(与磁盘 v2 信封 principalHash 同算法)。导出供测试播种缓存
 * 与诊断使用;生产消费方不需要直接调用(getCatalogState 内部已校验)。
 */
export function getCurrentCatalogPrincipalHash(): string {
  return currentPrincipalHash()
}

/** 目录状态:磁盘 v2 信封与内存会话态的公共形态。 */
type CatalogState = {
  models: ModelCapability[]
  filterStatus: string
  fetchedAt: number
  principalHash: string
}

// 降级态目录的内存会话载体(不落盘);logout 时随磁盘缓存一并清空。
let _sessionCatalog: CatalogState | null = null
// 会话态归属的缓存路径 —— 内存层必须与磁盘层同等隔离:磁盘缓存天然按
// config home 分文件, 内存态若只认 principalHash, 同账号切 config home
// (CRABCODE_CONFIG_DIR / 多 profile) 时会把上一目录的目录集喂给新目录。
let _sessionCatalogPath: string | null = null

function getCacheDir(): string {
  return join(getCrabCodeConfigHomeDir(), 'cache')
}

function getCachePath(): string {
  return join(getCacheDir(), 'model-capabilities.json')
}

function isModelCapabilitiesEligible(): boolean {
  if (getAPIProvider() === 'acosmi') return true
  if (getAPIProvider() !== 'firstParty') return false
  if (!isFirstPartyAcosmiBaseUrl()) return false
  return true
}

// Longest-id-first so substring match prefers most specific; secondary key for stable isEqual
function sortForMatching(models: ModelCapability[]): ModelCapability[] {
  return [...models].sort(
    (a, b) => b.id.length - a.id.length || a.id.localeCompare(b.id),
  )
}

// 网关 listModels 对同一 model 按 anthropic/openai 双格式各返一条，落盘后磁盘
// 缓存出现重复 slug。其中 openai 条目的能力字段几乎全 false，排在前面时
// getModelCapability 的 .find() 会首条命中它 → 污染思考档/effort/上下文窗口
// 解析。本函数按 id 去重，每个 id 只保留一条代表：
//   - 同 id 多条时优先保留 isAnthropicCompatibleManagedModel 为 true 的条目；
//   - 都不兼容 / 都兼容时保留首次出现的条目（稳定 tie-break，与网关返回顺序
//     无关 —— 同输入必同输出）。
// 返回顺序保持每个 id 首次出现的顺序，不引入 {x: undefined} own-key（避免触发
// 防止 lodash isEqual 写盘循环）。
// export 仅为单测可直接覆盖去重规则；运行时唯一调用点是 _refreshModelCapabilitiesImpl。
export function dedupeModelsById(
  models: ModelCapability[],
): ModelCapability[] {
  const order: string[] = []
  const byId = new Map<string, ModelCapability>()
  for (const m of models) {
    const key = m.id.toLowerCase()
    const existing = byId.get(key)
    if (existing === undefined) {
      order.push(key)
      byId.set(key, m)
      continue
    }
    // 已有同 id 条目：仅当当前条目 Anthropic 兼容、而已留条目不兼容时替换。
    // 其它情况（已留条目已兼容 / 两者都不兼容）保留首次出现的条目。
    if (
      isAnthropicCompatibleManagedModel(m) &&
      !isAnthropicCompatibleManagedModel(existing)
    ) {
      byId.set(key, m)
    }
  }
  return order.map(key => byId.get(key)!)
}

// 2026-05-05 UUID→slug 切换: 旧 disk cache 的 id 字段是 UUID, 新 schema 语义
// 是 slug, 直接使用会让 chat URL 拼到 UUID 撞 402。检测到任一 entry id 匹配
// UUID 正则即丢弃整份 cache, 触发 listModels 重刷。
const UUID_ID_REGEX_FOR_CACHE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

// Keyed on cache path so tests that set CRABCODE_CONFIG_DIR get a fresh read.
// V116.1 P1-2:返回完整 CatalogState(v2 信封);旧 v1 信封 {models,timestamp}
// 无 schemaVersion → safeParse 失败 → 整份弃用触发重刷(升级迁移语义)。
// principalHash 匹配不在此做(memoize 会冻结判定),在 getCatalogState 每次读时判。
const loadCache = memoize(
  (path: string): CatalogState | null => {
    try {
      // eslint-disable-next-line custom-rules/no-sync-fs -- memoized; called from sync getContextWindowForModel
      const raw = readFileSync(path, 'utf-8')
      const parsed = CacheFileSchemaV2().safeParse(safeParseJSON(raw, false))
      if (!parsed.success) {
        logForDebugging(
          '[modelCapabilities] disk cache is not schema v2 (legacy or corrupt); discarding',
        )
        return null
      }
      const models = parsed.data.models
      // 旧 UUID-shaped id 直接弃用整份 cache
      if (models.some(m => UUID_ID_REGEX_FOR_CACHE.test(m.id))) {
        logForDebugging(
          '[modelCapabilities] disk cache contains UUID-shaped ids (pre UUID→slug migration); discarding',
        )
        return null
      }
      return {
        models,
        filterStatus: parsed.data.filterStatus,
        fetchedAt: parsed.data.fetchedAt,
        principalHash: parsed.data.principalHash,
      }
    } catch {
      return null
    }
  },
  path => path,
)

/**
 * 统一目录读取:内存会话态(降级刷新也更新)优先,磁盘权威缓存兜底;
 * 两者都要求 principalHash 与当前主体一致 —— 不一致即视为不存在
 * (跨主体缓存绝不得被消费,P1-2 验收:主体切换缓存隔离)。
 */
function getCatalogState(): CatalogState | null {
  const principal = currentPrincipalHash()
  const cachePath = getCachePath()
  if (
    _sessionCatalog !== null &&
    _sessionCatalog.principalHash === principal &&
    _sessionCatalogPath === cachePath
  ) {
    return _sessionCatalog
  }
  const disk = loadCache(cachePath)
  if (disk !== null && disk.principalHash === principal) {
    return disk
  }
  return null
}

/** 消费方通用取模型集(等价旧 loadCache(getCachePath()) 的语义)。 */
function getCatalogModels(): ModelCapability[] | null {
  return getCatalogState()?.models ?? null
}

/**
 * Derives a human-readable display name from a raw model ID string by
 * matching the conventional `<family>-<major>(-<minor>)?` token at the start
 * of the id. Used as a last-resort label when the SDK catalog has no entry
 * for the given id; SDK names take precedence wherever available.
 */
function deriveDisplayNameFromId(modelId: string): string {
  // Strip provider prefixes (us.acosmi., etc.) and date suffixes
  const base = modelId
    .replace(/^us\.acosmi\./, '')
    .replace(/@\d+$/, '')
    .replace(/-v\d+:\d+$/, '')
    .replace(/-\d{8}$/, '')
  const match = base.match(/(\w+)-(\d+)(?:-(\d+))?/)
  if (match) {
    const family = match[1]!.charAt(0).toUpperCase() + match[1]!.slice(1)
    const version = match[3] ? `${match[2]}.${match[3]}` : match[2]!
    return `${family} ${version}`
  }
  return modelId
}

/**
 * Returns the display name for a model from the SDK cache.
 * Falls back to deriving a name from the model ID.
 */
export function getCachedModelDisplayName(model: string): string {
  if (isModelCapabilitiesEligible()) {
    const cached = getCatalogModels()
    if (cached && cached.length > 0) {
      const m = model.toLowerCase()
      const exact = cached.find(c => c.id.toLowerCase() === m)
      if (exact?.name) return exact.name
      const partial = cached.find(c => m.includes(c.id.toLowerCase()))
      if (partial?.name) return partial.name
    }
  }
  return deriveDisplayNameFromId(model)
}

export function getModelCapability(model: string): ModelCapability | undefined {
  if (!isModelCapabilitiesEligible()) return undefined
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return undefined
  // Defense in depth: even if the disk cache still contains old duplicate
  // anthropic/openai rows for the same slug,
  // first apply isAnthropicCompatibleManagedModel, then
  // 匹配，保证两个 .find() 都命中 Anthropic 条目，而非能力全 false 的 openai
  // 条目。两个 .find() 必须都覆盖：精确匹配与子串回退同病。
  const eligible = cached.filter(isAnthropicCompatibleManagedModel)
  const m = model.toLowerCase()
  const exact = eligible.find(c => c.id.toLowerCase() === m)
  if (exact) return exact
  return eligible.find(c => m.includes(c.id.toLowerCase()))
}

/**
 * Return only an exact catalog capability match.
 *
 * Privacy-sensitive routing must never use `getModelCapability()`'s historical
 * substring compatibility fallback: a custom/unknown id that merely contains
 * a managed slug could otherwise inherit its modality/provider and send media
 * to a destination the user did not select.
 */
export function getExactModelCapability(
  model: string,
): ModelCapability | undefined {
  if (!isModelCapabilitiesEligible()) return undefined
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return undefined
  const normalized = model.trim().toLowerCase()
  if (normalized.length === 0) return undefined
  return cached
    .filter(isAnthropicCompatibleManagedModel)
    .find(capability => capability.id.toLowerCase() === normalized)
}

/**
 * Returns the cached pricePerMTok for a given model from the SDK cache.
 * Returns undefined if the model is not found or has no pricing data.
 */
export function getCachedModelPricePerMTok(model: string): number | undefined {
  const cap = getModelCapability(model)
  return cap?.pricePerMTok
}

/**
 * Returns the default model ID from the SDK cache (IsDefault=true).
 * Returns undefined if no default model is found in cache.
 */
export function getCachedDefaultModelId(): string | undefined {
  if (!isModelCapabilitiesEligible()) return undefined
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return undefined
  // 默认模型必须排除 locked。取全集
  // （includeLocked）后 cache 会含 locked 行；默认模型若解析到 locked 模型，打 chat
  // 端点撞网关 402。当前不取全集时 locked 恒 undefined，此过滤为 no-op。
  const defaultModel = cached.find(
    c => c.isDefault === true && c.isEnabled !== false && c.locked !== true,
  )
  return defaultModel?.id
}

/**
 * Returns all enabled models from the SDK cache.
 * Returns an empty array if no cache is available.
 *
 * Also drops managed
 * models whose SDK request format is OpenAI-only — the managed runtime path
 * is Anthropic-compatible, so an OpenAI-only model is not usable from the
 * picker. Models with no declared format metadata (legacy upstream) are kept
 * conservatively. Filter is orthogonal to the `isEnabled` filter.
 */
export function getCachedEnabledModels(): ModelCapability[] {
  if (!isModelCapabilitiesEligible()) return []
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return []
  return cached
    .filter(c => c.isEnabled !== false)
    // 可用集排除 locked 模型。取全集
    // （includeLocked）后 locked 行只进 picker 展示（getCachedModels 保留），但「可用
    // 模型」语义（默认/swarm/findByCapability 等大量消费点）必须排除，否则把锁定模型
    // 当可用撞 402。当前不取全集时 locked 恒 undefined，此过滤为 no-op。
    .filter(c => c.locked !== true)
    .filter(isAnthropicCompatibleManagedModel)
}

/**
 * Non-mutating
 * diagnostic that explains WHY `getCachedEnabledModels()` is empty, so a live
 * `--debug` reproduction of "加自定义模型后网关模型消失" can be classified
 * WITHOUT touching any selection / merge logic. Mirrors the exact filter ladder
 * of `getCachedEnabledModels` and reports the first stage that emptied it:
 *
 *   - `ineligible`        → provider gate (`isModelCapabilitiesEligible` false)
 *   - `disk-cache-empty`  → the SDK model cache file is missing / empty. This is
 *                           the **transient** case the audit suspects (listModels
 *                           race / 本机 7890 代理 refresh failure). If the picker
 *                           shows only [Default] + custom models, this reason
 *                           confirms the gateway list was simply not loaded — the
 *                           custom models did NOT replace it (they are appended).
 *   - `all-filtered:…`    → the cache HAS entries but every one was dropped by the
 *                           enabled / locked / anthropic-compatible filters. This
 *                           would indicate an unexpected mechanism (audit branch
 *                           (b)) and the counts pinpoint which filter ate them.
 *   - `non-empty:N`       → not actually empty (N usable models) — caller should
 *                           not have reached the empty-cache fallback.
 *
 * Debug-only call site (gated by `isDebugMode()`); zero production cost.
 */
export function diagnoseEmptyEnabledModels(): string {
  if (!isModelCapabilitiesEligible()) return 'ineligible'
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return 'disk-cache-empty'
  const enabled = cached.filter(c => c.isEnabled !== false)
  const unlocked = enabled.filter(c => c.locked !== true)
  const usable = unlocked.filter(isAnthropicCompatibleManagedModel)
  if (usable.length > 0) return `non-empty:${usable.length}`
  return (
    `all-filtered:raw=${cached.length},` +
    `enabled=${enabled.length},unlocked=${unlocked.length},anthropicCompatible=0`
  )
}

/**
 * Resolves a persisted ManagedModel UUID to its slug (`ModelCapability.id`)
 * against the TRULY UNFILTERED disk cache.
 *
 * Chat URLs must use the slug; a bare UUID never indexes a
 * gateway entitlement bucket and hard-fails with HTTP 402.
 *
 * Why the unfiltered cache (not `getCachedEnabledModels` /
 * `getCachedModels({includeHidden:true})`): both of those apply
 * `isEnabled !== false` and/or `isAnthropicCompatibleManagedModel`
 * (OpenAI-only drop, modelCapabilities.ts), so a now-disabled or OpenAI-only
 * model's UUID would fail to resolve and the bare UUID would leak into the
 * chat URL. `loadCache(getCachePath())` only discards the whole cache when an
 * `id` is UUID-shaped (the §10 corruption guard) — it never filters by model
 * status — so a UUID can still be mapped back to its slug here.
 *
 * Returns the matched slug (`ModelCapability.id`), or `undefined` if no cache
 * entry carries that `.uuid`. Callers must fall back to a slug (e.g.
 * `getDefaultModel()`), never to the bare UUID.
 */
export function resolveUuidToSlugFromUnfilteredCache(
  uuid: string,
): string | undefined {
  if (!isModelCapabilitiesEligible()) return undefined
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return undefined
  const matched = cached.find(c => c.uuid === uuid)
  return matched?.id
}

/**
 * Controlled cache reader for off-process consumers (App Server `worker.model/list`).
 *
 * Why dedicated exporter instead of letting Rust read disk: model cache lives
 * at `<getCrabCodeConfigHomeDir()>/cache/model-capabilities.json` and the
 * read pipeline (loadCache + UUID-shaped id failsafe + lowercase matching
 * conventions) is owned by this module. Rust must not bypass; F4-1-B doc
 * §F4-1-B explicitly forbids "Rust 端为支持 includeHidden 直读
 * model-capabilities.json 路径".
 *
 * Semantics:
 *   - includeHidden=false (default): same filter as `getCachedEnabledModels`,
 *     drops `isEnabled === false` entries.
 *   - includeHidden=true: returns the full cache (hidden + disabled entries).
 *
 * Regardless of
 * `includeHidden`, managed models whose SDK request format is OpenAI-only are
 * dropped — the managed runtime path is Anthropic-compatible so an OpenAI-only
 * model can not be driven through it and must not leak into the TUI picker.
 * Models with no declared format metadata (legacy upstream) are kept
 * conservatively. The filter is orthogonal to `includeHidden` (a hidden
 * Anthropic model still surfaces under `includeHidden=true`; an OpenAI-only
 * model never does).
 *
 * Defense in depth: even though `loadCache` already drops the entire cache
 * file when any entry has a UUID-shaped id, filter individual
 * UUID-shaped ids again here so a partial-corruption scenario can not leak
 * UUID into the response payload (`Model.id` must be a slug).
 */
export function getCachedModels(
  options: { includeHidden?: boolean; includeLocked?: boolean } = {},
): ModelCapability[] {
  if (!isModelCapabilitiesEligible()) return []
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return []
  const includeHidden = options.includeHidden === true
  const filtered = includeHidden
    ? cached
    : cached.filter(c => c.isEnabled !== false)
  // 缺省排除 locked 模型。refresh 取全集
  // （includeLocked）后 cache 含越档付费模型的 locked:true 行；这些行只供 C 端选择器
  // 展示（worker.model/list 经 includeLocked:true 取回），但聊天/视觉「可用模型」语义
  // （默认 / swarm / findByCapability 等消费者）必须排除，
  // 否则把锁定模型当可用、打 chat 端点撞网关 402。默认 false =
  // 按构造安全，新增枚举消费者无需各自记得过滤。
  //
  // includeLocked:true 的调用点是有限且逐个论证过的集合，不是随手可加的开关：
  //   1. worker.model/list —— picker 展示路径（原始唯一调用点）
  //   2. getLockedGatewayModelIds（本文件）—— 要拿到锁定集本身
  //   3. 视觉 sidecar 的候选目录（chatMediaSidecar.ts）—— 仅当账号达到会员地板、
  //      即 `locked` 不再是本地否决时；判据在 entitlements/gatewayLock.ts
  //   4. 视觉占位文字的候选回显（mediaCapabilityPolicy.ts）—— 那是给人看的建议
  //      清单而非路由决策，锁定项带标注一并列出
  // 新增第 5 个调用点前，先说明它凭什么不属于「把锁定模型当可用」。
  const lockedFiltered =
    options.includeLocked === true
      ? filtered
      : filtered.filter(c => c.locked !== true)
  return lockedFiltered
    .filter(c => !UUID_ID_REGEX_FOR_CACHE.test(c.id))
    .filter(isAnthropicCompatibleManagedModel)
}

type MediaGenerationCapabilityKey =
  | 'supports_image_generation'
  | 'supports_video_generation'

/**
 * Read a generation-endpoint projection directly from the normalized cache.
 *
 * This deliberately does NOT call `getCachedModels()`: that accessor is the
 * managed-chat projection and always removes OpenAI-only rows, while image /
 * video generation endpoints are selected by their catalog capability rather
 * than Anthropic chat compatibility. The cache itself remains unchanged.
 *
 * Safe-by-default filters are shared by both media endpoints:
 * disabled, locked and UUID-shaped rows can never become runtime candidates.
 * The final capability predicate is endpoint-specific so image generation
 * never receives video-only rows (and vice versa).
 *
 * The current normalized cache is already slug-deduplicated at refresh time.
 * M1 intentionally does not merge same-slug profiles or change the cache
 * format; that needs an upstream identity contract before it can be lossless.
 */
function getCachedMediaGenerationModels(
  capability: MediaGenerationCapabilityKey,
): ModelCapability[] {
  if (!isModelCapabilitiesEligible()) return []
  const cached = getCatalogModels()
  if (!cached || cached.length === 0) return []
  return cached.filter(
    model =>
      model.isEnabled !== false &&
      model.locked !== true &&
      !UUID_ID_REGEX_FOR_CACHE.test(model.id) &&
      model.capabilities?.[capability] === true,
  )
}

/** OpenAI-only rows are valid when the catalog marks them image-capable. */
export function getCachedImageGenerationModels(): ModelCapability[] {
  return getCachedMediaGenerationModels('supports_image_generation')
}

/** OpenAI-only rows are valid when the catalog marks them video-capable. */
export function getCachedVideoGenerationModels(): ModelCapability[] {
  return getCachedMediaGenerationModels('supports_video_generation')
}

/**
 * Slugs of the gateway models the current account has NOT unlocked
 * (`locked === true` in the per-account entitlement projection).
 *
 * W-TUI-LOCKED-GATEWAY-MODELS (2026-07-05): single fetch point for the
 * selection gate (`validateModelSelection`'s `gatewayLockedModelIds` input)
 * and its callers — the gate itself stays pure and takes this snapshot as
 * input. Empty when the cache is missing or the account unlocks everything.
 */
export function getLockedGatewayModelIds(): string[] {
  // V116.1 P1-2 (2026-07-24):锁定集只允许来自「新鲜且权威」的目录 ——
  //   - filterStatus 非 ok 族(fallback-*/disabled-by-flag/''):降级目录未经
  //     权益过滤,可能把可用模型标锁/漏标,预检必须放行交服务端判定;
  //   - fetchedAt 超 24h:过期快照不再驱动本地拦截(事故形态:陈旧 locked 行
  //     让免费用户在服务端已修复后仍被本地预检拦死)。
  //   - 2026-07-27 新增第三条同族守卫:账号达到会员地板时 locked 本身就不权威
  //     ——它由「无权益桶」产生,而准入走 credit 链根本不看桶,被标锁的模型实测
  //     可正常调用(审计 doc §2.3-§2.5)。此处收口而非在各闸门各判一次:本函数
  //     是 selection gate 与 isLockedGatewayModel 的**单一取数点**,四形态
  //     (TUI /model、non-interactive、subagent)与 queryModel 预检因此同时收窄,
  //     不留第二处需要同步的判据。
  // 返回空集 = 预检放行,服务端仍是最终判定者(fail-open 只影响本地拦截,
  // 不影响任何计费/权益判定)。
  if (!shouldEnforceGatewayLockedFlag(getMembershipGateInput())) return []
  const state = getCatalogState()
  if (state === null) return []
  if (!isAuthoritativeFilterStatus(state.filterStatus)) return []
  if (Date.now() - state.fetchedAt > CACHE_FRESH_TTL_MS) return []
  return getCachedModels({ includeLocked: true })
    .filter(c => c.locked === true)
    .map(c => c.id)
}

/**
 * W-TUI-LOCKED-GATEWAY-MODELS D6① (2026-07-05): pre-flight predicate — is
 * `model` a gateway slug the current account has NOT unlocked?
 *
 * EXACT slug match after stripping the `[1m]`/`[2m]` context-variant suffix
 * (same normalization as model.ts's normalizeModelStringForAPI — inlined here
 * because model.ts already imports this module and a reverse import would be
 * a cycle). Deliberately never fuzzy: getModelCapability's substring fallback
 * could false-positive a custom/local reference onto a locked slug, and a
 * pre-flight must never block a legitimate request.
 */
export function isLockedGatewayModel(model: string): boolean {
  const slug = model.replace(/\[(1|2)m\]/gi, '').toLowerCase()
  return getLockedGatewayModelIds().some(id => id.toLowerCase() === slug)
}

/**
 * Returns the full capabilities object for a model from the SDK cache.
 * Returns undefined if the model is not found.
 */
export function getCachedModelCapabilities(modelId: string): ModelCapability['capabilities'] | undefined {
  const cap = getModelCapability(modelId)
  return cap?.capabilities
}

type BooleanCapabilityKey =
  | 'supports_thinking'
  | 'supports_adaptive_thinking'
  | 'supports_isp'
  | 'supports_web_search'
  | 'supports_tool_search'
  | 'supports_structured_output'
  | 'supports_effort'
  | 'supports_max_effort'
  | 'supports_fast_mode'
  | 'supports_1m_context'
  | 'supports_prompt_cache'
  | 'supports_cache_editing'
  | 'supports_token_efficient'
  | 'supports_redact_thinking'
  | 'supports_auto_mode'

/**
 * Resolve a boolean capability for a model from the SDK cache, falling back
 * to the SDK default model when the requested model has no entry for that
 * field. Catalog-driven so behavior follows the gateway, not source-pinned
 * substring patterns.
 *
 * Returns:
 *   - boolean   when the requested model OR the default model populates the field
 *   - undefined when neither has data (caller decides conservative default)
 */
export function getCachedCapabilityWithDefaultFallback(
  model: string,
  key: BooleanCapabilityKey,
): boolean | undefined {
  const direct = getCachedModelCapabilities(model)?.[key]
  if (typeof direct === 'boolean') return direct
  const defaultId = getCachedDefaultModelId()
  if (!defaultId || defaultId === model) return undefined
  const fallback = getCachedModelCapabilities(defaultId)?.[key]
  return typeof fallback === 'boolean' ? fallback : undefined
}

/**
 * Ensures the model capabilities cache is populated before the UI needs it.
 * Waits up to timeoutMs for a refresh if the cache is empty and the bridge is available.
 */
export async function ensureModelCachePopulated(timeoutMs = 5000): Promise<void> {
  const cached = getCatalogModels()
  if (cached && cached.length > 0) return
  // Y 路径 step 7 (2026-05-01): isGoBridgeAvailable 守卫已删除——SDK 直调时
  // hub 可用性无关，refreshModelCapabilities 内 try/catch 兜底静默吞。
  await Promise.race([
    refreshModelCapabilities(),
    new Promise<void>(resolve => setTimeout(resolve, timeoutMs)),
  ])
}

/**
 * 清空模型能力缓存:memoize 内存副本 + disk cache 文件。
 *
 * 必须在 logout 时调用,防止跨用户污染:用户 A (套餐 5 模型) 的 cache
 * 被用户 B (套餐 2 模型) 在 5 分钟 TTL 内消费到,UI 短暂展示 A 的 5 模型。
 *
 * 跨进程/跨语言契约:Go 端 hub auth.logout handler 也会删同一文件
 * (sdkadapter.RemoveDiskCache);两端独立删文件保证任一入口走 logout 都干净。
 * 文件不存在时 unlink 抛 ENOENT,静默吞掉。
 */
export async function clearCachedCapabilities(): Promise<void> {
  const path = getCachePath()
  // 取证 2026-05-05: 标记每次 unlink 来源 (含 stack), 让 forensic log 显示是哪条路径触发删除
  const stack = new Error('clearCachedCapabilities call site').stack ?? '(no stack)'
  logForDebugging(
    `[modelCapabilities] clearCachedCapabilities entered, stack:\n${stack}`,
  )
  // V116.1 P1-2:内存会话态(降级目录载体)必须随 logout 一并清空 ——
  // 跨用户污染防线与磁盘缓存同级(principalHash 是第二道防线,不是替代)。
  _sessionCatalog = null
  _sessionCatalogPath = null
  loadCache.cache.delete(path)
  try {
    await unlink(path)
    logForDebugging('[modelCapabilities] disk cache unlinked successfully')
  } catch (error) {
    const code = (error as NodeJS.ErrnoException)?.code
    if (code !== 'ENOENT') {
      logForDebugging(
        `[modelCapabilities] cache unlink failed: ${error instanceof Error ? error.message : 'unknown'}`,
      )
    } else {
      logForDebugging('[modelCapabilities] disk cache unlink ENOENT (already missing)')
    }
  }
}

// 重试退避 (根因修 2026-05-05): 启动期 cli.tsx:300 调 refreshModelCapabilities 时,
// SDK getAuthStatus 已就绪但 listModels 仍可能返空 (token race / 网关 entitlement
// 计算延迟 / 临时网络抖动 / Onboarding OAuth 尚未完成)。旧逻辑命中 parsed.length === 0
// 即 silent return, 后续 chat 路径在 ensureModelCached 内自己调 listModels 填 SDK
// 内存 cache, 但永远不会再触发 refreshModelCapabilities → disk cache 永不写盘 →
// /model picker 走 disk fallback 永远空。
//
// 修法: 失败 (返空或抛错) 时按 5s/15s/45s 退避重试 3 次, 给 OAuth 完成 + 网关
// entitlement 计算留充足窗口 (3 次合计 ~65s)。任一次成功立刻 reset, 后续 logout/
// re-login 周期能重新启用退避序列。setTimeout unref 防止阻塞 process.exit。
let _refreshRetryHandle: ReturnType<typeof setTimeout> | null = null
let _refreshRetryAttemptCount = 0
const REFRESH_RETRY_DELAYS_MS = [5_000, 15_000, 45_000]

function _scheduleRefreshRetry(): void {
  if (_refreshRetryHandle !== null) return
  if (_refreshRetryAttemptCount >= REFRESH_RETRY_DELAYS_MS.length) return
  const delay = REFRESH_RETRY_DELAYS_MS[_refreshRetryAttemptCount]
  _refreshRetryAttemptCount += 1
  _refreshRetryHandle = setTimeout(() => {
    _refreshRetryHandle = null
    void refreshModelCapabilities().catch(() => {
      /* best-effort, 静默; failure 已在 refreshModelCapabilities 内部 schedule 下次 */
    })
  }, delay)
  ;(_refreshRetryHandle as { unref?: () => void }).unref?.()
}

function _resetRefreshRetryState(): void {
  if (_refreshRetryHandle !== null) {
    clearTimeout(_refreshRetryHandle)
    _refreshRetryHandle = null
  }
  _refreshRetryAttemptCount = 0
}

// 2026-05-05 single-flight + cooldown 保护 (登录后 React useEffect 链反复触发
// refreshModelCapabilities 导致 TUI 卡死 → 每秒 5 次 listModels HTTP + 写盘 +
// emit OSC, event loop 被占满, Enter 按键无响应):
//   - in-flight: 同时只允许一个调用进行, 后到的等同一个 promise
//   - cooldown: 上次成功完成后 1s 内的调用直接 return (登录瞬间触发的几次都视为
//     等同一次刷新, 避免无效 HTTP)
//
// 不影响合法路径: cli.tsx 启动期一次 / login.tsx 登录后一次 / 5s 退避重试是分钟
// 级隔时, 都不会撞 1s cooldown。仅截断 200ms 级 React-loop 的反复触发。
let _refreshInFlight: Promise<void> | null = null
let _refreshLastSuccessMs = 0
const REFRESH_COOLDOWN_MS = 1000

export async function refreshModelCapabilities(): Promise<void> {
  if (!isModelCapabilitiesEligible()) return
  if (isEssentialTrafficOnly()) return

  // single-flight: 同时只允许一个调用; 后到的合流到同一个 promise
  if (_refreshInFlight !== null) {
    logForDebugging('[modelCapabilities] refresh skipped (in-flight)')
    return _refreshInFlight
  }

  // cooldown: 上次成功完成 1s 内的调用直接 return
  const sinceLastMs = Date.now() - _refreshLastSuccessMs
  if (_refreshLastSuccessMs > 0 && sinceLastMs < REFRESH_COOLDOWN_MS) {
    logForDebugging(
      `[modelCapabilities] refresh skipped (cooldown ${REFRESH_COOLDOWN_MS - sinceLastMs}ms remaining)`,
    )
    return
  }

  _refreshInFlight = _refreshModelCapabilitiesImpl()
  try {
    await _refreshInFlight
  } finally {
    _refreshInFlight = null
    _refreshLastSuccessMs = Date.now()
  }
}

async function _refreshModelCapabilitiesImpl(): Promise<void> {
  logForDebugging(
    `[modelCapabilities] refresh enter (attempt ${_refreshRetryAttemptCount + 1}/${REFRESH_RETRY_DELAYS_MS.length + 1})`,
  )
  try {
    // Y 路径 step 7 (2026-05-01): SDK Client.listModels() 直调替代 Phase 3 引入的
    // bridgeModelList IPC 中转层。SDK 内部含 IsAuthorized 短路 + 5min cache，
    // 未登录场景由本函数 try/catch 兜底静默吞。
    const client = await getAcosmiClient()
    // 取全集模式（网关 ?picker=1）。免费/未订阅
    // 账号在全集模式下能拿到越档付费模型作为 locked:true 行，供 C 端选择器渲染「付费订阅
    // 区」+ 置灰升级引导（选择器据 freeTier/locked 分区）。落盘 cache
    // 含 locked 行后，聊天「可用模型」入口 getCachedModels 默认过滤 locked（见下）；
    // media projection 也独立默认过滤 locked，永不把锁定行发往生成端点。
    // V116.1 P1-1 (2026-07-24):改调 listModelsWithStatus —— 同一端点、多带回
    // X-Entitlement-Filter-Status。仅 ok 族(权益过滤已生效)允许写磁盘权威缓存;
    // fallback-*/disabled-by-flag/''(老网关) 降级态只存内存会话态(见下方写盘段)。
    const { models, status: filterStatus } = await client.listModelsWithStatus(
      undefined,
      { includeLocked: true },
    )
    logForDebugging(
      `[modelCapabilities] listModelsWithStatus resolved rawCount=${models == null ? 'null' : models.length} filterStatus=${filterStatus === '' ? '(unknown/legacy)' : filterStatus} authorized=${client.isAuthorized()}`,
    )
    // 取证 2026-05-05 UUID→slug 切换前置: dump 第一个 raw entry 的全部字段,
    // 确认 SDK ManagedModel 是否带 modelId(slug) 字段
    if (models && models.length > 0) {
      logForDebugging(
        `[modelCapabilities] sample raw entry[0]: ${JSON.stringify(models[0])}`,
      )
      if (models.length > 1) {
        logForDebugging(
          `[modelCapabilities] sample raw entry[1]: ${JSON.stringify(models[1])}`,
        )
      }
    }
    const parsed: ModelCapability[] = []
    if (models) {
      for (const entry of models) {
        // ManagedModel 含 id / name / contextWindow / maxTokens / pricePerMTok /
        // isDefault / isEnabled / capabilities 直读字段。upstream /
        // upstreamEndpoint SDK 当前未声明在 ManagedModel 接口中，用 raw cast
        // 接收未来扩展（保留与原 BridgeModelInfo 时期相同的预留语义）。
        const raw = entry as unknown as Record<string, unknown>
        // 2026-05-05 UUID→slug 切换:
        //   - id 字段语义切换为 slug (modelId), 网关 chat URL / entitlement bucket
        //     都按 slug 索引; 旧 entry.id (UUID) 移到 uuid 字段做反查迁移
        //   - bucketInfo: 网关在 listModels 已聚合好该模型的总额/剩余, 直接保留
        const slug = (raw.modelId as string | undefined) ?? entry.id
        const uuid = entry.id !== slug ? entry.id : undefined
        // 关键: mapping 只写非 undefined 字段。`{a: undefined}` 写盘后 JSON.stringify
        // 丢弃 undefined → loadCache 解出无 'a' key, 而新 mapping 有 'a' key (值
        // undefined), lodash isEqual 比较 own keys 时返 false → 每次都判定 changed
        // → 反复写盘 → 触发 OSC/settings watchers → React useEffect 链反复触发
        // refreshModelCapabilities → TUI 卡死 (登录后 200ms 一次循环, event loop 被占)。
        const mapped: Record<string, unknown> = { id: slug }
        if (uuid !== undefined) mapped.uuid = uuid
        if (entry.name !== undefined) mapped.name = entry.name
        if (entry.provider !== undefined) mapped.provider = entry.provider
        const maxInputTokens = entry.contextWindow ?? (raw.max_input_tokens as number | undefined)
        if (maxInputTokens !== undefined) mapped.max_input_tokens = maxInputTokens
        const maxTokens = entry.maxTokens ?? (raw.max_tokens as number | undefined)
        if (maxTokens !== undefined) mapped.max_tokens = maxTokens
        if (entry.pricePerMTok !== undefined) mapped.pricePerMTok = entry.pricePerMTok
        if (entry.isDefault !== undefined) mapped.isDefault = entry.isDefault
        if (entry.isEnabled !== undefined) mapped.isEnabled = entry.isEnabled
        // 透传权益字段：仅在源值
        // present 时写键，绝不写 `{x: undefined}`（否则触发 lodash isEqual 误判 changed
        // 的 200ms 写盘死循环）。SDK 2.8.0 ManagedModel 已声明这些字段，可类型安全直读。
        if (entry.locked !== undefined) mapped.locked = entry.locked
        if (entry.freeTier !== undefined) mapped.freeTier = entry.freeTier
        if (entry.minPlanTier !== undefined) mapped.minPlanTier = entry.minPlanTier
        if (entry.chatRuntimeSupported !== undefined)
          mapped.chatRuntimeSupported = entry.chatRuntimeSupported
        if (typeof raw.upstream === 'string') mapped.upstream = raw.upstream
        if (typeof raw.upstreamEndpoint === 'string') mapped.upstreamEndpoint = raw.upstreamEndpoint
        if (typeof raw.bucketInfo === 'object' && raw.bucketInfo !== null) {
          mapped.bucketInfo = raw.bucketInfo
        }
        if (entry.capabilities !== undefined) {
          mapped.capabilities = entry.capabilities as unknown as Record<string, unknown>
        }
        // W-DESKTOP-AUTOMATION-PHASE3 (2026-05-18). Preserve
        // `inputModalities` when the SDK catalog provides it (≥ 1.2.x).
        // SDK 1.0.2 does not populate this field — `rawModalities` will
        // be undefined and the array stays absent. Visual sidecar
        // selector treats absence as "text only" (safe semantic
        // fallback). We omit
        // the key entirely when source is absent — never write
        // `{inputModalities: undefined}` (would tick the lodash isEqual
        // false-positive write loop).
        const rawModalities = raw.inputModalities as unknown
        if (Array.isArray(rawModalities)) {
          const filtered = rawModalities.filter(
            (m): m is string => typeof m === 'string',
          )
          if (filtered.length > 0) {
            mapped.inputModalities = filtered
          }
        }
        // Preserve SDK
        // `supported_formats` / `preferred_format` so the disk cache
        // carries managed-model request-format metadata. Same
        // Follow the same optional-field discipline as `inputModalities`:
        // only write the key when the source value is present (a string
        // for `preferred_format`, a non-empty string array for
        // `supported_formats`) — never `{x: undefined}`, which would tick
        // the lodash `isEqual` false-positive 200ms write loop. SDK 1.0.2
        // may not populate these; absence leaves the key off and the
        // model is treated as legacy upstream (conservatively kept).
        const rawSupportedFormats = raw.supported_formats as unknown
        if (Array.isArray(rawSupportedFormats)) {
          const filteredFormats = rawSupportedFormats.filter(
            (f): f is string => typeof f === 'string',
          )
          if (filteredFormats.length > 0) {
            mapped.supported_formats = filteredFormats
          }
        }
        if (typeof raw.preferred_format === 'string') {
          mapped.preferred_format = raw.preferred_format
        }
        const result = ModelCapabilitySchema().safeParse(mapped)
        if (result.success) parsed.push(result.data)
      }
    }
    if (parsed.length === 0) {
      logForDebugging(
        `[modelCapabilities] listModels parsed empty rawWasNull=${models == null} rawLen=${models?.length ?? -1} (attempt ${_refreshRetryAttemptCount + 1}/${REFRESH_RETRY_DELAYS_MS.length + 1}); scheduling backoff retry`,
      )
      _scheduleRefreshRetry()
      return
    }
    // 成功: reset retry 状态, 后续 logout/login 周期能重新启用退避序列。
    _resetRefreshRetryState()

    const path = getCachePath()
    // RC-2: 落盘前先按 id 去重（网关对同 model 返 anthropic/openai 双格式
    // 条目），再排序。去重在 sortForMatching 之前。
    const sortedModels = sortForMatching(dedupeModelsById(parsed))

    // V116.1 P1-1/P1-2 (2026-07-24) 会话态 + 条件落盘:
    //   1. 无论 status 如何,内存会话态总是更新 —— 当前会话的目录展示/能力解析
    //      用最新数据(降级目录也比空目录强),但降级态永不落盘、永不驱动预检
    //      (getLockedGatewayModelIds 只认 ok 族 + 24h 内)。
    //   2. 仅 ok 族写磁盘 v2 信封。写盘节流:模型集未变且盘上 principal/status
    //      一致且盘龄 < 6h → 跳过(防 watchers 回环);盘龄 ≥ 6h 重写以刷新
    //      fetchedAt(保证长驻会话的盘上新鲜度 < 24h TTL)。
    const principalHash = currentPrincipalHash()
    const fetchedAt = Date.now()
    const prevModels = getCatalogModels()
    const modelsChanged = !isEqual(prevModels, sortedModels)
    _sessionCatalog = {
      models: sortedModels,
      filterStatus,
      fetchedAt,
      principalHash,
    }
    _sessionCatalogPath = getCachePath()

    const timestamp = fetchedAt
    let wroteDisk = false
    if (isAuthoritativeFilterStatus(filterStatus)) {
      const disk = loadCache(path)
      const diskStillGood =
        disk !== null &&
        disk.principalHash === principalHash &&
        disk.filterStatus === filterStatus &&
        fetchedAt - disk.fetchedAt < DISK_REWRITE_MIN_INTERVAL_MS &&
        isEqual(disk.models, sortedModels)
      if (diskStillGood) {
        logForDebugging(
          '[modelCapabilities] cache unchanged and fresh, skipping write',
        )
        if (!modelsChanged) return
      } else {
        await mkdir(getCacheDir(), { recursive: true })
        await writeFile(
          path,
          jsonStringify({
            schemaVersion: 2,
            principalHash,
            filterStatus,
            fetchedAt,
            models: sortedModels,
          }),
          {
            encoding: 'utf-8',
            mode: 0o600,
          },
        )
        loadCache.cache.delete(path)
        wroteDisk = true
        logForDebugging(
          `[modelCapabilities] cached ${sortedModels.length} models (filterStatus=${filterStatus})`,
        )
      }
    } else {
      logForDebugging(
        `[modelCapabilities] degraded catalog (filterStatus=${filterStatus === '' ? 'unknown/legacy' : filterStatus}); kept session-only, disk cache untouched`,
      )
      if (!modelsChanged) return
    }
    // 取证 2026-05-05 disk-cache 写盘 bug: writeFile 看似成功但 disk 后续为空。
    // 立即 stat 自检 + 5s/30s 两次 watchdog 看文件是否在区间内被外部删。
    // V116.1:仅真实写盘后才有意义(降级态/节流跳过不落盘)。
    if (wroteDisk) {
      try {
        const exists0 = existsSync(path)
        const size0 = exists0 ? statSync(path).size : -1
        logForDebugging(
          `[modelCapabilities] post-write self-check: path=${path} exists=${exists0} size=${size0}`,
        )
      } catch (e) {
        logForDebugging(
          `[modelCapabilities] post-write self-check failed: ${e instanceof Error ? e.message : 'unknown'}`,
        )
      }
      const _wd1 = setTimeout(() => {
        try {
          const exists1 = existsSync(path)
          const size1 = exists1 ? statSync(path).size : -1
          logForDebugging(
            `[modelCapabilities] +5s watchdog: exists=${exists1} size=${size1}`,
          )
        } catch {/* best-effort */}
      }, 5000)
      ;(_wd1 as { unref?: () => void }).unref?.()
      const _wd2 = setTimeout(() => {
        try {
          const exists2 = existsSync(path)
          const size2 = exists2 ? statSync(path).size : -1
          logForDebugging(
            `[modelCapabilities] +30s watchdog: exists=${exists2} size=${size2}`,
          )
        } catch {/* best-effort */}
      }, 30000)
      ;(_wd2 as { unref?: () => void }).unref?.()
    }

    // OSC 推送保持原「仅变更才 emit」语义 —— 盘龄刷新(6h 重写)本身不构成
    // 目录变化,不应触发交互侧任何反应链。
    if (!modelsChanged) return

  } catch (error) {
    const errAny = error as { name?: string; status?: number; type?: string; code?: string }
    logForDebugging(
      `[modelCapabilities] fetch failed (attempt ${_refreshRetryAttemptCount + 1}/${REFRESH_RETRY_DELAYS_MS.length + 1}) errName=${errAny?.name ?? '?'} status=${errAny?.status ?? '?'} type=${errAny?.type ?? '?'} code=${errAny?.code ?? '?'} msg=${error instanceof Error ? error.message : 'unknown'}`,
    )
    // 抛错路径同样 schedule 退避重试: SDK listModels 在未登录 / token 失效 / 网络
    // 抖动时会抛 (而不是返空数组), 命中此处。retry 让 OAuth 完成后能自动恢复。
    _scheduleRefreshRetry()
  }
}
