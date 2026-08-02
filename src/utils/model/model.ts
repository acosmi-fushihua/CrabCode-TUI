// ANT-ONLY import markers must not be reordered
import { resolveAntModel, getAntModelOverrideConfig } from './antModels.js'
/**
 * Ensure that any model codenames introduced here are also added to
 * scripts/excluded-strings.txt to avoid leaking them. Wrap any codename string
 * literals with process.env.USER_TYPE === 'ant' for Bun to remove the codenames
 * during dead code elimination
 */
import { getMainLoopModelOverride } from '../../bootstrap/state.js'
import {
  getSubscriptionType,
  isAcosmiSubscriber,
  isMaxSubscriber,
  isProSubscriber,
  isTeamPremiumSubscriber,
} from '../auth.js'
import {
  has1mContext,
  is1mContextDisabled,
  modelSupports1M,
} from '../context.js'
import { isEnvTruthy } from '../envUtils.js'
import { getInitialSettings } from '../settings/settings.js'
import { getCachedDefaultModelId, getCachedEnabledModels, getCachedModelCapabilities, getCachedModelDisplayName, resolveUuidToSlugFromUnfilteredCache } from './modelCapabilities.js'
import { findModelByCapability } from './findModelByCapability.js'
import {
  normalizeRuntimeModelInput,
  resolveAnyEnabledOrDefault,
  resolveCapabilityModel,
} from './runtimeModelResolution.js'

/**
 * 反查 settings.modelOverrides — 把 provider-specific model ID（如 Bedrock
 * inference profile ARN）映射回 acosmi-canonical model ID。无匹配返回原值。
 * settings 未加载时返回原值（模块顶层调用安全）。
 */
export function resolveOverriddenModel(modelId: string): string {
  let overrides: Record<string, string> | undefined
  try {
    overrides = getInitialSettings().modelOverrides
  } catch {
    return modelId
  }
  if (!overrides) {
    return modelId
  }
  for (const [canonicalId, override] of Object.entries(overrides)) {
    if (override === modelId) {
      return canonicalId
    }
  }
  return modelId
}
import { getSettings_DEPRECATED } from '../settings/settings.js'
import type { PermissionMode } from '../permissions/PermissionMode.js'
import { getAPIProvider, isOfficialProvider } from './providers.js'
import { isCustomModelReferenceId } from './customModelReference.js'
import {
  describeLocalModelReference,
  isLocalModelReference,
  parseLocalModelReference,
} from './localModelReference.js'
import { resolveCustomModelDef } from './customModelResolver.js'
import { LIGHTNING_BOLT } from '../../constants/figures.js'
import { isModelAllowed } from './modelAllowlist.js'
import { type ModelAlias, isModelAlias } from './aliases.js'
import { capitalize } from '../stringUtils.js'
import {
  DEFAULT_MAIN_LOOP_MODEL,
  DEFAULT_SMALL_FAST_MODEL,
} from './defaults.js'
export {
  DEFAULT_MAIN_LOOP_MODEL,
  DEFAULT_SMALL_FAST_MODEL,
} from './defaults.js'

export type ModelShortName = string
export type ModelName = string
export type ModelSetting = ModelName | ModelAlias | null

// ============================================================================
// CrabCode 默认主循环模型 + 双路 fallback (2026-04-26 重构)
// ----------------------------------------------------------------------------
// 显式声明 fork 自己的默认值; 不依赖上游 SDK 的"默认"。
//   启动期: catalog 报告 DEFAULT 缺失 → 用 FALLBACK
//   运行期: SDK 报 model_not_found / 持续 529 → withRetry 抛 FallbackTriggeredError
//          → query.ts 切到 FALLBACK 并打系统提示
// 切换默认/兜底只改这两行。
//
// ID 语义 (2026-05-05 UUID→slug 切换):
//   ModelCapability.id 字段 = slug ("deepseek-v4-flash"), 网关 entitlement
//   bucket / chat URL 都按 slug 索引。两个常量直接是合法 id, 旧"用 name 子串
//   匹配反查 UUID"的逻辑已废弃。
//   旧 ModelCapability.uuid 仍保留, 仅供 settings 旧 UUID 反查迁移
//   (parseUserSpecifiedModel 内部处理)。
export function getSmallFastModel(): ModelName {
  // 统一小/快模型解析链（W-SMALL-MODEL-UNIFY 2026-06-19）：
  //   1. settings.smallModel（用户显式覆盖，最高优先，逐字返回）
  //   2. ACOSMI_SMALL_FAST_MODEL env
  //   3. DEFAULT_SMALL_FAST_MODEL（deepseek-v4-flash，网关常驻；仅当 catalog
  //      校验命中才用）
  //   4. getMainLoopModel()（兜底：跟随用户当前主会话模型）
  // 取代旧第 3 层非确定的 resolveCapabilityModel('supports_fast_mode',
  // any-enabled) —— 它可能挑中高价/vision 模型当"小模型"，且旧第 4 层落静态
  // 字面 DEFAULT_MAIN_LOOP_MODEL 而非会话主模型。
  const settings = getSettings_DEPRECATED()
  const settingsModel = normalizeRuntimeModelInput(settings?.smallModel)
  if (settingsModel) return settingsModel
  const envModel = normalizeRuntimeModelInput(process.env.ACOSMI_SMALL_FAST_MODEL)
  if (envModel) return envModel
  // §10：用 getCachedEnabledModels 缓存校验，绝不调 listModels（触发 200ms
  // catalog 死循环）。该 getter 已三重过滤（isEnabled / 非 locked / Anthropic
  // 兼容），命中即保证默认模型真实可达，使第 4 层主模型兜底在企业 allowlist
  // 禁用该模型时真正生效。
  if (getCachedEnabledModels().some(m => m.id === DEFAULT_SMALL_FAST_MODEL)) {
    return DEFAULT_SMALL_FAST_MODEL
  }
  return getMainLoopModel()
}

export function isMaxEffortModel(model: ModelName): boolean {
  return getCachedModelCapabilities(model)?.supports_max_effort === true
}

/**
 * Helper to get the model from /model (including via /config), the --model flag, environment variable,
 * or the saved settings. The returned value can be a model alias if that's what the user specified.
 * Undefined if the user didn't configure anything, in which case we fall back to
 * the default (null).
 *
 * Priority order within this function:
 * 1. Model override during session (from /model command) - highest priority
 * 2. Model override at startup (from --model flag)
 * 3. ACOSMI_MODEL environment variable
 * 4. Settings (from user's saved settings)
 */
export function getUserSpecifiedModelSetting(): ModelSetting | undefined {
  let specifiedModel: ModelSetting | undefined

  const modelOverride = getMainLoopModelOverride()
  if (modelOverride !== undefined) {
    specifiedModel = modelOverride
  } else {
    const settings = getSettings_DEPRECATED() || {}
    specifiedModel = process.env.ACOSMI_MODEL || settings.model || undefined
  }

  // Ignore the user-specified model if it's not in the availableModels allowlist.
  if (specifiedModel && !isModelAllowed(specifiedModel)) {
    return undefined
  }

  return specifiedModel
}

/**
 * Get the main loop model to use for the current session.
 *
 * Model Selection Priority Order:
 * 1. Model override during session (from /model command) - highest priority
 * 2. Model override at startup (from --model flag)
 * 3. ACOSMI_MODEL environment variable
 * 4. Settings (from user's saved settings)
 * 5. Built-in default
 *
 * @returns The resolved model name to use
 */
export function getMainLoopModel(): ModelName {
  const model = getUserSpecifiedModelSetting()
  if (model !== undefined && model !== null) {
    return parseUserSpecifiedModel(model)
  }
  return getDefaultMainLoopModel()
}

export function getMaxEffortModel(): ModelName {
  const envModel = normalizeRuntimeModelInput(
    process.env.ACOSMI_DEFAULT_MAX_EFFORT_MODEL,
  )
  if (envModel) return envModel
  const maxEffort = resolveCapabilityModel('supports_max_effort', {
    source: 'getMaxEffortModel',
    fallback: 'default',
    literalFallback: DEFAULT_MAIN_LOOP_MODEL,
  })
  return maxEffort.ok ? maxEffort.model : DEFAULT_MAIN_LOOP_MODEL
}

export function getDefaultModel(): ModelName {
  const envModel = normalizeRuntimeModelInput(process.env.ACOSMI_DEFAULT_MODEL)
  if (envModel) return envModel
  const defaultModel = resolveAnyEnabledOrDefault({
    source: 'getDefaultModel',
    literalFallback: getDefaultMainLoopModel(),
  })
  return defaultModel.ok ? defaultModel.model : DEFAULT_MAIN_LOOP_MODEL
}

export function getFastModeModel(): ModelName {
  const envModel = normalizeRuntimeModelInput(
    process.env.ACOSMI_DEFAULT_FAST_MODE_MODEL,
  )
  if (envModel) return envModel
  const fastMode = resolveCapabilityModel('supports_fast_mode', {
    source: 'getFastModeModel',
    fallback: 'default',
    literalFallback: DEFAULT_MAIN_LOOP_MODEL,
  })
  return fastMode.ok ? fastMode.model : DEFAULT_MAIN_LOOP_MODEL
}

/**
 * Get the model to use for runtime, depending on the runtime context.
 * @param params Subset of the runtime context to determine the model to use.
 * @returns The model to use
 */
export function getRuntimeMainLoopModel(params: {
  permissionMode: PermissionMode
  mainLoopModel: string
  exceeds200kTokens?: boolean
}): ModelName {
  const { permissionMode, mainLoopModel, exceeds200kTokens = false } = params

  // planmode uses max-effort model in plan mode without [1m] suffix.
  if (
    getUserSpecifiedModelSetting() === 'planmode' &&
    permissionMode === 'plan' &&
    !exceeds200kTokens
  ) {
    return getMaxEffortModel()
  }

  return mainLoopModel
}

/**
 * Get the default main loop model setting.
 *
 * CrabCode fork:
 *   - ANT 内部用户保留原始 max-effort/flag 路径
 *   - 其他所有用户默认 DEFAULT_MAIN_LOOP_MODEL (DeepSeek-v4-Flash)
 *   - catalog 不含 DEFAULT 时退回 SDK 报告的默认，再无则返字面量默认
 *
 * 2026-07-27: 中间那级「降级到 MAIN_LOOP_FALLBACK_MODEL」已删。该常量指向的
 * slug 早已不在网关目录内，这一级恒不命中，只是在解析链里多一次空转。
 */
export function getDefaultMainLoopModelSetting(): ModelName | ModelAlias {
  // Ants default to defaultModel from flag config, or max-effort 1M if not configured
  if (process.env.USER_TYPE === 'ant') {
    return (
      getAntModelOverrideConfig()?.defaultModel ??
      getMaxEffortModel() + '[1m]'
    )
  }

  // CrabCode 显式默认: DEFAULT → FALLBACK → SDK/catalog 兜底
  //
  // P2-4: 自 2026-05-05 UUID→slug 切换后 ModelCapability.id 语义即 slug
  // (modelCapabilities.ts: id = modelId ?? entry.id), DEFAULT_MAIN_LOOP_MODEL
  // 也是 slug 字面量, 所以 **exact-id (slug) 匹配是权威解析**; name 子串匹配仅作
  // legacy 兜底 (网关只回显示名而无可索引 slug 的旧 catalog)。此前只按 name 子串
  // 匹配, 当 slug 不是本地化显示名的子串时漏命中, 退化到 cached[0].id (任意首个
  // 启用模型, 可能是 vision/高价模型) = 错模型。
  //
  // 大小写归一化 (2026-05-05): 两侧统一 toLowerCase 再比较。
  const cached = getCachedEnabledModels()
  const defaultLower = DEFAULT_MAIN_LOOP_MODEL.toLowerCase()
  if (cached.length > 0) {
    const flash =
      cached.find(m => m.id?.toLowerCase() === defaultLower) ??
      cached.find(m => m.name?.toLowerCase().includes(defaultLower))
    if (flash) return flash.id
    // P2-4: default 未解析 — 优先 SDK 默认或字面量默认 slug, 而非任意
    // cached[0].id 错模型 (运行期 SDK fallback 接住真 not-found)。
    return getCachedDefaultModelId() ?? DEFAULT_MAIN_LOOP_MODEL
  }
  // catalog 未加载: 优先 SDK disk cache 的 default; 再无则返字面量常量
  // (运行期 SDK fallback 接住可能的 model_not_found, 比返空串触发
  // ModelNotFoundError "" 强 — 后者 picker UI 显示空 default 名)。
  return getCachedDefaultModelId() ?? DEFAULT_MAIN_LOOP_MODEL
}

/**
 * Synchronous operation to get the default main loop model to use
 * (bypassing any user-specified values).
 */
export function getDefaultMainLoopModel(): ModelName {
  return parseUserSpecifiedModel(getDefaultMainLoopModelSetting())
}

/**
 * 运行期 fallback 模型解析。`main.tsx` 启动时填 `--fallback-model` 缺省值用。
 *
 * 2026-07-27: 原实现按 MAIN_LOOP_FALLBACK_MODEL 常量在 catalog 里做 name 子串
 * 匹配。该常量指向的 slug 已不在目录内 → 匹配恒不命中 → 实际恒走末行，即
 * 「SDK 默认，或一个网关不服务的字面量」。删常量后保留的正是那条实际生效的
 * 路径，并去掉了不服务的兜底：网关自己报告的默认模型是这里唯一有数据支撑的
 * 答案，catalog 未加载时才退回本地默认 slug。
 *
 * 刻意**不**改成「挑一个与主模型不同的目录模型」：那会把一个当前无操作的开关
 * 变成「主模型失败即自动改用另一个（可能更贵的）模型」的新运行期行为。那是
 * 一条独立的产品决策，不该夹带在缺陷修复里（同 D4 的分离原则）。
 */
export function getDefaultFallbackModel(): ModelName {
  return getCachedDefaultModelId() ?? DEFAULT_MAIN_LOOP_MODEL
}

// @[MODEL LAUNCH]: Add a canonical name mapping for the new model below.
/**
 * Pure string-match that returns a canonical (lowercased) model id. Acosmi
 * provider hits the SDK enabled-models cache first; everything else falls
 * through with the input lowercased. Does not touch settings, so safe at
 * module top-level (see MODEL_COSTS in modelCost.ts).
 */
export function firstPartyNameToCanonical(name: ModelName): ModelShortName {
  name = name.toLowerCase()

  // For acosmi provider, check SDK cache first
  if (getAPIProvider() === 'acosmi') {
    const models = getCachedEnabledModels()
    if (models.length > 0) {
      const exact = models.find(m => m.id.toLowerCase() === name)
      if (exact) return exact.id.toLowerCase()
    }
  }

  return name
}

/**
 * Maps a full model string to a canonical (lowercased) id, resolving any
 * settings.modelOverrides indirection first. Provider-specific suffix
 * stripping has been retired — canonical ids come straight from the SDK
 * catalog for the acosmi provider; other providers pass through verbatim.
 */
export function getCanonicalName(fullModelName: ModelName): ModelShortName {
  return firstPartyNameToCanonical(resolveOverriddenModel(fullModelName))
}

export function getAcosmiUserDefaultModelDescription(
  fastMode = false,
): string {
  if (isMaxSubscriber() || isTeamPremiumSubscriber()) {
    const maxEffortId = getMaxEffortModel()
    const maxEffortName = getCachedModelDisplayName(maxEffortId)
    if (isMaxEffort1MMergeEnabled()) {
      return `${maxEffortName} with 1M context · Most capable for complex work${fastMode ? getMaxEffortPricingSuffix(true) : ''}`
    }
    return `${maxEffortName} · Most capable for complex work${fastMode ? getMaxEffortPricingSuffix(true) : ''}`
  }
  const defaultId = getDefaultMainLoopModel()
  const defaultName = getCachedModelDisplayName(defaultId)
  return `${defaultName} · Best for everyday tasks`
}

export function renderDefaultModelSetting(
  setting: ModelName | ModelAlias,
): string {
  if (setting === 'planmode') {
    const maxEffortId = getMaxEffortModel()
    const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
    const maxEffortName = getCachedModelDisplayName(maxEffortId)
    const defaultName = getCachedModelDisplayName(defaultId)
    return `${maxEffortName} in plan mode, else ${defaultName}`
  }
  return renderModelName(parseUserSpecifiedModel(setting))
}

export function getMaxEffortPricingSuffix(_fastMode: boolean): string {
  // SDK pricePerMTok 不区分 fast/normal mode, fast mode 真价 SDK 未提供
  // → 不显示价格段。SDK 后续提供 fast pricing 字段时再恢复 · 标注。
  return ''
}

export function isMaxEffort1MMergeEnabled(): boolean {
  if (
    is1mContextDisabled() ||
    isProSubscriber() ||
    !isOfficialProvider()
  ) {
    return false
  }
  // Fail closed when a subscriber's subscription type is unknown. The VS Code
  // config-loading subprocess can have OAuth tokens with valid scopes but no
  // subscriptionType field (stale or partial refresh). Without this guard,
  // isProSubscriber() returns false for such users and the merge leaks
  // max-effort[1m] into the model dropdown — the API then rejects it with a
  // misleading "rate limit reached" error.
  if (isAcosmiSubscriber() && getSubscriptionType() === null) {
    return false
  }
  return true
}

export function renderModelSetting(setting: ModelName | ModelAlias): string {
  if (setting === 'planmode') {
    const maxEffortId = getMaxEffortModel()
    const maxEffortName = getCachedModelDisplayName(maxEffortId)
    return `${maxEffortName} Plan`
  }
  if (isModelAlias(setting)) {
    return capitalize(setting)
  }
  return renderModelName(setting)
}

// @[MODEL LAUNCH]: Add display name cases for the new model (base + [1m] variant if applicable).
/**
 * Returns a human-readable display name for known public models, or null
 * if the model is not recognized as a public model.
 * Display names are resolved dynamically from the SDK model cache.
 */
export function getPublicModelDisplayName(model: ModelName): string | null {
  // PR-4 (2026-07-01 audit finding 7): prefixed refs resolve their display
  // name from their own registries — the SDK cache below only knows gateway
  // models, and its miss-path `deriveDisplayNameFromId` word-splits a
  // `custom:<uuid>` into garbage ("A716 446655440000") that then sits in the
  // TUI status line / Logo. A dangling ref shows the raw reference (honest,
  // and PR-3/PR-6 reset it shortly after).
  if (isCustomModelReferenceId(model)) {
    const def = resolveCustomModelDef(model)
    return def ? (def.displayName ?? def.id) : model
  }
  if (isLocalModelReference(model)) {
    const bare = parseLocalModelReference(model)
    return bare !== null ? describeLocalModelReference(bare).label : model
  }
  const baseModel = model.replace(/\[1m\]$/, '')

  // SDK cache — authoritative for acosmi (model IDs are UUIDs, not in knownModels)
  const sdkName = getCachedModelDisplayName(baseModel)
  if (sdkName && sdkName !== baseModel) {
    return sdkName
  }

  // Fallback: SDK enabled models for non-acosmi providers
  const knownModels = getCachedEnabledModels().map(m => m.id)
  const isKnown = knownModels.includes(baseModel) ||
    knownModels.some(k => getCanonicalName(k) === getCanonicalName(baseModel))
  if (!isKnown) return null

  const displayName = getCachedModelDisplayName(baseModel)
  return displayName
}

function maskModelCodename(baseName: string): string {
  // Mask only the first dash-separated segment (the codename), preserve the rest
  // e.g. capybara-v2-fast → cap*****-v2-fast
  const [codename = '', ...rest] = baseName.split('-')
  const masked =
    codename.slice(0, 3) + '*'.repeat(Math.max(0, codename.length - 3))
  return [masked, ...rest].join('-')
}

export function renderModelName(model: ModelName): string {
  const publicName = getPublicModelDisplayName(model)
  if (publicName) {
    return publicName
  }
  if (process.env.USER_TYPE === 'ant') {
    const resolved = parseUserSpecifiedModel(model)
    const antModel = resolveAntModel(model)
    if (antModel) {
      const baseName = antModel.model.replace(/\[1m\]$/i, '')
      const masked = maskModelCodename(baseName)
      const suffix = has1mContext(resolved) ? '[1m]' : ''
      return masked + suffix
    }
    if (resolved !== model) {
      return `${model} (${resolved})`
    }
    return resolved
  }
  return model
}

/**
 * Returns a safe author name for public display (e.g., in git commit trailers).
 * Returns the SDK display name for publicly known models, or the raw model string
 * for unknown/internal models.
 */
export function getPublicModelName(model: ModelName): string {
  const publicName = getPublicModelDisplayName(model)
  return publicName ?? model
}

/**
 * Returns a full model name for use in this session, possibly after resolving
 * a model alias.
 *
 * This function intentionally does not support version numbers to align with
 * the model switcher.
 *
 * Supports [1m] suffix on any model alias to enable 1M context window without
 * requiring each variant to be in MODEL_ALIASES.
 *
 * @param modelInput The model alias or name provided by the user.
 */
export function parseUserSpecifiedModel(
  modelInput: ModelName | ModelAlias,
): ModelName {
  const modelInputTrimmed = modelInput.trim()
  const normalizedModel = modelInputTrimmed.toLowerCase()

  const has1mTag = has1mContext(normalizedModel)
  const modelString = has1mTag
    ? normalizedModel.replace(/\[1m]$/i, '').trim()
    : normalizedModel

  if (isModelAlias(modelString)) {
    switch (modelString) {
      case 'planmode':
        // plan mode resolves to the default tier; getRuntimeMainLoopModel
        // overrides to max-effort when permissionMode === 'plan'.
        return getDefaultModel() + (has1mTag ? '[1m]' : '')
      case 'best':
        return getMaxEffortModel() + (has1mTag ? '[1m]' : '')
      default:
    }
  }

  if (process.env.USER_TYPE === 'ant') {
    const has1mAntTag = has1mContext(normalizedModel)
    const baseAntModel = normalizedModel.replace(/\[1m]$/i, '').trim()

    const antModel = resolveAntModel(baseAntModel)
    if (antModel) {
      const suffix = has1mAntTag ? '[1m]' : ''
      return antModel.model + suffix
    }

    // Fall through to the alias string if we cannot load the config. The API calls
    // will fail with this string, but we should hear about it through feedback and
    // can tell the user to restart/wait for flag cache refresh to get the latest values.
  }

  // Preserve original case for custom model names (e.g., Azure Foundry deployment IDs)
  // Only strip [1m] suffix if present, maintaining case of the base model
  const baseModel = has1mTag
    ? modelInputTrimmed.replace(/\[1m\]$/i, '').trim()
    : modelInputTrimmed

  // 2026-05-05 UUID→slug 兼容: 旧 settings/AppState 持久化的 mainLoopModel 可能
  // 是 UUID; 网关 chat URL 拼 UUID 会撞 entitlement bucket 不命中 → 402。
  // 此处反查 ManagedModel 把 UUID 替换成 slug, 保持下游 chat 路径透明。
  //
  // 必须查 TRULY UNFILTERED cache (resolveUuidToSlugFromUnfilteredCache),
  // 而非 getCachedEnabledModels — 后者按 isEnabled / OpenAI-only 过滤, 一个
  // 现已禁用 / OpenAI-only 模型的 UUID 反查失败 → 裸 UUID 漏进 chat URL → §9
  // 硬 402（chat URL 必须用 slug）。无命中时回退 getDefaultModel()
  //（一定是 slug）— 绝不让裸 UUID 进 chat URL（静默降级到默认模型可接受，
  // 远优于硬 402)。
  let resolvedBase = baseModel
  if (looksLikeUuid(baseModel)) {
    resolvedBase =
      resolveUuidToSlugFromUnfilteredCache(baseModel) ?? getDefaultModel()
  }

  if (has1mTag) {
    return resolvedBase + '[1m]'
  }
  return resolvedBase
}

const UUID_REGEX = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

function looksLikeUuid(value: string): boolean {
  return UUID_REGEX.test(value)
}

/**
 * Resolves a skill's `model:` frontmatter against the current model, carrying
 * the `[1m]` suffix over when the target supports it.
 *
 * A skill author specifying a max-effort or default-tier model means "use that
 * tier", not "downgrade to 200K". If the user is on a 1M-context session and
 * invokes a skill with a bare model id whose canonical form supports 1M, the
 * caller would otherwise lose the 1M window — tripping autocompact at low
 * apparent usage and surfacing "Context limit reached" even though nothing
 * overflowed.
 *
 * We only carry [1m] when the target actually supports it. Skill targets that
 * have no 1M variant downgrade naturally. Skills that already specify [1m]
 * are left untouched.
 */
export function resolveSkillModelOverride(
  skillModel: string,
  currentModel: string,
): string {
  if (has1mContext(skillModel) || !has1mContext(currentModel)) {
    return skillModel
  }
  // modelSupports1M matches on canonical IDs; resolve first to canonicalize.
  if (modelSupports1M(parseUserSpecifiedModel(skillModel))) {
    return skillModel + '[1m]'
  }
  return skillModel
}

/**
 * Opt-out env var historically used to disable the now-retired first-party
 * legacy SKU remap path. Kept as a public boolean so external callers (e.g.
 * tests, third-party providers) that still set CRABCODE_DISABLE_LEGACY_MODEL_REMAP
 * have a stable read. The remap path itself was removed in P4-Z3 — semantic
 * aliases ('best' / 'planmode') and SDK ids fully cover model selection.
 */
export function isLegacyModelRemapEnabled(): boolean {
  return !isEnvTruthy(process.env.CRABCODE_DISABLE_LEGACY_MODEL_REMAP)
}

export function modelDisplayString(model: ModelSetting): string {
  if (model === null) {
    if (process.env.USER_TYPE === 'ant') {
      return `Default for Ants (${renderDefaultModelSetting(getDefaultMainLoopModelSetting())})`
    } else if (isAcosmiSubscriber()) {
      return `Default (${getAcosmiUserDefaultModelDescription()})`
    }
    return `Default (${getDefaultMainLoopModel()})`
  }
  const resolvedModel = parseUserSpecifiedModel(model)
  return model === resolvedModel ? resolvedModel : `${model} (${resolvedModel})`
}

// @[MODEL LAUNCH]: Add a marketing name mapping for the new model below.
export function getMarketingNameForModel(modelId: string): string | undefined {
  if (getAPIProvider() === 'foundry') {
    // deployment ID is user-defined in Foundry, so it may have no relation to the actual model
    return undefined
  }

  const has1m = modelId.toLowerCase().includes('[1m]')
  const canonical = getCanonicalName(modelId)

  // Try to resolve from SDK cache for any known canonical model
  const baseCanonical = canonical.replace(/\[1m\]$/i, '')
  const displayName = getCachedModelDisplayName(baseCanonical)
  // If deriveDisplayNameFromId returned the raw ID unchanged, it's unrecognized
  if (displayName !== baseCanonical) {
    return has1m ? `${displayName} (with 1M context)` : displayName
  }

  return undefined
}

export function normalizeModelStringForAPI(model: string): string {
  return model.replace(/\[(1|2)m\]/gi, '')
}
