import { afterAll, describe, expect, mock, test } from 'bun:test'

import type {
  SecureStorage,
  SecureStorageData,
} from '../../src/utils/secureStorage/types.js'
import { createTestDir } from '../setup.js'

const previousConfigDir = process.env.CRABCODE_CONFIG_DIR
process.env.CRABCODE_CONFIG_DIR = createTestDir('oauth-force-refresh')
afterAll(() => {
  if (previousConfigDir === undefined) delete process.env.CRABCODE_CONFIG_DIR
  else process.env.CRABCODE_CONFIG_DIR = previousConfigDir
})

let store: SecureStorageData | null = null
let refreshCalls: (string | undefined)[] = []
let failStorageWrites = false
let refreshImpl: (
  currentAccessToken: string | undefined,
) => Promise<Record<string, unknown>> = async currentAccessToken => ({
  accessToken: `renewed-for-${currentAccessToken}`,
  refreshToken: 'refresh-token-next',
  expiresAt: Date.now() + 3_600_000,
  scopes: ['ai'],
})

const storage: SecureStorage = {
  name: 'in-memory-test',
  read: () => store,
  readAsync: async () => store,
  update: data => {
    store = data
    return { success: true }
  },
  updateAsync: async data => {
    store = data
    return { success: true }
  },
  mutateAsync: async mutation => {
    if (failStorageWrites) {
      return { success: false, committed: false, warning: 'write rejected' }
    }
    store = await mutation(structuredClone(store ?? {}))
    return { success: true }
  },
  delete: () => {
    store = null
    return true
  },
  deleteAsync: async () => {
    store = null
    return true
  },
}

mock.module('../../src/utils/secureStorage/index.js', () => ({
  getSecureStorage: () => storage,
  mutateSecureStorageWithPrimaryPlaintextCleanup: (
    mutation: (current: SecureStorageData) => SecureStorageData,
  ) => storage.mutateAsync(mutation),
}))

const realOAuthClient = await import('../../src/services/oauth/client.js')
mock.module('../../src/services/oauth/client.js', () => ({
  ...realOAuthClient,
  renewStoredOAuthTokens: async (currentAccessToken?: string) => {
    refreshCalls.push(currentAccessToken)
    return refreshImpl(currentAccessToken)
  },
}))

const { checkAndRefreshOAuthTokenIfNeeded, clearOAuthTokenCache } =
  await import('../../src/utils/auth.js')

const HOUR = 3_600_000

function seed(expiresAt: number): void {
  store = {
    acosmiOauth: {
      accessToken: 'server-already-rejected-this',
      refreshToken: 'refresh-token-1',
      expiresAt,
      scopes: ['ai'],
    },
  }
  refreshCalls = []
  failStorageWrites = false
  clearOAuthTokenCache()
}

function restoreRefreshImpl(): void {
  refreshImpl = async currentAccessToken => ({
    accessToken: `renewed-for-${currentAccessToken}`,
    refreshToken: 'refresh-token-next',
    expiresAt: Date.now() + HOUR,
    scopes: ['ai'],
  })
}

describe('force refresh survives every local expiry shortcut', () => {
  test('a locally valid token rejected by the server is renewed once', async () => {
    seed(Date.now() + HOUR)
    expect(await checkAndRefreshOAuthTokenIfNeeded(0, true)).toBe('refreshed')
    expect(refreshCalls).toEqual(['server-already-rejected-this'])
  })

  test('without force the same locally valid state performs no renewal', async () => {
    seed(Date.now() + HOUR)
    expect(await checkAndRefreshOAuthTokenIfNeeded()).toBe('temporary_failure')
    expect(refreshCalls).toEqual([])
  })

  test('normal expiry still renews through the same rotator', async () => {
    seed(Date.now() - HOUR)
    expect(await checkAndRefreshOAuthTokenIfNeeded()).toBe('refreshed')
    expect(refreshCalls).toEqual(['server-already-rejected-this'])
  })

  test('the renewed credential is persisted', async () => {
    seed(Date.now() + HOUR)
    await checkAndRefreshOAuthTokenIfNeeded(0, true)
    const persisted = store?.acosmiOauth as { accessToken: string }
    expect(persisted.accessToken).toBe(
      'renewed-for-server-already-rejected-this',
    )
  })
})

describe('failed force refresh is not reported as a recovery', () => {
  test('an uncommitted secure-storage failure is not reported as refreshed', async () => {
    seed(Date.now() + HOUR)
    failStorageWrites = true
    try {
      expect(await checkAndRefreshOAuthTokenIfNeeded(0, true)).toBe(
        'temporary_failure',
      )
      expect(
        (store?.acosmiOauth as { accessToken?: string }).accessToken,
      ).toBe('server-already-rejected-this')
    } finally {
      failStorageWrites = false
    }
  })

  test('an unchanged token remains a failure', async () => {
    seed(Date.now() + HOUR)
    refreshImpl = async () => {
      throw new Error('network down')
    }
    try {
      expect(await checkAndRefreshOAuthTokenIfNeeded(0, true)).not.toBe(
        'refreshed',
      )
      expect(refreshCalls).toEqual(['server-already-rejected-this'])
    } finally {
      restoreRefreshImpl()
    }
  })

  test('a different token written concurrently is a real recovery', async () => {
    seed(Date.now() + HOUR)
    refreshImpl = async () => {
      store = {
        acosmiOauth: {
          accessToken: 'written-by-another-process',
          refreshToken: 'refresh-token-2',
          expiresAt: Date.now() + HOUR,
          scopes: ['ai'],
        },
      }
      throw new Error('network down')
    }
    try {
      expect(await checkAndRefreshOAuthTokenIfNeeded(0, true)).toBe('refreshed')
    } finally {
      restoreRefreshImpl()
    }
  })
})
