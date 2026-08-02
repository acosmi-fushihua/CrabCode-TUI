import { describe, expect, test } from 'bun:test'

import {
  buildAccountBridgePickerRows,
  deriveModelAccessEntries,
  resolveAccountBridgeAccess,
} from '../../src/services/accountBridge/domain.js'
import {
  parseAccountBridgeModelRouteView,
  parseAccountBridgeRuntimeAccess,
  parseAccountBridgeUsageSnapshot,
  type AccountBridgeAccountView,
  type AccountBridgeModelRouteView,
} from '../../src/services/accountBridge/types.js'
import {
  deriveCrabCodeThinkingMode,
  resolveSupportedCrabCodeThinkingMode,
} from '../../src/services/accountBridge/thinking.js'

const routeId = 'A'.repeat(43)
const inferenceKey = 'K'.repeat(43)

function route(
  overrides: Partial<AccountBridgeModelRouteView> = {},
): AccountBridgeModelRouteView {
  return {
    routeId,
    accountId: 'acct-1',
    connectorId: 'connector-1',
    modelId: 'model-1',
    displayName: null,
    connectorLabel: 'Connector',
    accountLabel: 'Account',
    chatRuntimeSupported: true,
    supportsTools: true,
    supportsThinking: true,
    supportsAdaptiveThinking: true,
    supportsEffort: true,
    supportsMaxEffort: true,
    supportsVision: null,
    supportsJsonMode: null,
    supportedThinkingModes: ['auto', 'off', 'standard', 'deep'],
    defaultThinkingMode: 'auto',
    contextWindow: null,
    maxOutputTokens: null,
    ...overrides,
  }
}

const account: AccountBridgeAccountView = {
  accountId: 'acct-1',
  connectorId: 'connector-1',
  displayLabel: 'Account',
  status: 'ready',
  connectedAt: '2026-07-13T00:00:00Z',
  lastUsedAt: null,
  cooldownUntil: null,
}

describe('Account Bridge direct-TUI domain', () => {
  test('normalizes nullable route fields and rejects secret-shaped public views', () => {
    const parsed = parseAccountBridgeModelRouteView({
      ...route(),
      displayName: undefined,
      supportsVision: undefined,
    })
    expect(parsed.displayName).toBeNull()
    expect(parsed.supportsVision).toBeNull()
    expect(() =>
      parseAccountBridgeModelRouteView({ ...route(), apiKey: 'secret' }),
    ).toThrow(/forbidden field apiKey/)
  })

  test('accepts only the exact numeric-loopback inference endpoint', () => {
    expect(
      parseAccountBridgeRuntimeAccess({
        endpoint: 'http://127.0.0.1:43123/v1/messages',
        route: route(),
        inferenceKey,
      }).inferenceKey,
    ).toBe(inferenceKey)
    for (const endpoint of [
      'https://gateway.example/v1/messages',
      'http://localhost:43123/v1/messages',
      'http://127.0.0.1/v1/messages',
      'http://127.0.0.1:43123/v1/messages?key=x',
      'http://127.0.0.1:43123/v1/chat/completions',
    ]) {
      expect(() =>
        parseAccountBridgeRuntimeAccess({ endpoint, route: route(), inferenceKey }),
      ).toThrow(/loopback/)
    }
  })

  test('derives the limiting usage window without summing unlike windows', () => {
    const snapshot = parseAccountBridgeUsageSnapshot({
      routeId,
      accountId: 'acct-1',
      state: 'available',
      remainingPercent: 99,
      windows: [
        { label: 'daily', limit: 10, used: 2, resetsAt: null },
        { label: 'weekly', limit: 20, used: 15, resetsAt: 'later' },
      ],
      observedAt: 'now',
    })
    expect(snapshot.remainingPercent).toBe(25)
    expect(snapshot.limitingWindowLabel).toBe('weekly')
    expect(snapshot.resetsAt).toBe('later')
  })

  test('requires confirmed chat and tool capability for the TUI picker', () => {
    const input = {
      eligibility: {
        state: 'allowed' as const,
        countryCode: null,
        policyVersion: null,
        checkedAt: null,
        expiresAt: null,
        reasonCode: null,
      },
      runtime: {
        state: 'ready' as const,
        componentVersion: null,
        protocolVersion: null,
        lastErrorCode: null,
      },
      accounts: [account],
      routes: [route()],
      usageByRoute: new Map(),
    }
    expect(resolveAccountBridgeAccess(input).available).toBe(true)
    expect(buildAccountBridgePickerRows(input)[0]?.modelReference).toBe(
      `account:${routeId}`,
    )
    expect(
      resolveAccountBridgeAccess({
        ...input,
        routes: [route({ supportsTools: null })],
      }).available,
    ).toBe(false)
  })

  test('keeps thinking fallback and access-entry order deterministic', () => {
    expect(deriveCrabCodeThinkingMode({ type: 'adaptive' }, 'max')).toBe('deep')
    expect(
      resolveSupportedCrabCodeThinkingMode('deep', ['standard', 'auto']),
    ).toMatchObject({ ok: true, mode: 'standard', fellBack: true })
    expect(
      deriveModelAccessEntries(
        {
          accountBridge: {
            available: true,
            protectedReadsPermitted: true,
            disabledReasonCode: null,
            eligibilityState: 'allowed',
            connectedAccountCount: 1,
            connectorSummary: 'Connector',
          },
          customConfigured: true,
          localConfigured: false,
        },
        'en-US',
      ).map(entry => entry.kind),
    ).toEqual(['managed', 'custom-local', 'account', 'catalog'])
  })
})
