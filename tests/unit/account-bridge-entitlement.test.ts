/**
 * Account Bridge entitlement — client half of the double-layer membership gate
 * (2026-07-24 三轮方案 §3.2#2).
 *
 * The authoritative refusal is server-side (the control plane declines to sign
 * an eligibility grant below the floor). This module is the UX + defence-in-
 * depth predicate; it must behave identically to the custom/local gates so one
 * membership refresh heals every surface at once. Covers:
 *  1. allowed:  membershipActive === true AND plan ≥ BASIC.
 *  2. blocked:  membershipActive === false (free tier).
 *  3. blocked:  active but below the floor (GO) → 'tier-too-low'.
 *  4. blocked:  unknown membership (null / undefined / absent) → fail-closed.
 *  5. blocked:  active with a missing / unregistered plan code → 'unknown-tier'.
 *  6. floor parity with the custom/local gates (one product ruling).
 *  7. every reason has distinct, actionable copy naming the floor.
 */
import { describe, test, expect } from 'bun:test'
import {
  ACCOUNT_BRIDGE_MIN_TIER_DISPLAY_NAME,
  accountBridgeBlockedDetail,
  canUseAccountBridge,
  resolveAccountBridgeEntitlement,
  type AccountBridgeEntitlementReason,
} from '../../src/utils/entitlements/accountBridge.js'
import { canUseCustomModels } from '../../src/utils/entitlements/customModels.js'
import { canUseLocalModels } from '../../src/utils/entitlements/localModels.js'

describe('account-bridge entitlement — allowed accounts', () => {
  test('active membership at the Plus floor is allowed', () => {
    const result = resolveAccountBridgeEntitlement({
      membershipActive: true,
      membershipPlanCode: 'BASIC',
    })
    expect(result).toEqual({ allowed: true, reason: 'paid-subscription' })
    expect(
      canUseAccountBridge({
        membershipActive: true,
        membershipPlanCode: 'BASIC',
      }),
    ).toBe(true)
  })

  test('tiers above the floor are allowed', () => {
    for (const planCode of ['PRO', 'PRO_MAX', 'ULTRA', 'ENT_ULTRA']) {
      expect(
        canUseAccountBridge({ membershipActive: true, membershipPlanCode: planCode }),
      ).toBe(true)
    }
  })

  test('yearly billing variants normalize to their base tier', () => {
    expect(
      canUseAccountBridge({
        membershipActive: true,
        membershipPlanCode: 'pro_max_yearly',
      }),
    ).toBe(true)
  })
})

describe('account-bridge entitlement — blocked accounts', () => {
  test('free tier (membershipActive false) is blocked', () => {
    const result = resolveAccountBridgeEntitlement({ membershipActive: false })
    expect(result.allowed).toBe(false)
    expect(result.reason).toBe('free-tier')
  })

  test('unknown membership fails closed', () => {
    for (const membershipActive of [null, undefined]) {
      const result = resolveAccountBridgeEntitlement({ membershipActive })
      expect(result.allowed).toBe(false)
      expect(result.reason).toBe('no-subscription')
    }
    // Absent input entirely — the accidental `canUseAccountBridge()` call.
    expect(canUseAccountBridge()).toBe(false)
  })

  test('active membership below the floor is tier-too-low', () => {
    const result = resolveAccountBridgeEntitlement({
      membershipActive: true,
      membershipPlanCode: 'GO',
    })
    expect(result.allowed).toBe(false)
    expect(result.reason).toBe('tier-too-low')
  })

  test('active membership with a missing or unregistered plan code fails closed', () => {
    for (const membershipPlanCode of [null, undefined, '', 'NOT_A_REAL_TIER']) {
      const result = resolveAccountBridgeEntitlement({
        membershipActive: true,
        membershipPlanCode,
      })
      expect(result.allowed).toBe(false)
      expect(result.reason).toBe('unknown-tier')
    }
  })
})

describe('account-bridge entitlement — floor parity with custom / local', () => {
  test('the three paid-feature gates agree on every membership shape', () => {
    const cases = [
      { membershipActive: true, membershipPlanCode: 'BASIC' },
      { membershipActive: true, membershipPlanCode: 'GO' },
      { membershipActive: true, membershipPlanCode: 'ULTRA' },
      { membershipActive: true, membershipPlanCode: null },
      { membershipActive: false, membershipPlanCode: 'ULTRA' },
      { membershipActive: null, membershipPlanCode: 'ULTRA' },
    ] as const
    for (const input of cases) {
      expect(canUseAccountBridge(input)).toBe(canUseCustomModels(input))
      expect(canUseAccountBridge(input)).toBe(canUseLocalModels(input))
    }
  })
})

describe('account-bridge entitlement — blocked copy', () => {
  const reasons: AccountBridgeEntitlementReason[] = [
    'no-subscription',
    'free-tier',
    'tier-too-low',
    'unknown-tier',
    'paid-subscription',
  ]

  test('every reason yields distinct, non-empty copy naming the floor', () => {
    const rendered = reasons.map(accountBridgeBlockedDetail)
    for (const text of rendered) {
      expect(text.length).toBeGreaterThan(0)
      expect(text).toContain(ACCOUNT_BRIDGE_MIN_TIER_DISPLAY_NAME)
    }
    expect(new Set(rendered).size).toBe(reasons.length)
  })

  test('the floor display name is the shared constant, not a literal', () => {
    expect(ACCOUNT_BRIDGE_MIN_TIER_DISPLAY_NAME).toBe('Plus')
  })
})
