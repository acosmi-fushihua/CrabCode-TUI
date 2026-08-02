import { describe, expect, test } from 'bun:test'
import {
  acquireDirectAccountBridgeTurnAccess,
  DirectAccountBridgeTurnError,
  type DirectAccountBridgeTurnAccessDeps,
} from '../../src/services/accountBridge/directTurnAccess.js'
import {
  AccountBridgeError,
  resolveAccountBridgePackageRoot,
} from '../../src/services/accountBridge/runtimeManager.js'
import type {
  AccountBridgeModelRouteView,
  AccountBridgeRuntimeAccess,
} from '../../src/services/accountBridge/types.js'
import {
  hasCleanupFinalizers,
  registerCleanup,
  registerCleanupFinalizer,
  runCleanupFunctions,
  runCleanupFunctionsRequiringFinalizers,
} from '../../src/utils/cleanupRegistry.js'

const ROUTE_A = 'A'.repeat(43)
const ROUTE_B = 'B'.repeat(43)
const INFERENCE_KEY = 'C'.repeat(43)

function route(
  routeId = ROUTE_A,
  overrides: Partial<AccountBridgeModelRouteView> = {},
): AccountBridgeModelRouteView {
  return {
    routeId,
    accountId: 'D'.repeat(43),
    connectorId: 'xai',
    modelId: 'provider-test',
    displayName: 'Provider test',
    connectorLabel: 'xAI',
    accountLabel: 'Account',
    chatRuntimeSupported: true,
    supportsTools: true,
    supportsThinking: true,
    supportsAdaptiveThinking: true,
    supportsEffort: true,
    supportsMaxEffort: true,
    supportsVision: false,
    supportsJsonMode: false,
    supportedThinkingModes: ['auto', 'standard', 'deep'],
    defaultThinkingMode: 'auto',
    contextWindow: 128_000,
    maxOutputTokens: 8_192,
    ...overrides,
  }
}

function access(
  routeView = route(),
  inferenceKey = INFERENCE_KEY,
): AccountBridgeRuntimeAccess {
  return {
    endpoint: 'http://127.0.0.1:43123/v1/messages',
    route: routeView,
    inferenceKey,
  }
}

describe('direct Account Bridge turn access', () => {
  test('ordinary explicit and default models never call the bridge', async () => {
    let defaultResolutions = 0
    let bridgeCalls = 0
    const deps: DirectAccountBridgeTurnAccessDeps = {
      resolveDefaultModel() {
        defaultResolutions += 1
        return 'gateway-default'
      },
      async turnAccess() {
        bridgeCalls += 1
        throw new Error('must not run')
      },
    }

    const explicit = await acquireDirectAccountBridgeTurnAccess(
      'gateway-explicit',
      deps,
    )
    expect(explicit.modelForQuery).toBe('gateway-explicit')
    expect(explicit.runtimeAccess).toBeUndefined()

    const implicit = await acquireDirectAccountBridgeTurnAccess(undefined, deps)
    expect(implicit.modelForQuery).toBeUndefined()
    expect(implicit.runtimeAccess).toBeUndefined()
    expect(defaultResolutions).toBe(1)
    expect(bridgeCalls).toBe(0)
  })

  test('revalidates and mints access independently for every account turn', async () => {
    let calls = 0
    const deps: DirectAccountBridgeTurnAccessDeps = {
      resolveDefaultModel: () => 'unused',
      async turnAccess(routeId) {
        calls += 1
        expect(routeId).toBe(ROUTE_A)
        return access(route(), calls === 1 ? INFERENCE_KEY : 'E'.repeat(43))
      },
    }

    const first = await acquireDirectAccountBridgeTurnAccess(
      `account:${ROUTE_A}`,
      deps,
    )
    const second = await acquireDirectAccountBridgeTurnAccess(
      `account:${ROUTE_A}`,
      deps,
    )
    expect(calls).toBe(2)
    expect(first.runtimeAccess?.inferenceKey).toBe(INFERENCE_KEY)
    expect(second.runtimeAccess?.inferenceKey).toBe('E'.repeat(43))
    first.release()
    expect(first.runtimeAccess?.inferenceKey).toBe(INFERENCE_KEY)
    expect(first.runtimeAccess?.endpoint).toBe(
      'http://127.0.0.1:43123/v1/messages',
    )
    expect(second.runtimeAccess?.inferenceKey).toBe('E'.repeat(43))
    second.release()
  })

  test('pins a settings-resolved account model to the authorized route', async () => {
    const lease = await acquireDirectAccountBridgeTurnAccess(undefined, {
      resolveDefaultModel: () => `account:${ROUTE_A}`,
      turnAccess: async () => access(),
    })
    expect(lease.modelForQuery).toBe(`account:${ROUTE_A}`)
    expect(lease.runtimeAccess?.route.routeId).toBe(ROUTE_A)
    lease.release()
  })

  test('propagates manager error identity and never retries through a fallback', async () => {
    const managerError = new AccountBridgeError('eligibility-denied')
    let calls = 0
    try {
      await acquireDirectAccountBridgeTurnAccess(`account:${ROUTE_A}`, {
        resolveDefaultModel: () => 'unused',
        async turnAccess() {
          calls += 1
          throw managerError
        },
      })
      throw new Error('expected acquisition to fail')
    } catch (error) {
      expect(error).toBe(managerError)
      expect((error as AccountBridgeError).code).toBe('eligibility-denied')
      expect(String((error as Error).message)).not.toContain(INFERENCE_KEY)
    }
    expect(calls).toBe(1)
  })

  test('rejects malformed, mismatched and capability-denied routes', async () => {
    let calls = 0
    const deps: DirectAccountBridgeTurnAccessDeps = {
      resolveDefaultModel: () => 'unused',
      async turnAccess() {
        calls += 1
        return access(route(ROUTE_B))
      },
    }
    await expect(
      acquireDirectAccountBridgeTurnAccess('account:not-opaque', deps),
    ).rejects.toMatchObject({
      code: 'invalid-account-route-reference',
    } satisfies Partial<DirectAccountBridgeTurnError>)
    expect(calls).toBe(0)

    await expect(
      acquireDirectAccountBridgeTurnAccess(`account:${ROUTE_A}`, deps),
    ).rejects.toMatchObject({
      code: 'account-route-access-mismatch',
    } satisfies Partial<DirectAccountBridgeTurnError>)

    await expect(
      acquireDirectAccountBridgeTurnAccess(`account:${ROUTE_A}`, {
        resolveDefaultModel: () => 'unused',
        turnAccess: async () =>
          access(route(ROUTE_A, { supportsTools: false })),
      }),
    ).rejects.toMatchObject({
      code: 'account-route-capability-denied',
    } satisfies Partial<DirectAccountBridgeTurnError>)
  })

  test('never creates a provider after process shutdown has started', async () => {
    let defaultResolutions = 0
    let bridgeCalls = 0
    await expect(
      acquireDirectAccountBridgeTurnAccess(`account:${ROUTE_A}`, {
        resolveDefaultModel() {
          defaultResolutions += 1
          return `account:${ROUTE_A}`
        },
        async turnAccess() {
          bridgeCalls += 1
          return access()
        },
        isProcessShuttingDown: () => true,
      }),
    ).rejects.toMatchObject({
      code: 'runtime-process-shutting-down',
    } satisfies Partial<DirectAccountBridgeTurnError>)
    expect(defaultResolutions).toBe(0)
    expect(bridgeCalls).toBe(0)
  })

  test('shutdown beginning during async default-model resolution cannot create a provider', async () => {
    let resolveDefault!: (model: string) => void
    const defaultModel = new Promise<string>(resolve => {
      resolveDefault = resolve
    })
    let shuttingDown = false
    let bridgeCalls = 0
    const acquiring = acquireDirectAccountBridgeTurnAccess(undefined, {
      resolveDefaultModel: () => defaultModel,
      isProcessShuttingDown: () => shuttingDown,
      async turnAccess() {
        bridgeCalls += 1
        throw new Error('provider must not be created after shutdown starts')
      },
    })

    await Promise.resolve()
    shuttingDown = true
    resolveDefault(`account:${ROUTE_A}`)

    await expect(acquiring).rejects.toMatchObject({
      code: 'runtime-process-shutting-down',
    } satisfies Partial<DirectAccountBridgeTurnError>)
    expect(bridgeCalls).toBe(0)
  })
})

describe('Account Bridge package-root layouts', () => {
  test('accepts only the native TUI release and raw-source layouts', () => {
    expect(
      resolveAccountBridgePackageRoot(
        'file:///opt/crabcode/dist/tui-runtime/index.js',
      ),
    ).toBe('/opt/crabcode')
    expect(
      resolveAccountBridgePackageRoot(
        'file:///workspace/src/services/accountBridge/runtimeManager.ts',
      ),
    ).toBe('/workspace')
  })

  test('rejects unknown layouts instead of searching PATH or cwd', () => {
    for (const moduleUrl of [
      'file:///tmp/random/index.js',
      'file:///opt/crabcode/dist/index.js',
      'file:///opt/crabcode/runtime/worker/index.js',
      'file:///opt/crabcode/dist/worker/index.js',
      'file:///opt/crabcode/dist/arbitrary/index.js',
    ]) {
      expect(() => resolveAccountBridgePackageRoot(moduleUrl)).toThrow(
        'artifact-layout-invalid',
      )
    }
  })
})

test('provider finalizers run after ordinary cleanup consumers settle', async () => {
  const order: string[] = []
  const unregisterConsumer = registerCleanup(async () => {
    await Promise.resolve()
    order.push('consumer')
  })
  const unregisterProvider = registerCleanupFinalizer(async () => {
    order.push('provider')
  })
  try {
    await runCleanupFunctions()
    expect(order).toEqual(['consumer', 'provider'])
  } finally {
    unregisterConsumer()
    unregisterProvider()
  }
})

test('provider finalizers still run when an ordinary cleanup rejects', async () => {
  const order: string[] = []
  const failure = new Error('consumer cleanup failed')
  const unregisterConsumer = registerCleanup(async () => {
    order.push('consumer')
    throw failure
  })
  const unregisterProvider = registerCleanupFinalizer(async () => {
    order.push('provider')
  })
  try {
    await expect(runCleanupFunctions()).rejects.toBe(failure)
    expect(order).toEqual(['consumer', 'provider'])
  } finally {
    unregisterConsumer()
    unregisterProvider()
  }
})

test('required provider shutdown ignores consumer failure but awaits the finalizer', async () => {
  const order: string[] = []
  let finishProvider!: () => void
  const providerGate = new Promise<void>(resolve => {
    finishProvider = resolve
  })
  const unregisterConsumer = registerCleanup(async () => {
    order.push('consumer')
    throw new Error('best-effort consumer failure')
  })
  const unregisterProvider = registerCleanupFinalizer(async () => {
    order.push('provider-start')
    await providerGate
    order.push('provider-finished')
  })
  try {
    expect(hasCleanupFinalizers()).toBe(true)
    let settled = false
    const shutdown = runCleanupFunctionsRequiringFinalizers().then(() => {
      settled = true
    })
    await Promise.resolve()
    await Promise.resolve()
    expect(order).toEqual(['consumer', 'provider-start'])
    expect(settled).toBe(false)

    finishProvider()
    await shutdown
    expect(order).toEqual([
      'consumer',
      'provider-start',
      'provider-finished',
    ])
    expect(settled).toBe(true)
  } finally {
    unregisterConsumer()
    unregisterProvider()
  }
  expect(hasCleanupFinalizers()).toBe(false)
})

test('required provider shutdown propagates finalizer failure', async () => {
  const providerFailure = new Error('provider teardown failed')
  const unregisterProvider = registerCleanupFinalizer(async () => {
    throw providerFailure
  })
  try {
    await expect(
      runCleanupFunctionsRequiringFinalizers(),
    ).rejects.toBe(providerFailure)
  } finally {
    unregisterProvider()
  }
})
