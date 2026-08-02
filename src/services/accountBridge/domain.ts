import {
  renderAccountBridgeReference,
} from '../../utils/model/accountBridgeReference.js'
import {
  type AccountBridgeAccountView,
  type AccountBridgeConnectorView,
  type AccountBridgeEligibilityView,
  type AccountBridgeModel,
  type AccountBridgeModelRouteView,
  type AccountBridgeRuntimeView,
  type AccountBridgeUsageSnapshot,
  type ModelAccessEntryView,
} from './types.js'
import type { Locale } from '../../i18n/config.js'

export interface AccountBridgeAccessInput {
  eligibility: AccountBridgeEligibilityView
  runtime: AccountBridgeRuntimeView
  accounts: readonly AccountBridgeAccountView[]
  connectors?: readonly AccountBridgeConnectorView[]
  routes: readonly AccountBridgeModelRouteView[]
  usageByRoute?: ReadonlyMap<string, AccountBridgeUsageSnapshot>
}

export interface AccountBridgeAccessView {
  available: boolean
  protectedReadsPermitted: boolean
  disabledReasonCode: string | null
  eligibilityState: AccountBridgeEligibilityView['state']
  connectedAccountCount: number
  connectorSummary: string | null
  models: AccountBridgeModel[]
}

/**
 * Protected reads (runtime / accounts / routes / usage) are admitted when the
 * region is allowed, or under blocked-cn
 * when the signed directory carries at least one region-exempt enabled
 * connector (the worker applies the CN floor before setting `enabled`, so the
 * directory row is authoritative). checking / unavailable / consent-gated
 * views keep an empty allow set and all-disabled directory rows, so this
 * never widens the fail-closed lock.
 */
export function accountBridgeProtectedReadsPermitted(
  eligibility: Pick<AccountBridgeEligibilityView, 'state'> | null,
  connectors: readonly AccountBridgeConnectorView[],
): boolean {
  if (eligibility?.state === 'allowed') return true
  if (eligibility?.state !== 'blocked-cn') return false
  return connectors.some(connector => connector.enabled)
}

export interface AccountBridgePickerRow {
  modelReference: string
  routeId: string
  label: string
  description: string
  usageCell: string
  disabledReasonCode: string | null
  capability: AccountBridgeModelRouteView
  usage: AccountBridgeUsageSnapshot | null
}

export function resolveAccountBridgeAccess(
  input: AccountBridgeAccessInput,
): AccountBridgeAccessView {
  const protectedReadsPermitted = accountBridgeProtectedReadsPermitted(
    input.eligibility,
    input.connectors ?? [],
  )
  const globalReason = !protectedReadsPermitted
    ? input.eligibility.reasonCode ?? input.eligibility.state
    : input.runtime.state !== 'ready'
      ? input.runtime.lastErrorCode ?? `runtime-${input.runtime.state}`
      : null

  const accounts = new Map(input.accounts.map(account => [account.accountId, account]))
  const models = input.routes.map(route => ({
    ...route,
    usage: input.usageByRoute?.get(route.routeId) ?? null,
  }))
  const hasRunnableRoute = models.some(model => {
    const account = accounts.get(model.accountId)
    return (
      account?.status === 'ready' &&
      model.chatRuntimeSupported === true &&
      model.supportsTools === true
    )
  })

  return {
    available: globalReason === null && hasRunnableRoute,
    protectedReadsPermitted,
    disabledReasonCode:
      globalReason ?? (hasRunnableRoute ? null : 'no-agent-capable-route'),
    eligibilityState: input.eligibility.state,
    connectedAccountCount: input.accounts.length,
    connectorSummary: connectorSummary(input.connectors ?? []),
    models,
  }
}

function connectorSummary(
  connectors: readonly AccountBridgeConnectorView[],
): string | null {
  const labels = [
    ...new Set(
      connectors
        .map(connector => connector.displayName.trim())
        .filter(Boolean),
    ),
  ]
  return labels.length > 0 ? labels.join(' / ') : null
}

export function usageCellForAccountBridge(
  usage: AccountBridgeUsageSnapshot | null,
): string {
  if (usage?.remainingPercent !== null && usage?.remainingPercent !== undefined) {
    return `${Math.floor(Math.min(100, Math.max(0, usage.remainingPercent)))}%`
  }
  if (usage?.state === 'exhausted') return '0%'
  if (usage?.state === 'cooldown') return '冷却'
  return '—'
}

export function buildAccountBridgePickerRows(
  access: AccountBridgeAccessInput,
): AccountBridgePickerRow[] {
  const accounts = new Map(access.accounts.map(account => [account.accountId, account]))
  const global = resolveAccountBridgeAccess(access)
  return global.models.map(model => {
    const account = accounts.get(model.accountId)
    const disabledReasonCode =
      global.disabledReasonCode ??
      (!account
        ? 'account-missing'
        : account.status !== 'ready'
          ? `account-${account.status}`
          : model.chatRuntimeSupported !== true
            ? model.chatRuntimeSupported === null
              ? 'chat-runtime-capability-unknown'
              : 'chat-runtime-unsupported'
            : model.supportsTools !== true
              ? model.supportsTools === null
                ? 'tool-capability-unknown'
                : 'tools-unsupported'
              : null)
    return {
      modelReference: renderAccountBridgeReference(model.routeId),
      routeId: model.routeId,
      label: model.displayName ?? model.modelId,
      description: `${model.connectorLabel} · ${model.accountLabel}`,
      usageCell: usageCellForAccountBridge(model.usage),
      disabledReasonCode,
      capability: model,
      usage: model.usage,
    }
  })
}

export interface DeriveModelAccessEntriesInput {
  accountBridge: Pick<
    AccountBridgeAccessView,
    | 'available'
    | 'protectedReadsPermitted'
    | 'disabledReasonCode'
    | 'eligibilityState'
    | 'connectedAccountCount'
    | 'connectorSummary'
  >
  customConfigured: boolean
  localConfigured: boolean
}

interface ModelAccessEntryCopy {
  managedLabel: string
  managedDescription: string
  customLocalLabel: string
  customLocalDescription: string
  configuredBadge: string
  accountLabel: string
  accountBase: (connectorSummary: string | null) => string
  accountCheckingDescription: string
  accountBlockedDescription: string
  accountBlockedPartialDescription: string
  accountUnavailableDescription: string
  accountCheckingBadge: string
  accountBlockedBadge: string
  accountBlockedPartialBadge: string
  accountUnavailableBadge: string
  accountConnectedBadge: (count: number) => string
  accountAvailableBadge: string
  catalogLabel: string
  catalogDescription: string
}

const MODEL_ACCESS_ENTRY_COPY: Record<Locale, ModelAccessEntryCopy> = {
  'zh-CN': {
    managedLabel: 'CrabCode 订阅账户',
    managedDescription: 'Go、Plus、Pro、Pro Max 或 Ultra',
    customLocalLabel: '自定义/本地模型',
    customLocalDescription: '配置自定义 API 端点，或在本机运行本地模型',
    configuredBadge: '已配置',
    accountLabel: '账户接入',
    accountBase: connectorSummary => connectorSummary
      ? `通过 OAuth 连接 ${connectorSummary} 账户`
      : '通过 OAuth 连接已审核的服务商账户',
    accountCheckingDescription: '正在按当前公网出口 IP 判定是否可用',
    accountBlockedDescription: '此功能仅面向中国大陆以外地区开放，系统按当前公网出口 IP 判断',
    accountBlockedPartialDescription: '当前地区可使用区域豁免连接器，其余连接器仅面向中国大陆以外地区开放',
    accountUnavailableDescription: '无法确认当前公网出口地区，账户接入暂未解锁，请检查网络后重试',
    accountCheckingBadge: '正在检查地区',
    accountBlockedBadge: '当前地区不可用',
    accountBlockedPartialBadge: '当前地区部分可用',
    accountUnavailableBadge: '地区待确认',
    accountConnectedBadge: count => `已连接 ${count} 个账户`,
    accountAvailableBadge: '可用 · 未连接',
    catalogLabel: '模型目录',
    catalogDescription: '中国区 / 国际区 / 网关路由',
  },
  'en-US': {
    managedLabel: 'CrabCode subscription account',
    managedDescription: 'Go, Plus, Pro, Pro Max, or Ultra',
    customLocalLabel: 'Custom / local models',
    customLocalDescription: 'Configure a custom API endpoint or run a model locally',
    configuredBadge: 'Configured',
    accountLabel: 'Account access',
    accountBase: connectorSummary => connectorSummary
      ? `Connect ${connectorSummary} accounts with OAuth`
      : 'Connect reviewed provider accounts with OAuth',
    accountCheckingDescription: 'Checking availability from the current public egress IP',
    accountBlockedDescription: 'This feature is available only outside mainland China; availability is determined from the current public egress IP',
    accountBlockedPartialDescription: 'Region-exempt connectors are available here; the remaining connectors are only available outside mainland China',
    accountUnavailableDescription: 'The current public egress region could not be confirmed, so account access remains locked; check the network and try again',
    accountCheckingBadge: 'Checking region',
    accountBlockedBadge: 'Unavailable in the current region',
    accountBlockedPartialBadge: 'Partially available in the current region',
    accountUnavailableBadge: 'Region not confirmed',
    accountConnectedBadge: count => `${count} account(s) connected`,
    accountAvailableBadge: 'Available · Not connected',
    catalogLabel: 'Model catalog',
    catalogDescription: 'China region / Global region / gateway routing',
  },
}

export function deriveModelAccessEntries(
  input: DeriveModelAccessEntriesInput,
  locale: Locale,
): ModelAccessEntryView[] {
  const copy = MODEL_ACCESS_ENTRY_COPY[locale]
  return [
    {
      kind: 'managed',
      label: copy.managedLabel,
      description: copy.managedDescription,
      badge: null,
      disabledReasonCode: null,
      action: 'select-managed-model',
    },
    {
      kind: 'custom-local',
      label: copy.customLocalLabel,
      description: copy.customLocalDescription,
      badge:
        input.customConfigured || input.localConfigured
          ? copy.configuredBadge
          : null,
      disabledReasonCode: null,
      action: 'open-custom-local-model',
    },
    {
      kind: 'account',
      label: copy.accountLabel,
      description: accountBridgeEntryDescription(input.accountBridge, copy),
      badge: accountBridgeStatusBadge(input.accountBridge, copy),
      disabledReasonCode: input.accountBridge.available
        ? null
        : input.accountBridge.disabledReasonCode,
      action: input.accountBridge.eligibilityState === 'checking'
        ? null
        : input.accountBridge.protectedReadsPermitted
          ? 'open-account-bridge'
          : 'recheck-account-bridge',
    },
    {
      kind: 'catalog',
      label: copy.catalogLabel,
      description: copy.catalogDescription,
      badge: null,
      disabledReasonCode: null,
      action: 'open-model-catalog',
    },
  ]
}

function accountBridgeEntryDescription(
  access: DeriveModelAccessEntriesInput['accountBridge'],
  copy: ModelAccessEntryCopy,
): string {
  const base = copy.accountBase(access.connectorSummary)
  switch (access.eligibilityState) {
    case 'checking':
      return `${base} · ${copy.accountCheckingDescription}`
    case 'blocked-cn':
      return `${base} · ${
        access.protectedReadsPermitted
          ? copy.accountBlockedPartialDescription
          : copy.accountBlockedDescription
      }`
    case 'unavailable':
      return `${base} · ${copy.accountUnavailableDescription}`
    case 'allowed':
      return base
  }
}

function accountBridgeStatusBadge(
  access: DeriveModelAccessEntriesInput['accountBridge'],
  copy: ModelAccessEntryCopy,
): string {
  switch (access.eligibilityState) {
    case 'checking':
      return copy.accountCheckingBadge
    case 'blocked-cn':
      return access.protectedReadsPermitted
        ? access.connectedAccountCount > 0
          ? copy.accountConnectedBadge(access.connectedAccountCount)
          : copy.accountBlockedPartialBadge
        : copy.accountBlockedBadge
    case 'unavailable':
      return copy.accountUnavailableBadge
    case 'allowed':
      return access.connectedAccountCount > 0
        ? copy.accountConnectedBadge(access.connectedAccountCount)
        : copy.accountAvailableBadge
  }
}
