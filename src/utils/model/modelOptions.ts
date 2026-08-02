// ANT-ONLY import markers must not be reordered
// getAntModels is ANT-ONLY; stub for external builds.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const getAntModels = (): any[] => []
import { getInitialMainLoopModel } from '../../bootstrap/state.js'
import {
  isAcosmiSubscriber,
  isMaxSubscriber,
  isTeamPremiumSubscriber,
} from '../auth.js'
import {
  diagnoseEmptyEnabledModels,
  getCachedDefaultModelId,
  getCachedEnabledModels,
  getCachedModelDisplayName,
  getCachedModels,
  getModelCapability,
  type ModelCapability,
} from './modelCapabilities.js'
import {
  MANAGED_LOCKED_REASON,
  UPGRADE_CTA_LABEL,
} from './entitlementCopy.js'
import { findModelByCapability } from './findModelByCapability.js'
import { getSettings_DEPRECATED } from '../settings/settings.js'
import { checkMaxEffort1mAccess, checkDefault1mAccess } from './check1mAccess.js'
import { getAPIProvider, isOfficialProvider } from './providers.js'
import { isModelAllowed } from './modelAllowlist.js'
import { stringWidth } from '../stringWidth.js'
import {
  getCanonicalName,
  getAcosmiUserDefaultModelDescription,
  getDefaultModel,
  getMaxEffortModel,
  getDefaultMainLoopModelSetting,
  getMarketingNameForModel,
  getUserSpecifiedModelSetting,
  isMaxEffort1MMergeEnabled,
  getMaxEffortPricingSuffix,
  renderDefaultModelSetting,
  type ModelSetting,
} from './model.js'
import { has1mContext } from '../context.js'
import { getGlobalConfig } from '../config.js'
import { isDebugMode, logForDebugging } from '../debug.js'
import { getAcosmiClient } from '../../services/acosmi/client.js'
import {
  describeLocalModelReference,
  parseLocalModelReference,
} from './localModelReference.js'
import { renderCustomModelReference } from './customModelReference.js'
import { ACCOUNT_BRIDGE_REFERENCE_PREFIX } from './accountBridgeReference.js'
import {
  canUseCustomModels,
  isCustomModelReference,
} from '../entitlements/customModels.js'
import { canUseLocalModels } from '../entitlements/localModels.js'
import { shouldEnforceGatewayLockedFlag } from '../entitlements/gatewayLock.js'
import { getMembershipGateInput } from '../auth.js'
import { t } from '../../i18n/index.js'

// SDK 复合能力查询：max_effort + 1m_context AND
function getMaxEffort1MModelId(): string {
  const all = getCachedEnabledModels()
  const m = all.find(x =>
    x.capabilities?.supports_max_effort === true &&
    x.capabilities?.supports_1m_context === true,
  )
  return m?.id ?? getMaxEffortModel()
}

// SDK 复合能力查询：default + 1m_context；fallback 任一 1m 模型 / default
function getDefault1MModelId(): string {
  const defaultId = getCachedDefaultModelId()
  if (defaultId) {
    const all = getCachedEnabledModels()
    const m = all.find(x => x.id === defaultId)
    if (m?.capabilities?.supports_1m_context === true) return defaultId
  }
  return findModelByCapability('supports_1m_context')?.id ?? defaultId ?? getDefaultModel()
}

// Per-model 剩余 % (2026-05-05 UUID→slug 切换):
//   网关 listManagedModels 已在每个 entry 内嵌 bucketInfo (聚合好的该模型
//   总额/剩余), TS 直读 ModelCapability.bucketInfo 即可, 无需再调
//   getQuotaSummary 自己 sum buckets。bucketInfo 缺失 (旧 cache / SDK 未刷新)
//   返 null, 调用方退回原"模型名+回源"描述。
function formatRemainingPercent(modelSlug?: string): string | null {
  if (!modelSlug) return null
  const cached = getCachedEnabledModels()
  const m = cached.find(x => x.id === modelSlug)
  const info = m?.bucketInfo
  if (!info) return null
  const quota = info.quotaEtu ?? 0
  const remaining = info.remainingEtu ?? 0
  if (quota <= 0) return null
  const pct = Math.max(0, Math.min(100, Math.floor((remaining / quota) * 100)))
  return `${pct}%`
}

// 一次性取证: 启动期 dump SDK 提供的 per-model entitlement 原始数据
let _quotaForensicDumped = false
export async function dumpQuotaForensicOnce(): Promise<void> {
  if (_quotaForensicDumped) return
  _quotaForensicDumped = true
  try {
    const client = await getAcosmiClient()
    logForDebugging(
      `[quotaForensic] enter authorized=${client.isAuthorized()}`,
    )
    let quotaSummary: unknown = null
    let buckets: unknown = null
    let coefficients: unknown = null
    try {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      quotaSummary = await (client as any).getQuotaSummary?.()
      logForDebugging(
        `[quotaForensic] getQuotaSummary OK json=${JSON.stringify(quotaSummary)}`,
      )
    } catch (e) {
      logForDebugging(
        `[quotaForensic] getQuotaSummary FAIL ${(e as Error)?.message}`,
      )
    }
    try {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      buckets = await (client as any).listBuckets?.()
      logForDebugging(
        `[quotaForensic] listBuckets OK json=${JSON.stringify(buckets)}`,
      )
    } catch (e) {
      logForDebugging(
        `[quotaForensic] listBuckets FAIL ${(e as Error)?.message}`,
      )
    }
    try {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      coefficients = await (client as any).listCoefficients?.()
      logForDebugging(
        `[quotaForensic] listCoefficients OK json=${JSON.stringify(coefficients)}`,
      )
    } catch (e) {
      logForDebugging(
        `[quotaForensic] listCoefficients FAIL ${(e as Error)?.message}`,
      )
    }
    // per-model: 遍历当前 enabled models, 调 getByModel 各取一次
    try {
      const models = getCachedEnabledModels()
      for (const m of models) {
        try {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const byModel = await (client as any).getByModel?.(m.id)
          logForDebugging(
            `[quotaForensic] getByModel modelId=${m.id} OK json=${JSON.stringify(byModel)}`,
          )
        } catch (e) {
          logForDebugging(
            `[quotaForensic] getByModel modelId=${m.id} FAIL ${(e as Error)?.message}`,
          )
        }
      }
    } catch (e) {
      logForDebugging(
        `[quotaForensic] enumerate models FAIL ${(e as Error)?.message}`,
      )
    }
  } catch (e) {
    logForDebugging(
      `[quotaForensic] outer FAIL ${(e as Error)?.message}`,
    )
  }
}

// @[MODEL LAUNCH]: Update all the available and default model option strings below.

export type ModelOption = {
  value: ModelSetting
  label: string
  description: string
  descriptionForModel?: string
  /**
   * PR-5 (2026-07-01 audit finding 8): which picker section this option
   * belongs to. Absent = 'gateway'. The TUI `/model` picker renders gateway /
   * custom / local as separate sections that never dedupe against each other.
   */
  group?: 'gateway' | 'custom' | 'local' | 'account'
  /**
   * Fixed right-side usage cell shared by managed and Account Bridge rows.
   * `null`/absent means the row is not a runtime-model row (for example a
   * section header or control action); `—` means a model has no structured
   * quota snapshot.
   */
  usageCell?: string
  /** Non-selectable row (section headers / guidance lines). */
  disabled?: boolean
  /**
   * W-TUI-LOCKED-GATEWAY-MODELS (2026-07-05): gateway model the current
   * account has NOT unlocked (`ModelCapability.locked === true`). The row is
   * SELECTABLE (Enter routes to the upgrade guidance, never a dead end — I3)
   * and `buildSectionedModelOptions` renders it under the
   * `── 订阅会员模型（升级解锁）──` sub-header.
   */
  locked?: boolean
}

export function getDefaultOptionForUser(fastMode = false): ModelOption {
  if (process.env.USER_TYPE === 'ant') {
    const currentModel = renderDefaultModelSetting(
      getDefaultMainLoopModelSetting(),
    )
    return {
      value: null,
      label: 'Default (recommended)',
      description: `Use the default model for Ants (currently ${currentModel})`,
      descriptionForModel: `Default model (currently ${currentModel})`,
    }
  }

  // Subscribers
  if (isAcosmiSubscriber()) {
    return {
      value: null,
      label: 'Default (recommended)',
      description: getAcosmiUserDefaultModelDescription(fastMode),
    }
  }

  // PAYG
  return {
    value: null,
    label: 'Default (recommended)',
    description: `Use the default model (currently ${renderDefaultModelSetting(getDefaultMainLoopModelSetting())})`,
  }
}

function getCustomDefaultOption(): ModelOption | undefined {
  const is3P = !isOfficialProvider()
  const customDefaultModel = process.env.ACOSMI_DEFAULT_MODEL
  // When a 3P user has a custom default-tier model string, show it directly.
  if (is3P && customDefaultModel) {
    const is1m = has1mContext(customDefaultModel)
    return {
      value: customDefaultModel,
      label:
        process.env.ACOSMI_DEFAULT_MODEL_NAME ?? customDefaultModel,
      description:
        process.env.ACOSMI_DEFAULT_MODEL_DESCRIPTION ??
        `Custom default model${is1m ? ' (1M context)' : ''}`,
      descriptionForModel: `${process.env.ACOSMI_DEFAULT_MODEL_DESCRIPTION ?? `Custom default model${is1m ? ' with 1M context' : ''}`} (${customDefaultModel})`,
    }
  }
}

function getDefaultModelOption(): ModelOption {
  const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
  const name = getCachedModelDisplayName(defaultId)
  return {
    value: defaultId,
    label: name,
    description: `${name} · Best for everyday tasks`,
    descriptionForModel:
      `${name} - best for everyday tasks. Generally recommended for most coding tasks`,
  }
}

function getCustomMaxEffortOption(): ModelOption | undefined {
  const is3P = !isOfficialProvider()
  const customMaxEffortModel = process.env.ACOSMI_DEFAULT_MAX_EFFORT_MODEL
  // When a 3P user has a custom max-effort model string, show it directly.
  if (is3P && customMaxEffortModel) {
    const is1m = has1mContext(customMaxEffortModel)
    return {
      value: customMaxEffortModel,
      label: process.env.ACOSMI_DEFAULT_MAX_EFFORT_MODEL_NAME ?? customMaxEffortModel,
      description:
        process.env.ACOSMI_DEFAULT_MAX_EFFORT_MODEL_DESCRIPTION ??
        `Custom max-effort model${is1m ? ' (1M context)' : ''}`,
      descriptionForModel: `${process.env.ACOSMI_DEFAULT_MAX_EFFORT_MODEL_DESCRIPTION ?? `Custom max-effort model${is1m ? ' with 1M context' : ''}`} (${customMaxEffortModel})`,
    }
  }
}

function getMaxEffortModelOption(fastMode = false): ModelOption {
  const maxEffortId = getMaxEffortModel()
  const name = getCachedModelDisplayName(maxEffortId)
  return {
    value: maxEffortId,
    label: name,
    description: `${name} · Most capable for complex work${getMaxEffortPricingSuffix(fastMode)}`,
    descriptionForModel: `${name} - most capable for complex work`,
  }
}

export function getDefault1MModelOption(): ModelOption {
  const default1MId = getDefault1MModelId()
  const name = getCachedModelDisplayName(default1MId)
  return {
    value: default1MId + '[1m]',
    label: `${name} (1M context)`,
    description: `${name} for long sessions`,
    descriptionForModel:
      `${name} with 1M context window - for long sessions with large codebases`,
  }
}

export function getMaxEffort1MModelOption(fastMode = false): ModelOption {
  const maxEffort1MId = getMaxEffort1MModelId()
  const name = getCachedModelDisplayName(maxEffort1MId)
  return {
    value: maxEffort1MId + '[1m]',
    label: `${name} (1M context)`,
    description: `${name} for long sessions${getMaxEffortPricingSuffix(fastMode)}`,
    descriptionForModel:
      `${name} with 1M context window - for long sessions with large codebases`,
  }
}

function getCustomFastModeOption(): ModelOption | undefined {
  const is3P = !isOfficialProvider()
  const customFastModeModel = process.env.ACOSMI_DEFAULT_FAST_MODE_MODEL
  // When a 3P user has a custom fast-mode model string, show it directly.
  if (is3P && customFastModeModel) {
    return {
      value: customFastModeModel,
      label: process.env.ACOSMI_DEFAULT_FAST_MODE_MODEL_NAME ?? customFastModeModel,
      description:
        process.env.ACOSMI_DEFAULT_FAST_MODE_MODEL_DESCRIPTION ??
        'Custom fast-mode model',
      descriptionForModel: `${process.env.ACOSMI_DEFAULT_FAST_MODE_MODEL_DESCRIPTION ?? 'Custom fast-mode model'} (${customFastModeModel})`,
    }
  }
}

function getFastModeModelOption(): ModelOption {
  const fastId = findModelByCapability('supports_fast_mode')?.id ?? getDefaultModel()
  const name = getCachedModelDisplayName(fastId)
  const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
  const defaultName = getCachedModelDisplayName(defaultId)
  return {
    value: fastId,
    label: name,
    description: `${name} · Fastest for quick answers`,
    descriptionForModel:
      `${name} - fastest for quick answers. Lower cost but less capable than ${defaultName}.`,
  }
}

function getSubscriberMaxEffortOption(fastMode = false): ModelOption {
  const maxEffortId = getMaxEffortModel()
  const name = getCachedModelDisplayName(maxEffortId)
  return {
    value: maxEffortId,
    label: name,
    description: `${name} · Most capable for complex work${fastMode ? getMaxEffortPricingSuffix(true) : ''}`,
  }
}

export function getSubscriberDefault1MOption(): ModelOption {
  const billingInfo = isAcosmiSubscriber() ? ' · Billed as extra usage' : ''
  const default1MId = getDefault1MModelId()
  const name = getCachedModelDisplayName(default1MId)
  return {
    value: default1MId + '[1m]',
    label: `${name} (1M context)`,
    description: `${name} with 1M context${billingInfo}`,
  }
}

export function getSubscriberMaxEffort1MOption(fastMode = false): ModelOption {
  const billingInfo = isAcosmiSubscriber() ? ' · Billed as extra usage' : ''
  const maxEffort1MId = getMaxEffort1MModelId()
  const name = getCachedModelDisplayName(maxEffort1MId)
  return {
    value: maxEffort1MId + '[1m]',
    label: `${name} (1M context)`,
    description: `${name} with 1M context${billingInfo}${getMaxEffortPricingSuffix(fastMode)}`,
  }
}

function getMergedMaxEffort1MOption(fastMode = false): ModelOption {
  const maxEffort1MId = getMaxEffort1MModelId()
  const name = getCachedModelDisplayName(maxEffort1MId)
  const is3P = !isOfficialProvider()
  return {
    value: maxEffort1MId + '[1m]',
    label: `${name} (1M context)`,
    description: `${name} with 1M context · Most capable for complex work${!is3P && fastMode ? getMaxEffortPricingSuffix(fastMode) : ''}`,
    descriptionForModel:
      `${name} with 1M context - most capable for complex work`,
  }
}

function getSubscriberDefaultOption(): ModelOption {
  const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
  const name = getCachedModelDisplayName(defaultId)
  return {
    value: defaultId,
    label: name,
    description: `${name} · Best for everyday tasks`,
  }
}

function getSubscriberFastModeOption(): ModelOption {
  const fastId = findModelByCapability('supports_fast_mode')?.id ?? getDefaultModel()
  const name = getCachedModelDisplayName(fastId)
  return {
    value: fastId,
    label: name,
    description: `${name} · Fastest for quick answers`,
  }
}

function getPlanModeOption(): ModelOption {
  const maxEffortId = getMaxEffortModel()
  const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
  const maxEffortName = getCachedModelDisplayName(maxEffortId)
  const defaultName = getCachedModelDisplayName(defaultId)
  return {
    value: 'planmode',
    label: `${maxEffortName} Plan Mode`,
    description: `Use ${maxEffortName} in plan mode, ${defaultName} otherwise`,
  }
}

// 模态徽标列固定显示宽度（问题 #9）。select.tsx 已保证 description 列**起点**跨行
// 对齐，但模态前缀（'· 视觉  ' / '· 纯文本  ' / 无）显示宽度不一（8 / 10 / 0），会让
// 紧随其后的「可用%」起始列在混合模态行间差 2~10 列。这里把前缀补到最宽前缀的固定
// 显示宽度，使「可用%」三类行真正成列。
//
// 必须用 stringWidth（ink，CJK 安全）而非裸 String.padEnd：中文字符是 1 code unit 但
// 占 2 显示列，padEnd 按 code unit 计会错位。
// 模态文案常量（badge 取值集；padModalityPrefix 与下方 ternary 单一真源复用）。
const MODALITY_VISION = '视觉'
const MODALITY_TEXT = '纯文本'
// 前缀显示宽度 = 所有非空模态前缀的最宽值（动态取 max，不再硬编码「纯文本最宽」的隐式
// 假设）——未来新增更长模态名时「可用%」列仍自动成列，无需手改此常量。当前 max=10。
const MODALITY_PREFIX_WIDTH = Math.max(
  ...[MODALITY_VISION, MODALITY_TEXT].map(t => stringWidth(`· ${t}  `)),
)
export function padModalityPrefix(modalityText: string): string {
  const prefix = modalityText ? `· ${modalityText}  ` : ''
  const pad = MODALITY_PREFIX_WIDTH - stringWidth(prefix)
  return pad > 0 ? prefix + ' '.repeat(pad) : prefix
}

const TUI_USAGE_DESCRIPTION_WIDTH = 44

/** Render a model description plus its fixed right-side usage cell. */
export function renderModelOptionDescription(option: ModelOption): string {
  if (option.usageCell === undefined) return option.description
  const width = stringWidth(option.description)
  const gap = Math.max(2, TUI_USAGE_DESCRIPTION_WIDTH - width)
  return `${option.description}${' '.repeat(gap)}${option.usageCell}`
}

function pickerValueKey(value: ModelOption['value']): string {
  return value === null ? 'null:<default>' : `string:${value}`
}

function dedupePickerOptionsByValue(options: ModelOption[]): ModelOption[] {
  const seen = new Set<string>()
  const out: ModelOption[] = []
  for (const option of options) {
    const key = pickerValueKey(option.value)
    if (seen.has(key)) continue
    seen.add(key)
    out.push(option)
  }
  return out
}

/**
 * Build model options dynamically from the SDK model cache for acosmi provider.
 * Returns null if cache is empty (caller should fallback to hardcoded options).
 *
 * W-TUI-LOCKED-GATEWAY-MODELS (2026-07-05): `includeLockedGateway` additionally
 * appends the account's LOCKED gateway models (subscription tier too low) as
 * selectable rows carrying `locked: true` — display path only. The usable rows
 * keep coming from `getCachedEnabledModels()` (locked-excluded, I4): default /
 * swarm / findModelByCapability consumers never see locked entries.
 */
/**
 * 第二列文案：模态徽标 + 模型名 + 价格 + 回源。
 *
 * 模态徽标作为 description 第二列的前缀（select.tsx 已保证 description 列起点
 * 跨行统一对齐）；模态缺失时不标「未知」，但仍补等宽空白占位，使「可用%」起始
 * 列在「视觉 / 纯文本 / 无模态」三类行间真正成列（问题 #9）。
 *
 * 2026-07-27 抽出：达标会员的「原 locked 行」要与普通行长得一模一样，两处各写
 * 一遍公式就是下一次两列错位的来源。
 */
function buildUsableRowDescription(
  model: ModelCapability,
  displayName: string,
  modalityText: string,
): string {
  const pricePart = model.pricePerMTok ? ` · $${model.pricePerMTok}/Mtok` : ''
  // Upstream tag — surfaces gateway routing (e.g. "回源:dashscope") so users
  // can attribute timeouts/quirks to the actual upstream rather than the
  // managed-model alias. Field is optional; absent until the acosmi
  // ListModels response populates it (see modelCapabilities ModelCapabilitySchema).
  const upstreamPart = model.upstream ? ` · 回源:${model.upstream}` : ''
  return `${padModalityPrefix(modalityText)}${displayName}${pricePart}${upstreamPart}`
}

/**
 * Modality badge (W-MULTIMODAL-INPUT P4.1, catalog-driven). Mirrors
 * classifyModelImageModality: 'image' → 视觉; non-empty without image → 纯文本;
 * absent/empty → no badge (do NOT label un-backfilled models as「未知」—— a
 * badge should assert a known fact, and most None models are flagship models
 * whose modality simply was not populated upstream).
 */
function modalityTextOf(model: ModelCapability): string {
  const mods = model.inputModalities
  return Array.isArray(mods) && mods.includes('image')
    ? MODALITY_VISION
    : Array.isArray(mods) && mods.length > 0
      ? MODALITY_TEXT
      : ''
}

function getAcosmiModelOptions(opts?: {
  includeLockedGateway?: boolean
}): ModelOption[] | null {
  // 2026-07-27：达标会员不再把网关 locked 当本地判据（entitlements/gatewayLock.ts）。
  // 对这类账号，被标锁的行必须与普通行**同样渲染**——否则 Enter 能选中、屏幕却
  // 还写着「升级解锁」，会在 TUI 中形成误导。
  const enforceLock = shouldEnforceGatewayLockedFlag(getMembershipGateInput())
  const enabledModels = getCachedEnabledModels()
  // Locked rows honour the same picker-level chat filter as usable rows
  // (D8): a locked image/video/embedding model stays out of the chat picker.
  const lockedGatewayModels =
    opts?.includeLockedGateway === true
      ? getCachedModels({ includeLocked: true }).filter(
          m => m.locked === true && m.chatRuntimeSupported !== false,
        )
      : []
  // Keep rendering when every gateway model is locked (e.g. an expired
  // account whose plan allows nothing) — an empty-cache fallback there would
  // hide the upgrade guidance behind a fake "Loading model list...".
  if (enabledModels.length === 0 && lockedGatewayModels.length === 0) {
    return null
  }

  const defaultOption = getDefaultOptionForUser()
  // Default 行: 用 default model 的 per-model 剩余% (与默认模型行一致)
  const defaultModelId = getCachedDefaultModelId() ?? undefined
  const defaultRemainingPct = formatRemainingPercent(defaultModelId)
  if (defaultRemainingPct) {
    defaultOption.usageCell = defaultRemainingPct
  } else {
    defaultOption.usageCell = '—'
  }
  const options: ModelOption[] = [defaultOption]
  for (const model of enabledModels) {
    // W-MEDIA-GEN-IGNITION PR-4 (修 G2, 裁决④甲): keep non-chat models
    // (image/video generation, embedding, rerank…) out of the chat-model
    // picker — selecting one as the chat model 402s at the gateway. Same
    // rule and semantics as every TUI chat-model selection path. Picker-layer
    // only by design: `getCachedEnabledModels()` itself stays untouched (its
    // consumers — pickAnyEnabledModel / findModelByCapability / the
    // custom-model collision P0-guard — need the un-narrowed set; see SoT §三
    // PR-4 方案乙缓行 rationale).
    if (model.chatRuntimeSupported === false) continue
    const displayName = getCachedModelDisplayName(model.id)
    // Context window from SDK capabilities (dynamic, not hardcoded)
    const ctxTokens = model.capabilities?.max_input_tokens ?? model.max_input_tokens
    const ctxLabel = ctxTokens
      ? ctxTokens >= 1_000_000
        ? `${Math.round(ctxTokens / 1_000_000)}M`
        : `${Math.round(ctxTokens / 1_000)}K`
      : undefined
    // 网关 catalog 的 model.name 已是 "DeepSeek-v4-Flash（快速1M）" 这种带括号
    // 上下文标注的格式; 再追加 (200K) 会双括号. 仅当 displayName 没有任何括号
    // 时再补 ctxLabel, 避免 "智普GLM-5.1（ 200K ） (200K)" 重复.
    const hasParenInName = /[（(]/.test(displayName)
    // Label = 模型名(上下文) only（对齐的第一列）。模态徽标移到 description 头部
    // （见下方），与「可用%」一起构成统一起点的第二列。此前 badge 拼在 label 尾，
    // 会随模型名长短在每行不同水平位置浮动，与右侧可用%列错位（问题 #9）。
    const label =
      hasParenInName || !ctxLabel
        ? displayName
        : `${displayName} (${ctxLabel})`
    const remainingPct = formatRemainingPercent(model.id)
    const description = buildUsableRowDescription(
      model,
      displayName,
      modalityTextOf(model),
    )
    options.push({
      value: model.id,
      label,
      description,
      usageCell: remainingPct ?? '—',
      descriptionForModel: `${displayName} (${model.id})`,
    })
  }
  // W-TUI-LOCKED-GATEWAY-MODELS D10 (2026-07-05): locked rows are APPENDED by
  // their own loop — the usable-row loop above stays untouched so its
  // behaviour needs no re-proof. Label mirrors the usable-row rules
  // (displayName + ctxLabel with the double-paren guard); description is the
  // shared lock copy (no remaining-% — a locked model has no bucketInfo).
  // value = the real slug, so a persisted-but-now-locked selection lands its
  // ✓ here naturally (D3) and the lock-blind tail-append path dedupes away.
  for (const model of lockedGatewayModels) {
    const displayName = getCachedModelDisplayName(model.id)
    const ctxTokens =
      model.capabilities?.max_input_tokens ?? model.max_input_tokens
    const ctxLabel = ctxTokens
      ? ctxTokens >= 1_000_000
        ? `${Math.round(ctxTokens / 1_000_000)}M`
        : `${Math.round(ctxTokens / 1_000)}K`
      : undefined
    const hasParenInName = /[（(]/.test(displayName)
    const label =
      hasParenInName || !ctxLabel
        ? displayName
        : `${displayName} (${ctxLabel})`
    options.push({
      value: model.id,
      label,
      // 达标会员：与可用行同款文案；locked:false 让它落进可用分区而不是
      //「订阅会员模型（升级解锁）」子标题下。usageCell 仍是 '—'——被标锁的模型
      // 没有 bucketInfo，这一格没有数据可显示，写别的就是编造。
      description: enforceLock
        ? `${MANAGED_LOCKED_REASON} · 回车${UPGRADE_CTA_LABEL}`
        : buildUsableRowDescription(model, displayName, modalityTextOf(model)),
      usageCell: '—',
      descriptionForModel: `${displayName} (${model.id})`,
      locked: enforceLock,
    })
  }
  return options
}

/**
 * Minimal fallback options when SDK cache is empty (offline, bridge not ready).
 * Shows only the default option and a loading hint.
 */
function getEmptyCacheFallbackOptions(): ModelOption[] {
  return [
    {
      value: null,
      label: 'Default',
      description: 'Loading model list...',
    },
  ]
}

// @[MODEL LAUNCH]: Update the model picker lists below to include/reorder options for the new model.
// Each user tier (ant, Max/Team Premium, Pro/Team Standard/Enterprise, PAYG 1P, PAYG 3P) has its own list.
function getModelOptionsBase(
  fastMode = false,
  opts?: GetModelOptionsOpts,
): ModelOption[] {
  // Acosmi provider: prefer dynamic model list from SDK cache
  if (getAPIProvider() === 'acosmi') {
    const acosmiOptions = getAcosmiModelOptions(opts)
    if (acosmiOptions) return acosmiOptions
    // Diagnostic: the gateway
    // model list is EMPTY at picker-build time. This is the exact "网关模型消失"
    // moment — when custom models are configured, getModelOptions() will append
    // them AFTER this [Default]-only fallback, so the picker shows only
    // [Default] + custom entries and the user reads it as "custom replaced the
    // gateway". It did NOT: custom models are appended, never substituted (the
    // append happens in getModelOptions, downstream of this fallback). Capture
    // WHY the gateway list is empty so a live `--debug` repro can settle the
    // audit's (a) transient-empty-cache vs (b) unexpected-filter question.
    // Debug-gated; never changes selection behaviour (RED LINE: append is not
    // touched).
    if (isDebugMode()) {
      logForDebugging(
        `[modelOptions] acosmi gateway model list EMPTY at picker build — ` +
          `reason=${diagnoseEmptyEnabledModels()}. Falling back to [Default]. ` +
          `Any custom models are APPENDED (not substituted); a "missing gateway" ` +
          `here means the SDK cache was empty, not custom models replacing it.`,
      )
    }
    // Fallback: minimal options when SDK cache is empty
    return getEmptyCacheFallbackOptions()
  }

  if (process.env.USER_TYPE === 'ant') {
    // Build options from antModels config
    const antModelOptions: ModelOption[] = getAntModels().map(m => ({
      value: m.alias,
      label: m.label,
      description: m.description ?? `[ANT-ONLY] ${m.label} (${m.model})`,
    }))

    return [
      getDefaultOptionForUser(),
      ...antModelOptions,
      getMergedMaxEffort1MOption(fastMode),
      getDefaultModelOption(),
      getDefault1MModelOption(),
      getFastModeModelOption(),
    ]
  }

  if (isAcosmiSubscriber()) {
    if (isMaxSubscriber() || isTeamPremiumSubscriber()) {
      // Premium subscriber tier: max-effort model is default, show default + 1M variants alongside.
      const premiumOptions = [getDefaultOptionForUser(fastMode)]
      if (!isMaxEffort1MMergeEnabled() && checkMaxEffort1mAccess()) {
        premiumOptions.push(getSubscriberMaxEffort1MOption(fastMode))
      }

      premiumOptions.push(getSubscriberDefaultOption())
      if (checkDefault1mAccess()) {
        premiumOptions.push(getSubscriberDefault1MOption())
      }

      premiumOptions.push(getSubscriberFastModeOption())
      return premiumOptions
    }

    // Standard subscriber tier: default model is the everyday choice, max-effort shown as alternative.
    const standardOptions = [getDefaultOptionForUser(fastMode)]
    if (checkDefault1mAccess()) {
      standardOptions.push(getSubscriberDefault1MOption())
    }

    if (isMaxEffort1MMergeEnabled()) {
      standardOptions.push(getMergedMaxEffort1MOption(fastMode))
    } else {
      standardOptions.push(getSubscriberMaxEffortOption(fastMode))
      if (checkMaxEffort1mAccess()) {
        standardOptions.push(getSubscriberMaxEffort1MOption(fastMode))
      }
    }

    standardOptions.push(getSubscriberFastModeOption())
    return standardOptions
  }

  // PAYG 1P API: default + default-1M + max-effort + max-effort-1M + fast-mode (caps-driven)
  if (isOfficialProvider()) {
    const payg1POptions = [getDefaultOptionForUser(fastMode)]
    if (checkDefault1mAccess()) {
      payg1POptions.push(getDefault1MModelOption())
    }
    if (isMaxEffort1MMergeEnabled()) {
      payg1POptions.push(getMergedMaxEffort1MOption(fastMode))
    } else {
      payg1POptions.push(getMaxEffortModelOption(fastMode))
      if (checkMaxEffort1mAccess()) {
        payg1POptions.push(getMaxEffort1MModelOption(fastMode))
      }
    }
    payg1POptions.push(getFastModeModelOption())
    return payg1POptions
  }

  // PAYG 3P: default + custom-default OR (default + default-1M) + custom-max-effort OR (max-effort + max-effort-1M) + custom-fast-mode OR fast-mode
  const payg3pOptions = [getDefaultOptionForUser(fastMode)]

  const customDefault = getCustomDefaultOption()
  if (customDefault !== undefined) {
    payg3pOptions.push(customDefault)
  } else {
    payg3pOptions.push(getDefaultModelOption())
    if (checkDefault1mAccess()) {
      payg3pOptions.push(getDefault1MModelOption())
    }
  }

  const customMaxEffort = getCustomMaxEffortOption()
  if (customMaxEffort !== undefined) {
    payg3pOptions.push(customMaxEffort)
  } else {
    // Single max-effort entry (caps-driven SDK selection) + 1M variant; no
    // duplicate slots — version-pin distinction collapsed under SDK caps.
    payg3pOptions.push(getMaxEffortModelOption(fastMode))
    if (checkMaxEffort1mAccess()) {
      payg3pOptions.push(getMaxEffort1MModelOption(fastMode))
    }
  }
  const customFastMode = getCustomFastModeOption()
  if (customFastMode !== undefined) {
    payg3pOptions.push(customFastMode)
  } else {
    payg3pOptions.push(getFastModeModelOption())
  }
  return payg3pOptions
}

// @[MODEL LAUNCH]: Add the new model ID to the appropriate family pattern below
// so the "newer version available" hint works correctly.
/**
 * Returns a ModelOption with a human-readable label from the SDK catalog.
 * Detects "newer version available" by comparing against the SDK default
 * model (id resolved via getCachedDefaultModelId) — when the user-selected
 * model has the same marketing-name family prefix as the default but a
 * different full name, suggest the default. Falls back to no upgrade hint
 * when the SDK catalog lacks the data. Returns null if the model is not
 * recognized.
 */
function getKnownModelOption(model: string): ModelOption | null {
  const marketingName = getMarketingNameForModel(model)
  if (!marketingName) return null

  // W-TUI-LOCKED-GATEWAY-MODELS D3 defense (2026-07-05): a persisted selection
  // that is now LOCKED must never render with a usable look (the 事故指纹 #1
  // "glm-5.2 裸 slug 幽灵行"). On standalone pickers the locked row already
  // exists and this path dedupes away; this branch covers the remaining
  // surfaces where locked rows are not injected. getModelCapability reads the
  // unrestricted-by-lock cache, so locked entries are visible here.
  if (getModelCapability(model)?.locked === true) {
    return {
      value: model,
      label: marketingName,
      description: `${MANAGED_LOCKED_REASON} · 回车${UPGRADE_CTA_LABEL}`,
      locked: true,
    }
  }

  const defaultId = getCachedDefaultModelId()
  if (defaultId && defaultId !== model) {
    const defaultName = getCachedModelDisplayName(defaultId)
    const family = marketingName.split(' ')[0]
    const defaultFamily = defaultName.split(' ')[0]
    if (family && defaultFamily && family === defaultFamily && marketingName !== defaultName) {
      return {
        value: model,
        label: marketingName,
        description: `Newer version available · ${defaultName}`,
      }
    }
  }

  return {
    value: model,
    label: marketingName,
    description: model,
  }
}

/**
 * W-TUI-LOCKED-GATEWAY-MODELS D8 (2026-07-05): opt-in display switches.
 * `includeLockedGateway` defaults to false so every legacy surface
 * (Settings/Config default-model pickers, installer wizard) keeps its exact
 * current output; only the standalone pickers (`/model`, chat shortcut) opt in.
 */
export type GetModelOptionsOpts = {
  includeLockedGateway?: boolean
}

export function getModelOptions(
  fastMode = false,
  opts?: GetModelOptionsOpts,
): ModelOption[] {
  // 取证: 仅在 debug 模式下一次性 dump SDK 提供的真实 entitlement 数据。
  // 非 debug 路径绝不触发 per-model SDK round-trip（picker 每次打开都会调用
  // 本函数，无条件 dump = 性能/卫生缺陷）。
  if (isDebugMode()) {
    void dumpQuotaForensicOnce()
  }
  const options = getModelOptionsBase(fastMode, opts)

  // HIGH-5 (merged audit 2026-05-21): the picker option list must respect the
  // custom / local model feature entitlements, so the menu path cannot offer
  // a model the inline `/model` gate would reject. Snapshot the entitlements
  // once and gate every custom / local option below.
  const membershipGate = getMembershipGateInput()
  const customModelsAllowed = canUseCustomModels(membershipGate)
  const localModelsAllowed = canUseLocalModels(membershipGate)

  // Add the custom model from the ACOSMI_CUSTOM_MODEL_OPTION env var
  const envCustomModel = process.env.ACOSMI_CUSTOM_MODEL_OPTION
  if (
    envCustomModel &&
    customModelsAllowed &&
    !options.some(existing => existing.value === envCustomModel)
  ) {
    options.push({
      value: envCustomModel,
      label: process.env.ACOSMI_CUSTOM_MODEL_OPTION_NAME ?? envCustomModel,
      description:
        process.env.ACOSMI_CUSTOM_MODEL_OPTION_DESCRIPTION ??
        `Custom model (${envCustomModel})`,
      group: 'custom',
      usageCell: '—',
    })
  }

  // Add custom models from the flat `customModels` registry. One option per
  // `enabled !== false` entry (value = a
  // `custom:<entry.id>` reference, W-CUSTOM-MODEL-NAMESPACE-ISOLATION
  // 2026-07-01; label = displayName ?? modelId). Gated by the custom-model
  // entitlement, same as the env-provided custom option above. When the
  // registry is non-empty, legacy `customModel` is not separately enumerated
  // (PR-2b projects legacy into this array, so reading both would duplicate
  // rows). `entry.id` is a stable UUID, so it can never collide with another
  // option's `value` — the old `options.some(v === entry.modelId) continue`
  // dedup guard is gone: it used to silently drop the entire row whenever the
  // free-form `modelId` happened to match an existing option (e.g. a gateway
  // slug), which is exactly the namespace-collision bug this change fixes.
  const customModelsRegistry = getSettings_DEPRECATED()?.customModels
  if (
    customModelsAllowed &&
    Array.isArray(customModelsRegistry) &&
    customModelsRegistry.length > 0
  ) {
    for (const entry of customModelsRegistry) {
      if (entry.enabled === false) continue
      const value = renderCustomModelReference(entry.id)
      options.push({
        value,
        label: entry.displayName ?? entry.modelId,
        description: `Custom model (${entry.modelId})`,
        group: 'custom',
        usageCell: '—',
      })
    }
  }

  // Append additional model options fetched during bootstrap
  for (const opt of getGlobalConfig().additionalModelOptionsCache ?? []) {
    if (!options.some(existing => existing.value === opt.value)) {
      options.push(opt)
    }
  }

  // Add custom model from either the current model value or the initial one
  // if it is not already in the options.
  let customModel: ModelSetting = null
  const currentMainLoopModel = getUserSpecifiedModelSetting()
  const initialMainLoopModel = getInitialMainLoopModel()
  if (currentMainLoopModel !== undefined && currentMainLoopModel !== null) {
    customModel = currentMainLoopModel
  } else if (initialMainLoopModel !== null) {
    customModel = initialMainLoopModel
  }
  if (customModel === null || options.some(opt => opt.value === customModel)) {
    return filterModelOptionsByAllowlist(options)
  } else if (customModel === 'planmode') {
    return filterModelOptionsByAllowlist([...options, getPlanModeOption()])
  } else {
    // A `local:<id>` reference is a TUI-managed local model — not a
    // managed-catalog model and not a bring-your-own custom model string.
    // Render it with a local-model label so localModels and customModels are
    // never conflated; the generic "Custom model" fallback below would mislabel
    // it. getModelOptions() is sync and cannot read the live local catalog, so
    // the option carries a "runtime not connected yet" hint instead.
    const localModelId = parseLocalModelReference(customModel)
    if (localModelId !== null) {
      // HIGH-5: a persisted `local:<id>` selection is only a pickable option
      // when the account still holds the local-model entitlement. Otherwise
      // drop it — the menu must not re-offer a model the inline gate rejects.
      if (!localModelsAllowed) {
        return filterModelOptionsByAllowlist(options)
      }
      const { label, description } = describeLocalModelReference(localModelId)
      options.push({ value: customModel, label, description, group: 'local', usageCell: '—' })
      return filterModelOptionsByAllowlist(options)
    }
    // Account Bridge owns the reserved `account:` namespace. Its rows arrive
    // asynchronously from `fetchAccountBridgeModelOptions`; never let the
    // generic persisted-value fallback mislabel a valid, dangling or malformed
    // account reference as a custom model (or duplicate a real account row).
    if (customModel.trim().startsWith(ACCOUNT_BRIDGE_REFERENCE_PREFIX)) {
      return filterModelOptionsByAllowlist(options)
    }
    // HIGH-5: same for a persisted custom (bring-your-own) model reference.
    // The generic catch-all below also covers unknown Acosmi slugs, so only
    // a confirmed custom reference is gated here.
    if (
      !customModelsAllowed &&
      isCustomModelReference({
        model: customModel,
        envCustomModelOption: process.env.ACOSMI_CUSTOM_MODEL_OPTION,
        customModel: getSettings_DEPRECATED()?.customModel,
        customModels: getSettings_DEPRECATED()?.customModels,
      })
    ) {
      return filterModelOptionsByAllowlist(options)
    }
    // Try to show a human-readable label for known Acosmi models, with an
    // upgrade hint if the alias now resolves to a newer version.
    const knownOption = getKnownModelOption(customModel)
    if (knownOption) {
      options.push(knownOption)
    } else {
      options.push({
        value: customModel,
        label: customModel,
        description: 'Custom model',
        group: 'custom',
        usageCell: '—',
      })
    }
    return filterModelOptionsByAllowlist(options)
  }
}

// ── TUI picker sections (PR-5, 2026-07-01 audit finding 8) ─────────────────

/** Values reserved for non-selectable section rows in the TUI picker. */
export const MODEL_SECTION_GATEWAY = '__section_gateway__'
/**
 * W-TUI-LOCKED-GATEWAY-MODELS D2 (2026-07-05): sub-header splitting the
 * gateway section into usable rows above / locked rows below. Split key is
 * `ModelOption.locked` (per-account entitlement), not the catalog's freeTier
 * zoning — freeTier is unfilled on prod and would fake the partition.
 */
export const MODEL_SECTION_GATEWAY_LOCKED = '__section_gateway_locked__'
export const MODEL_SECTION_CUSTOM_LOCAL = '__section_custom_local__'
/** @deprecated Kept for source compatibility; the picker emits the merged header. */
export const MODEL_SECTION_CUSTOM = '__section_custom__'
/** @deprecated Kept for source compatibility; the picker emits the merged header. */
export const MODEL_SECTION_LOCAL = '__section_local__'
export const MODEL_SECTION_LOCAL_EMPTY = '__section_local_empty__'
export const MODEL_SECTION_ACCOUNT = '__section_account__'
export const MODEL_SECTION_ACCOUNT_EMPTY = '__section_account_empty__'

// W-CUSTOM-MODEL-PLUS-GATE PR-3 — SELECTABLE control rows (unlike the
// disabled section headers above). The picker's onSelect handlers intercept
// these values before the model-selection gate ever sees them.
/** Non-member discoverability row for the custom section → gate guidance. */
export const MODEL_PICKER_CUSTOM_GATE = '__custom_gate__'
/** Non-member discoverability row for the local section → gate guidance. */
export const MODEL_PICKER_LOCAL_GATE = '__local_gate__'
/** Trailing management-entry row → CustomModelManagement screen. */
export const MODEL_PICKER_MANAGE = '__manage_models__'
/** Open the Account Bridge management view without persisting a model. */
export const MODEL_PICKER_ACCOUNT_MANAGE = '__account_bridge_manage__'
/** Re-run signed public-egress eligibility without starting OAuth/runtime. */
export const MODEL_PICKER_ACCOUNT_RECHECK = '__account_bridge_recheck__'

/** Every reserved picker-control value (section headers + selectable rows). */
export function isModelPickerControlValue(value: string | null | undefined): boolean {
  return (
    value === MODEL_SECTION_GATEWAY ||
    value === MODEL_SECTION_GATEWAY_LOCKED ||
    value === MODEL_SECTION_CUSTOM_LOCAL ||
    value === MODEL_SECTION_CUSTOM ||
    value === MODEL_SECTION_LOCAL ||
    value === MODEL_SECTION_LOCAL_EMPTY ||
    value === MODEL_SECTION_ACCOUNT ||
    value === MODEL_SECTION_ACCOUNT_EMPTY ||
    value === MODEL_PICKER_CUSTOM_GATE ||
    value === MODEL_PICKER_LOCAL_GATE ||
    value === MODEL_PICKER_MANAGE ||
    value === MODEL_PICKER_ACCOUNT_MANAGE ||
    value === MODEL_PICKER_ACCOUNT_RECHECK
  )
}

/**
 * Arrange picker options into gateway / custom / local sections with
 * non-selectable header rows. The gateway section stays at the top without a
 * header (so index 0 is always a selectable row); custom and local sections
 * follow with `── 自定义模型 ──` / `── 本地模型 ──` separators. Sections never
 * dedupe against each other — the `custom:`/`local:` namespaces already
 * guarantee value uniqueness (发现 8 的「互不干扰」要求).
 *
 * `asyncLocalOptions` are the App Server catalog rows fetched by the picker
 * component after mount (`fetchLocalModelOptions`) — `getModelOptions()` is
 * synchronous and cannot read the catalog itself (the historical reason local
 * models were never enumerated). They take precedence over any sync-derived
 * `group: 'local'` placeholder with the same value.
 *
 * `localEntitled + localLoaded` render an empty local section with guidance
 * instead of hiding it, so entitled users can discover `/local-models`.
 */
export function buildSectionedModelOptions(
  options: ModelOption[],
  asyncLocalOptions: ModelOption[],
  opts?: {
    localEntitled?: boolean
    localLoaded?: boolean
    /**
     * W-CUSTOM-MODEL-PLUS-GATE PR-3 — `/model` renders a `── 网关模型 ──`
     * header above the gateway section; legacy embedders (installer wizard)
     * keep the headerless layout, so index 0 stays selectable there.
     */
    gatewayHeader?: boolean
    /**
     * PR-3 non-member discoverability: when the custom/local entitlement is
     * missing, render the section anyway with one SELECTABLE gate row
     * (`MODEL_PICKER_CUSTOM_GATE` / `MODEL_PICKER_LOCAL_GATE`) that routes to
     * the login/upgrade guidance. `/model` only — legacy embedders omit.
     */
    customEntitled?: boolean
    /** PR-3 trailing「⚙ 管理自定义 / 本地模型…」row. `/model` only. */
    manageRow?: boolean
    /** Async Account Bridge rows/control states. Present only in standalone pickers. */
    accountOptions?: ModelOption[]
  },
): ModelOption[] {
  const gatewayAll = dedupePickerOptionsByValue(
    options.filter(o => (o.group ?? 'gateway') === 'gateway'),
  ).map(option => ({
    ...option,
    usageCell: option.usageCell ?? '—',
  }))
  // W-TUI-LOCKED-GATEWAY-MODELS D2 (2026-07-05): the gateway section renders
  // as usable rows first, then a disabled sub-header, then the locked rows
  // (selectable — Enter routes to the upgrade guidance, I3). Locked rows keep
  // their cache order (server-side sort_order).
  const gateway = gatewayAll.filter(o => o.locked !== true)
  const gatewayLocked = gatewayAll.filter(o => o.locked === true)
  const custom = dedupePickerOptionsByValue(options.filter(o => o.group === 'custom'))
  const syncLocal = dedupePickerOptionsByValue(options.filter(o => o.group === 'local'))
  const local = dedupePickerOptionsByValue([
    ...asyncLocalOptions,
    ...syncLocal.filter(
      s => !asyncLocalOptions.some(a => a.value === s.value),
    ),
  ])
  const account = dedupePickerOptionsByValue(
    (opts?.accountOptions ?? []).filter(o => o.group === 'account'),
  )

  const out: ModelOption[] = []
  if (opts?.gatewayHeader) {
    out.push({
      value: MODEL_SECTION_GATEWAY,
      label: `── ${t('model_picker_group_gateway')} ──`,
      description: '',
      disabled: true,
      group: 'gateway',
    })
  }
  out.push(...gateway)
  if (gatewayLocked.length > 0) {
    out.push({
      value: MODEL_SECTION_GATEWAY_LOCKED,
      label: `── ${t('model_picker_group_gateway_locked')} ──`,
      description: '',
      disabled: true,
      group: 'gateway',
    })
    out.push(...gatewayLocked)
  }
  const customLocalRows: ModelOption[] = []
  if (custom.length > 0) {
    customLocalRows.push(
      ...custom.map(option => ({
        ...option,
        label: `${t('model_picker_custom_prefix')} · ${String(option.label)}`,
      })),
    )
  } else if (opts?.customEntitled === false) {
    customLocalRows.push({
      value: MODEL_PICKER_CUSTOM_GATE,
      label: t('model_picker_custom_gate'),
      description: t('model_picker_gate_description'),
      group: 'custom',
    })
  }
  if (opts?.localEntitled === false) {
    customLocalRows.push({
      value: MODEL_PICKER_LOCAL_GATE,
      label: t('model_picker_local_gate'),
      description: t('model_picker_gate_description'),
      group: 'local',
    })
  } else if (local.length > 0) {
    customLocalRows.push(
      ...local.map(option => ({
        ...option,
        label: `${t('model_picker_local_prefix')} · ${String(option.label)}`,
      })),
    )
  } else if (opts?.localEntitled && opts?.localLoaded) {
    customLocalRows.push({
      value: MODEL_SECTION_LOCAL_EMPTY,
      label: t('model_picker_local_empty'),
      description: t('model_picker_local_empty_description'),
      disabled: true,
      group: 'local',
    })
  }
  if (customLocalRows.length > 0) {
    out.push({
      value: MODEL_SECTION_CUSTOM_LOCAL,
      label: `── ${t('model_source_custom_local')} ──`,
      description: '',
      disabled: true,
      group: 'custom',
    })
    out.push(...customLocalRows)
  }
  if (account.length > 0) {
    out.push({
      value: MODEL_SECTION_ACCOUNT,
      label: `── ${t('model_source_account_bridge')} ──`,
      description: '',
      disabled: true,
      group: 'account',
    })
    out.push(...account)
  }
  if (opts?.manageRow) {
    out.push({
      value: MODEL_PICKER_MANAGE,
      label: t('model_picker_manage_sources'),
      description: t('model_picker_manage_sources_description'),
    })
  }
  return out
}

/**
 * Filter model options by the availableModels allowlist.
 * Always preserves the "Default" option (value: null).
 */
function filterModelOptionsByAllowlist(options: ModelOption[]): ModelOption[] {
  const settings = getSettings_DEPRECATED() || {}
  if (!settings.availableModels) {
    return options // No restrictions
  }
  return options.filter(
    opt =>
      opt.value === null || (opt.value !== null && isModelAllowed(opt.value)),
  )
}
