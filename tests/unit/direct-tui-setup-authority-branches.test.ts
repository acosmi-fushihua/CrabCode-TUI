import {
  afterEach,
  describe,
  expect,
  mock,
  spyOn,
  test,
} from 'bun:test'
import type { ZodType } from 'zod/v4'

import * as bootstrapState from '../../src/bootstrap/state.js'
import {
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
  type CrabCodeTuiSetupRequest,
} from '../../src/cli/crabcodeTuiBridgeProtocol.js'
import {
  runDirectTuiPostTrustSetup,
  runDirectTuiPreTrustOnboarding,
  type DirectTuiSetupRequester,
} from '../../src/cli/directTuiSetupLifecycle.js'
import * as i18n from '../../src/i18n/index.js'
import * as growthbook from '../../src/services/analytics/growthbook.js'
import * as acosmiClient from '../../src/services/acosmi/client.js'
import * as installOAuth from '../../src/services/auth/installOAuthTokens.js'
import * as localAuthState from '../../src/services/auth/localAuthState.js'
import * as channelAllowlist from '../../src/services/mcp/channelAllowlist.js'
import * as mcpConfig from '../../src/services/mcp/config.js'
import * as mcpUtils from '../../src/services/mcp/utils.js'
import * as auth from '../../src/utils/auth.js'
import * as config from '../../src/utils/config.js'
import * as crabcodemd from '../../src/utils/crabcodemd.js'
import { env } from '../../src/utils/env.js'
import * as envUtils from '../../src/utils/envUtils.js'
import * as featureFlags from '../../src/utils/featurePolyfill.js'
import * as hooks from '../../src/utils/hooks.js'
import * as preflight from '../../src/utils/preflightChecksCore.js'
import * as settingsErrors from '../../src/utils/settings/allErrors.js'
import * as settings from '../../src/utils/settings/settings.js'

type SetupResponse = Record<string, unknown>

const originalApiKey = process.env.ACOSMI_API_KEY
const originalClaubbit = process.env.CLAUBBIT
const originalTerminal = env.terminal
const originalLocale = i18n.getLocale()

afterEach(() => {
  mock.restore()
  if (originalApiKey === undefined) {
    delete process.env.ACOSMI_API_KEY
  } else {
    process.env.ACOSMI_API_KEY = originalApiKey
  }
  if (originalClaubbit === undefined) {
    delete process.env.CLAUBBIT
  } else {
    process.env.CLAUBBIT = originalClaubbit
  }
  env.terminal = originalTerminal
  i18n.setLocale(originalLocale)
})

function branchId(request: CrabCodeTuiSetupRequest): string {
  if (request.kind === 'onboarding') {
    return [
      request.kind,
      request.stage,
      'phase' in request ? request.phase : undefined,
    ]
      .filter(Boolean)
      .join('/')
  }
  return [
    request.kind,
    'phase' in request ? request.phase : undefined,
  ]
    .filter(Boolean)
    .join('/')
}

function response(
  request: CrabCodeTuiSetupRequest,
  body: Record<string, unknown>,
): SetupResponse {
  return {
    protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
    kind: request.kind,
    ...(request.kind === 'onboarding'
      ? {
          stage: request.stage,
          ...('phase' in request ? { phase: request.phase } : {}),
        }
      : {}),
    ...('phase' in request && request.kind !== 'onboarding'
      ? { phase: request.phase }
      : {}),
    ...body,
  }
}

function executingRequester(
  requests: CrabCodeTuiSetupRequest[],
  choose: (
    request: CrabCodeTuiSetupRequest,
  ) => SetupResponse | Promise<SetupResponse>,
): DirectTuiSetupRequester {
  return async function requestSetup<Response>(
    request: CrabCodeTuiSetupRequest,
    responseSchema: ZodType<Response>,
  ): Promise<Response> {
    requests.push(request)
    return responseSchema.parse(await choose(request))
  }
}

function installConfigAuthority(initial: Record<string, unknown>): {
  current(): Record<string, unknown>
  projectWrites: Record<string, unknown>[]
} {
  let current = {
    numStartups: 0,
    theme: 'dark',
    preferredNotifChannel: 'auto',
    verbose: false,
    autoCompactEnabled: true,
    ...initial,
  } as ReturnType<typeof config.getGlobalConfig>
  const projectWrites: Record<string, unknown>[] = []
  spyOn(config, 'getGlobalConfig').mockImplementation(() => current)
  spyOn(config, 'saveGlobalConfig').mockImplementation(updater => {
    current = updater(current)
  })
  spyOn(config, 'saveCurrentProjectConfig').mockImplementation(updater => {
    projectWrites.push(
      updater({}) as unknown as Record<string, unknown>,
    )
  })
  return {
    current: () => current as unknown as Record<string, unknown>,
    projectWrites,
  }
}

function installSharedSettingsAuthority(
  initial: Record<string, unknown> = {},
): Array<{ source: string; value: Record<string, unknown> }> {
  const writes: Array<{ source: string; value: Record<string, unknown> }> = []
  spyOn(settings, 'getInitialSettings').mockImplementation(
    () => initial as ReturnType<typeof settings.getInitialSettings>,
  )
  spyOn(settings, 'updateSettingsForSource').mockImplementation(
    (source, value) => {
      writes.push({
        source,
        value: value as unknown as Record<string, unknown>,
      })
      return { error: null }
    },
  )
  return writes
}

function installDirectOAuthStorageOutcome(
  storageResult: Awaited<ReturnType<typeof installOAuth.installOAuthTokens>>,
  tokenOverrides: Partial<{
    refreshToken: string | null
    expiresAt: number | null
  }> = {},
  forceLoginMethod?: 'acosmi' | 'console',
): ReturnType<typeof installConfigAuthority> & {
  installedTokens: string[]
  notificationTypes: string[]
} {
  delete process.env.ACOSMI_API_KEY
  env.terminal = null
  const configAuthority = installConfigAuthority({
    theme: undefined,
    hasCompletedOnboarding: false,
  })
  installSharedSettingsAuthority({
    forceLoginMethod,
    syntaxHighlightingDisabled: false,
  })
  spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(true)
  spyOn(auth, 'validateForceLoginOrg').mockResolvedValue({ valid: true })
  spyOn(preflight, 'checkOnboardingEndpoints').mockResolvedValue({
    success: true,
  })
  spyOn(acosmiClient, 'loginStream').mockImplementation(async function* () {
    yield { type: 'complete' }
  })
  spyOn(acosmiClient, 'getAuthStatus').mockResolvedValue({
    authorized: true,
    tokens: {
      accessToken: 'synthetic-persistence-secret-marker',
      refreshToken: 'synthetic-refresh-secret-marker',
      expiresAt: Date.now() + 60_000,
      scopes: ['ai'],
      ...tokenOverrides,
    },
    hasProfileScope: false,
    isSubscriber: true,
  })
  const installedTokens: string[] = []
  spyOn(installOAuth, 'installOAuthTokens').mockImplementation(async tokens => {
    installedTokens.push(tokens.accessToken)
    return storageResult
  })
  const notificationTypes: string[] = []
  spyOn(hooks, 'executeNotificationHooks').mockImplementation(async input => {
    notificationTypes.push(input.notificationType)
  })
  return {
    ...configAuthority,
    installedTokens,
    notificationTypes,
  }
}

describe('item-specific direct-TUI setup authorities', () => {
  test('completed onboarding requires both OAuth stores or an existing non-OAuth authority', async () => {
    i18n.setLocale('zh-CN')
    delete process.env.ACOSMI_API_KEY
    env.terminal = null
    installConfigAuthority({
      theme: 'dark',
      hasCompletedOnboarding: true,
    })
    const authEnabled = spyOn(
      auth,
      'isAcosmiAuthEnabled',
    ).mockReturnValue(true)
    let secureOAuthAvailable = false
    spyOn(auth, 'isAcosmiSubscriber').mockImplementation(
      () => secureOAuthAvailable,
    )
    const apiKeyAuth = spyOn(
      auth,
      'hasAcosmiApiKeyAuth',
    ).mockReturnValue(false)
    let sdkOAuthAvailable = false
    const getStatus = spyOn(
      acosmiClient,
      'getAuthStatus',
    ).mockImplementation(async () => ({
      authorized: sdkOAuthAvailable,
      tokens: sdkOAuthAvailable
        ? {
            accessToken: 'stored-access-token',
            refreshToken: 'stored-refresh-token',
            expiresAt: Date.now() + 60_000,
            scopes: ['ai'],
          }
        : null,
      hasProfileScope: false,
      isSubscriber: false,
    }))
    const expectOnboardingReopened = async (): Promise<void> => {
      const requests: CrabCodeTuiSetupRequest[] = []
      await expect(
        runDirectTuiPreTrustOnboarding(
          executingRequester(requests, request => {
            throw new Error(`onboarding reopened at ${branchId(request)}`)
          }),
          { shouldSkipSetup: () => false },
        ),
      ).rejects.toThrow('onboarding reopened at onboarding/language')
      expect(requests.map(branchId)).toEqual(['onboarding/language'])
    }

    await expectOnboardingReopened()
    sdkOAuthAvailable = true
    await expectOnboardingReopened()
    sdkOAuthAvailable = false
    secureOAuthAvailable = true
    await expectOnboardingReopened()

    sdkOAuthAvailable = true
    const completeOauthRequests: CrabCodeTuiSetupRequest[] = []
    await expect(
      runDirectTuiPreTrustOnboarding(
        executingRequester(completeOauthRequests, request => {
          throw new Error(`unexpected setup branch ${branchId(request)}`)
        }),
        { shouldSkipSetup: () => false },
      ),
    ).resolves.toEqual({ onboardingShown: false })
    expect(completeOauthRequests).toEqual([])

    sdkOAuthAvailable = false
    secureOAuthAvailable = false
    apiKeyAuth.mockReturnValue(true)
    const apiKeyRequests: CrabCodeTuiSetupRequest[] = []
    await expect(
      runDirectTuiPreTrustOnboarding(
        executingRequester(apiKeyRequests, request => {
          throw new Error(`unexpected setup branch ${branchId(request)}`)
        }),
        { shouldSkipSetup: () => false },
      ),
    ).resolves.toEqual({ onboardingShown: false })
    expect(apiKeyRequests).toEqual([])

    authEnabled.mockReturnValue(false)
    apiKeyAuth.mockReturnValue(false)
    const externalProviderRequests: CrabCodeTuiSetupRequest[] = []
    await expect(
      runDirectTuiPreTrustOnboarding(
        executingRequester(externalProviderRequests, request => {
          throw new Error(`unexpected setup branch ${branchId(request)}`)
        }),
        { shouldSkipSetup: () => false },
      ),
    ).resolves.toEqual({ onboardingShown: false })
    expect(externalProviderRequests).toEqual([])
    expect(getStatus).toHaveBeenCalledTimes(4)
  })

  test('the shared installer rejects incomplete durable tokens before clearing prior auth', async () => {
    i18n.setLocale('zh-CN')
    const clear = spyOn(
      localAuthState,
      'clearLocalAuthState',
    ).mockResolvedValue()

    await expect(
      installOAuth.installOAuthTokens({
        accessToken: 'incomplete-token',
        refreshToken: null,
        expiresAt: null,
        scopes: ['ai'],
        subscriptionType: null,
        rateLimitTier: null,
        membershipActive: null,
      }),
    ).rejects.toThrow(
      '登录凭据未能安全保存，请检查系统凭据存储或配置目录权限后重试。',
    )
    expect(clear).not.toHaveBeenCalled()
  })

  test('executes language, failed preflight, theme, onboarding API-key, security, and terminal authorities', async () => {
    i18n.setLocale('zh-CN')
    process.env.ACOSMI_API_KEY = 'authority-branch-key'
    // Use a terminal supported on every host so this authority test does not
    // accidentally depend on the CI runner's process.platform value.
    env.terminal = 'vscode'
    const configAuthority = installConfigAuthority({
      theme: undefined,
      hasCompletedOnboarding: false,
    })
    const settingsWrites = installSharedSettingsAuthority({
      syntaxHighlightingDisabled: false,
    })
    spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(true)
    spyOn(auth, 'getCustomApiKeyStatus').mockReturnValue('new')
    spyOn(envUtils, 'isRunningOnHomespace').mockReturnValue(false)
    spyOn(preflight, 'checkOnboardingEndpoints').mockResolvedValue({
      success: false,
      error: 'offline',
      sslHint: 'check TLS',
    })
    const selectedLocales: string[] = []
    const applyLocale = i18n.setLocale
    spyOn(i18n, 'setLocale').mockImplementation(locale => {
      selectedLocales.push(locale)
      applyLocale(locale)
    })
    const installedThemes: string[] = []
    const requests: CrabCodeTuiSetupRequest[] = []

    const result = await runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, {
              decision: 'select',
              locale: 'en-US',
            })
          case 'onboarding/preflight':
            return response(request, { decision: 'rendered' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'light',
              syntax_highlighting_disabled: true,
            })
          case 'api_key_approval':
            return response(request, { decision: 'accept' })
          case 'onboarding/security':
            return response(request, { decision: 'continue' })
          case 'onboarding/terminal':
            return response(request, { decision: 'install' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      {
        shouldSkipSetup: () => false,
        async installTerminal(theme) {
          installedThemes.push(theme)
        },
      },
    )

    expect(result).toEqual({ onboardingShown: true })
    expect(requests.map(branchId)).toEqual([
      'onboarding/language',
      'onboarding/preflight',
      'onboarding/theme',
      'api_key_approval',
      'onboarding/security',
      'onboarding/terminal',
    ])
    expect(requests[0]).toMatchObject({
      title: '选择界面语言 / Select interface language',
      body: ['Enter 确认 · Esc 跳过（默认中文）'],
    })
    expect(
      requests.find(request => branchId(request) === 'onboarding/preflight'),
    ).toMatchObject({
      title: 'Unable to connect to Acosmi services',
      error: 'offline',
      ssl_hint: 'check TLS',
    })
    expect(
      requests.find(request => branchId(request) === 'onboarding/theme'),
    ).toMatchObject({
      title: 'Choose your preferred theme',
      options: expect.arrayContaining([
        { value: 'light', label: 'Light mode' },
      ]),
    })
    expect(selectedLocales).toEqual(['en-US'])
    expect(installedThemes).toEqual(['light'])
    expect(configAuthority.current()).toMatchObject({
      uiLanguage: 'en-US',
      theme: 'light',
      hasCompletedOnboarding: true,
      customApiKeyResponses: {
        approved: ['authority-branch-key'],
      },
    })
    expect(settingsWrites).toContainEqual({
      source: 'userSettings',
      value: { syntaxHighlightingDisabled: true },
    })
  })

  test('defaults the post-language onboarding copy to Chinese', async () => {
    i18n.setLocale('zh-CN')
    delete process.env.ACOSMI_API_KEY
    env.terminal = null
    const configAuthority = installConfigAuthority({
      theme: undefined,
      hasCompletedOnboarding: false,
    })
    installSharedSettingsAuthority({
      syntaxHighlightingDisabled: false,
    })
    spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(false)
    const requests: CrabCodeTuiSetupRequest[] = []

    await runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, {
              decision: 'select',
              locale: 'zh-CN',
            })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/security':
            return response(request, { decision: 'continue' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      {
        shouldSkipSetup: () => false,
      },
    )

    expect(
      requests.find(request => branchId(request) === 'onboarding/theme'),
    ).toMatchObject({
      title: '选择偏好的主题',
      options: expect.arrayContaining([
        { value: 'dark', label: '深色模式' },
        { value: 'light', label: '浅色模式' },
      ]),
    })
    expect(
      requests.find(request => branchId(request) === 'onboarding/security'),
    ).toMatchObject({
      title: '安全须知：',
    })
  })

  test('executes OAuth method selection, platform information, custom-provider, and success authorities', async () => {
    delete process.env.ACOSMI_API_KEY
    env.terminal = null
    installConfigAuthority({
      theme: undefined,
      hasCompletedOnboarding: false,
    })
    installSharedSettingsAuthority({
      forceLoginMethod: undefined,
      syntaxHighlightingDisabled: false,
    })
    spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(true)
    spyOn(preflight, 'checkOnboardingEndpoints').mockResolvedValue({
      success: true,
    })
    const requests: CrabCodeTuiSetupRequest[] = []
    const configured: Array<Record<string, string>> = []
    let methodSelections = 0

    await runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, { decision: 'skip' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/oauth/select_method':
            methodSelections += 1
            return response(request, {
              decision: 'select',
              method: methodSelections === 1 ? 'platform' : 'console',
            })
          case 'onboarding/oauth/platform_setup':
            return response(request, { decision: 'continue' })
          case 'onboarding/oauth/custom_provider':
            return response(request, {
              decision: 'select',
            })
          case 'onboarding/oauth/custom_base_url':
            return response(request, {
              decision: 'submit',
              base_url: 'https://models.example.test/v1',
            })
          case 'onboarding/oauth/custom_model_id':
            return response(request, {
              decision: 'submit',
              model_id: 'authority-model',
            })
          case 'onboarding/oauth/custom_api_key':
            return response(request, {
              decision: 'submit',
              api_key: 'authority-secret',
            })
          case 'onboarding/oauth/success':
            return response(request, { decision: 'continue' })
          case 'onboarding/security':
            return response(request, { decision: 'continue' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      {
        shouldSkipSetup: () => false,
        async configureCustomModel(input) {
          configured.push(input)
        },
      },
    )

    expect(requests.map(branchId)).toEqual([
      'onboarding/language',
      'onboarding/theme',
      'onboarding/oauth/select_method',
      'onboarding/oauth/platform_setup',
      'onboarding/oauth/select_method',
      'onboarding/oauth/custom_provider',
      'onboarding/oauth/custom_base_url',
      'onboarding/oauth/custom_model_id',
      'onboarding/oauth/custom_api_key',
      'onboarding/oauth/success',
      'onboarding/security',
    ])
    expect(
      requests.find(
        request => branchId(request) === 'onboarding/oauth/platform_setup',
      ),
    ).toMatchObject({
      links: [
        { label: expect.any(String), url: expect.any(String) },
        { label: expect.any(String), url: expect.any(String) },
        { label: expect.any(String), url: expect.any(String) },
      ],
    })
    expect(configured).toEqual([
      {
        provider: 'openai-compatible',
        baseUrl: 'https://models.example.test/v1',
        modelId: 'authority-model',
        apiKey: 'authority-secret',
      },
    ])
  })

  test('the default custom-model authority retries instead of completing after a settings write error', async () => {
    i18n.setLocale('zh-CN')
    delete process.env.ACOSMI_API_KEY
    env.terminal = null
    const configAuthority = installConfigAuthority({
      theme: undefined,
      hasCompletedOnboarding: false,
    })
    spyOn(settings, 'getInitialSettings').mockReturnValue({
      syntaxHighlightingDisabled: false,
    } as ReturnType<typeof settings.getInitialSettings>)
    spyOn(settings, 'updateSettingsForSource').mockImplementation(
      (_source, value) => ({
        error:
          value.customModel === undefined
            ? null
            : new Error('synthetic custom settings write failure'),
      }),
    )
    spyOn(auth, 'saveApiKey').mockResolvedValue()
    spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(true)
    spyOn(preflight, 'checkOnboardingEndpoints').mockResolvedValue({
      success: true,
    })

    const requests: CrabCodeTuiSetupRequest[] = []
    let apiKeyRequests = 0
    const run = runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, { decision: 'skip' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/oauth/select_method':
            return response(request, {
              decision: 'select',
              method: 'console',
            })
          case 'onboarding/oauth/custom_provider':
            return response(request, { decision: 'select' })
          case 'onboarding/oauth/custom_base_url':
            return response(request, {
              decision: 'submit',
              base_url: 'https://models.example.test/v1',
            })
          case 'onboarding/oauth/custom_model_id':
            return response(request, {
              decision: 'submit',
              model_id: 'custom-model',
            })
          case 'onboarding/oauth/custom_api_key':
            apiKeyRequests += 1
            if (apiKeyRequests === 2) {
              throw new Error('stop-after-custom-settings-retry')
            }
            return response(request, {
              decision: 'submit',
              api_key: 'custom-secret',
            })
          case 'onboarding/oauth/error':
            throw new Error('stop-after-custom-settings-retry')
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      { shouldSkipSetup: () => false },
    )

    await expect(run).rejects.toThrow('stop-after-custom-settings-retry')
    const retry = requests.filter(
      request => branchId(request) === 'onboarding/oauth/custom_api_key',
    )[1]
    expect(retry).toMatchObject({
      error: 'synthetic custom settings write failure',
    })
    expect(JSON.stringify(retry)).not.toContain('custom-secret')
    expect(requests.map(branchId)).not.toContain('onboarding/oauth/success')
    expect(requests.map(branchId)).not.toContain('onboarding/security')
    expect(configAuthority.current().hasCompletedOnboarding).toBe(false)
  })

  test('executes OAuth browser URL, browser-open-failed, token install, and success authorities', async () => {
    delete process.env.ACOSMI_API_KEY
    env.terminal = null
    const configAuthority = installConfigAuthority({
      theme: undefined,
      hasCompletedOnboarding: false,
    })
    installSharedSettingsAuthority({
      forceLoginMethod: undefined,
      syntaxHighlightingDisabled: false,
    })
    spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(true)
    spyOn(auth, 'validateForceLoginOrg').mockResolvedValue({ valid: true })
    spyOn(preflight, 'checkOnboardingEndpoints').mockResolvedValue({
      success: true,
    })
    spyOn(acosmiClient, 'loginStream').mockImplementation(async function* () {
      yield {
        type: 'auth_url',
        url: 'https://acosmi.test/authorize',
      }
      yield {
        type: 'error',
        err_code: 'browser_open_failed',
      }
      yield { type: 'complete' }
    })
    spyOn(acosmiClient, 'getAuthStatus').mockResolvedValue({
      authorized: true,
      tokens: {
        accessToken: 'access-token',
        refreshToken: 'synthetic-refresh-token',
        expiresAt: Date.now() + 60_000,
        // SDK 2.12.0 projects an RFC 6749 §5.1 omitted scope as [].
        scopes: [],
        clientId: 'client',
        serverUrl: 'https://acosmi.test',
      },
      hasProfileScope: false,
      isSubscriber: true,
    })
    const installedTokens: string[] = []
    const installedScopes: string[][] = []
    spyOn(installOAuth, 'installOAuthTokens').mockImplementation(
      async tokens => {
        installedTokens.push(tokens.accessToken)
        installedScopes.push(tokens.scopes)
        return { success: true }
      },
    )
    const notifications: string[] = []
    spyOn(hooks, 'executeNotificationHooks').mockImplementation(
      async input => {
        notifications.push(input.notificationType)
      },
    )
    const requests: CrabCodeTuiSetupRequest[] = []

    await runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, { decision: 'skip' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/oauth/select_method':
            return response(request, {
              decision: 'select',
              method: 'acosmi',
            })
          case 'onboarding/oauth/browser_url':
          case 'onboarding/oauth/browser_open_failed':
            return response(request, { decision: 'rendered' })
          case 'onboarding/oauth/success':
            return response(request, { decision: 'continue' })
          case 'onboarding/security':
            return response(request, { decision: 'continue' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      { shouldSkipSetup: () => false },
    )

    expect(requests.map(branchId)).toEqual([
      'onboarding/language',
      'onboarding/theme',
      'onboarding/oauth/select_method',
      'onboarding/oauth/browser_url',
      'onboarding/oauth/browser_open_failed',
      'onboarding/oauth/success',
      'onboarding/security',
    ])
    expect(
      requests.find(
        request => branchId(request) === 'onboarding/oauth/browser_url',
      ),
    ).toMatchObject({
      url: 'https://acosmi.test/authorize',
      body: [expect.any(String), expect.any(String)],
    })
    expect(
      requests.find(
        request =>
          branchId(request) === 'onboarding/oauth/browser_open_failed',
      ),
    ).toEqual({
      subtype: 'crabcode_tui_setup',
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'oauth',
      phase: 'browser_open_failed',
    })
    expect(installedTokens).toEqual(['access-token'])
    expect(installedScopes).toEqual([['ai', 'skills', 'account']])
    expect(notifications).toEqual(['auth_success'])
    expect(configAuthority.current().hasCompletedOnboarding).toBe(true)
  })

  test('fails closed when OAuth secure storage neither succeeds nor commits', async () => {
    i18n.setLocale('zh-CN')
    const configAuthority = installDirectOAuthStorageOutcome({
      success: false,
      committed: false,
      warning: 'synthetic storage failure',
    })
    const requests: CrabCodeTuiSetupRequest[] = []
    const run = runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, { decision: 'skip' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/oauth/select_method':
            if (
              requests.filter(
                item =>
                  branchId(item) === 'onboarding/oauth/select_method',
              ).length > 1
            ) {
              throw new Error('stop-after-persistence-retry')
            }
            return response(request, {
              decision: 'select',
              method: 'acosmi',
            })
          case 'onboarding/oauth/error':
            return response(request, { decision: 'retry' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      { shouldSkipSetup: () => false },
    )

    await expect(run).rejects.toThrow('stop-after-persistence-retry')
    const branchIds = requests.map(branchId)
    expect(branchIds).toContain('onboarding/oauth/error')
    expect(branchIds).not.toContain('onboarding/oauth/success')
    expect(branchIds).not.toContain('onboarding/security')
    expect(configAuthority.current().hasCompletedOnboarding).toBe(false)
    expect(configAuthority.installedTokens).toEqual([
      'synthetic-persistence-secret-marker',
    ])
    expect(configAuthority.notificationTypes).toEqual([])
    const errorRequest = requests.find(
      request => branchId(request) === 'onboarding/oauth/error',
    )
    expect(errorRequest).toMatchObject({
      error:
        '登录凭据未能安全保存，请检查系统凭据存储或配置目录权限后重试。',
    })
    expect(JSON.stringify(errorRequest)).not.toContain(
      'synthetic-persistence-secret-marker',
    )
  })

  test('continues OAuth onboarding after an authoritative committed write with cleanup warning', async () => {
    const configAuthority = installDirectOAuthStorageOutcome({
      success: false,
      committed: true,
      warning: 'synthetic post-commit cleanup failure',
    })
    const requests: CrabCodeTuiSetupRequest[] = []

    await runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, { decision: 'skip' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/oauth/select_method':
            return response(request, {
              decision: 'select',
              method: 'acosmi',
            })
          case 'onboarding/oauth/success':
            return response(request, { decision: 'continue' })
          case 'onboarding/security':
            return response(request, { decision: 'continue' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      { shouldSkipSetup: () => false },
    )

    const branchIds = requests.map(branchId)
    expect(branchIds).toContain('onboarding/oauth/success')
    expect(branchIds).not.toContain('onboarding/oauth/error')
    expect(configAuthority.current().hasCompletedOnboarding).toBe(true)
    expect(configAuthority.installedTokens).toEqual([
      'synthetic-persistence-secret-marker',
    ])
    expect(configAuthority.notificationTypes).toEqual(['auth_success'])
  })

  for (const [missingField, tokenOverrides] of [
    [
      'refresh token',
      { refreshToken: null },
    ],
    [
      'expiry',
      { expiresAt: null },
    ],
  ] as const) {
    test(`fails closed before token installation when Acosmi OAuth omits its ${missingField}`, async () => {
      i18n.setLocale('zh-CN')
      const configAuthority = installDirectOAuthStorageOutcome(
        { success: true },
        tokenOverrides,
      )
      const requests: CrabCodeTuiSetupRequest[] = []
      const run = runDirectTuiPreTrustOnboarding(
        executingRequester(requests, request => {
          switch (branchId(request)) {
            case 'onboarding/language':
              return response(request, { decision: 'skip' })
            case 'onboarding/theme':
              return response(request, {
                decision: 'select',
                theme: 'dark',
                syntax_highlighting_disabled: false,
              })
            case 'onboarding/oauth/select_method':
              if (
                requests.filter(
                  item =>
                    branchId(item) === 'onboarding/oauth/select_method',
                ).length > 1
              ) {
                throw new Error(`stop-after-missing-${missingField}`)
              }
              return response(request, {
                decision: 'select',
                method: 'acosmi',
              })
            case 'onboarding/oauth/error':
              return response(request, { decision: 'retry' })
            default:
              throw new Error(`unexpected setup branch ${branchId(request)}`)
          }
        }),
        { shouldSkipSetup: () => false },
      )

      await expect(run).rejects.toThrow(
        `stop-after-missing-${missingField}`,
      )
      const branchIds = requests.map(branchId)
      expect(branchIds).toContain('onboarding/oauth/error')
      expect(branchIds).not.toContain('onboarding/oauth/success')
      expect(branchIds).not.toContain('onboarding/security')
      expect(configAuthority.current().hasCompletedOnboarding).toBe(false)
      expect(configAuthority.installedTokens).toEqual([])
      expect(configAuthority.notificationTypes).toEqual([])
      const errorRequest = requests.find(
        request => branchId(request) === 'onboarding/oauth/error',
      )
      expect(errorRequest).toMatchObject({
        error:
          '登录凭据未能安全保存，请检查系统凭据存储或配置目录权限后重试。',
      })
      const serializedError = JSON.stringify(errorRequest)
      expect(serializedError).not.toContain(
        'synthetic-persistence-secret-marker',
      )
      expect(serializedError).not.toContain(
        'synthetic-refresh-secret-marker',
      )
    })
  }

  for (const [missingField, tokenOverrides] of [
    [
      'refresh token',
      { refreshToken: null },
    ],
    [
      'expiry',
      { expiresAt: null },
    ],
  ] as const) {
    test(`fails closed before token installation when forced Console OAuth omits its ${missingField}`, async () => {
      i18n.setLocale('zh-CN')
      const configAuthority = installDirectOAuthStorageOutcome(
        { success: true },
        tokenOverrides,
        'console',
      )
      const requests: CrabCodeTuiSetupRequest[] = []
      const run = runDirectTuiPreTrustOnboarding(
        executingRequester(requests, request => {
          switch (branchId(request)) {
            case 'onboarding/language':
              return response(request, { decision: 'skip' })
            case 'onboarding/theme':
              return response(request, {
                decision: 'select',
                theme: 'dark',
                syntax_highlighting_disabled: false,
              })
            case 'onboarding/oauth/error':
              if (
                requests.filter(
                  item => branchId(item) === 'onboarding/oauth/error',
                ).length > 1
              ) {
                throw new Error(
                  `stop-after-forced-console-missing-${missingField}`,
                )
              }
              return response(request, { decision: 'retry' })
            default:
              throw new Error(`unexpected setup branch ${branchId(request)}`)
          }
        }),
        { shouldSkipSetup: () => false },
      )

      await expect(run).rejects.toThrow(
        `stop-after-forced-console-missing-${missingField}`,
      )
      const branchIds = requests.map(branchId)
      expect(branchIds).toContain('onboarding/oauth/error')
      expect(branchIds).not.toContain('onboarding/oauth/select_method')
      expect(branchIds).not.toContain('onboarding/oauth/success')
      expect(branchIds).not.toContain('onboarding/security')
      expect(configAuthority.current().hasCompletedOnboarding).toBe(false)
      expect(configAuthority.installedTokens).toEqual([])
      expect(configAuthority.notificationTypes).toEqual([])
      const errorRequest = requests.find(
        request => branchId(request) === 'onboarding/oauth/error',
      )
      expect(errorRequest).toMatchObject({
        error:
          '登录凭据未能安全保存，请检查系统凭据存储或配置目录权限后重试。',
      })
      const serializedError = JSON.stringify(errorRequest)
      expect(serializedError).not.toContain(
        'synthetic-persistence-secret-marker',
      )
      expect(serializedError).not.toContain(
        'synthetic-refresh-secret-marker',
      )
    })
  }

  test('executes the OAuth error authority and retries through the existing method selector', async () => {
    delete process.env.ACOSMI_API_KEY
    env.terminal = null
    installConfigAuthority({
      theme: undefined,
      hasCompletedOnboarding: false,
    })
    installSharedSettingsAuthority({
      forceLoginMethod: undefined,
      syntaxHighlightingDisabled: false,
    })
    spyOn(auth, 'isAcosmiAuthEnabled').mockReturnValue(true)
    spyOn(auth, 'validateForceLoginOrg').mockResolvedValue({ valid: true })
    spyOn(preflight, 'checkOnboardingEndpoints').mockResolvedValue({
      success: true,
    })
    let loginAttempt = 0
    spyOn(acosmiClient, 'loginStream').mockImplementation(async function* () {
      loginAttempt += 1
      if (loginAttempt === 1) {
        yield {
          type: 'error',
          err_code: 'auth_denied',
        }
        return
      }
      yield { type: 'complete' }
    })
    spyOn(acosmiClient, 'getAuthStatus').mockResolvedValue({
      authorized: true,
      tokens: {
        accessToken: 'retry-access-token',
        refreshToken: 'retry-refresh-token',
        expiresAt: Date.now() + 60_000,
        scopes: ['ai'],
      },
      hasProfileScope: false,
      isSubscriber: true,
    })
    spyOn(installOAuth, 'installOAuthTokens').mockResolvedValue({
      success: true,
    })
    spyOn(hooks, 'executeNotificationHooks').mockResolvedValue()
    const requests: CrabCodeTuiSetupRequest[] = []

    await runDirectTuiPreTrustOnboarding(
      executingRequester(requests, request => {
        switch (branchId(request)) {
          case 'onboarding/language':
            return response(request, { decision: 'skip' })
          case 'onboarding/theme':
            return response(request, {
              decision: 'select',
              theme: 'dark',
              syntax_highlighting_disabled: false,
            })
          case 'onboarding/oauth/select_method':
            return response(request, {
              decision: 'select',
              method: 'acosmi',
            })
          case 'onboarding/oauth/error':
            return response(request, { decision: 'retry' })
          case 'onboarding/oauth/success':
            return response(request, { decision: 'continue' })
          case 'onboarding/security':
            return response(request, { decision: 'continue' })
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
      }),
      { shouldSkipSetup: () => false },
    )

    expect(loginAttempt).toBe(2)
    expect(requests.map(branchId)).toEqual([
      'onboarding/language',
      'onboarding/theme',
      'onboarding/oauth/select_method',
      'onboarding/oauth/error',
      'onboarding/oauth/select_method',
      'onboarding/oauth/success',
      'onboarding/security',
    ])
    expect(
      requests.find(
        request => branchId(request) === 'onboarding/oauth/error',
      ),
    ).toMatchObject({
      error: expect.any(String),
      body: [expect.any(String)],
    })
  })

  test('executes every retained post-trust setup authority on the direct renderer route', async () => {
    delete process.env.CLAUBBIT
    process.env.ACOSMI_API_KEY = 'post-trust-authority-key'
    const configAuthority = installConfigAuthority({
      hasCompletedOnboarding: true,
    })
    const settingsWrites = installSharedSettingsAuthority({
      enabledMcpjsonServers: [],
      disabledMcpjsonServers: [],
    })
    spyOn(settings, 'hasSkipDangerousModePermissionPrompt').mockReturnValue(
      false,
    )
    spyOn(settings, 'hasAutoModeOptIn').mockReturnValue(false)
    spyOn(settingsErrors, 'getSettingsWithAllErrors').mockReturnValue({
      settings: {},
      errors: [],
    })
    spyOn(mcpConfig, 'getMcpConfigsByScope').mockReturnValue({
      servers: {
        alpha: {} as never,
        beta: {} as never,
      },
      errors: [],
    })
    spyOn(mcpUtils, 'getProjectMcpServerStatus').mockReturnValue('pending')
    spyOn(crabcodemd, 'shouldShowCrabcodeMdExternalIncludesWarning')
      .mockResolvedValue(true)
    spyOn(crabcodemd, 'getMemoryFiles').mockResolvedValue([])
    spyOn(crabcodemd, 'getExternalCrabcodeMdIncludes').mockReturnValue([
      {
        path: '/outside/CRABCODE.md',
        content: 'external',
      },
    ])
    spyOn(auth, 'getCustomApiKeyStatus').mockReturnValue('new')
    spyOn(auth, 'getAcosmiOAuthTokens').mockReturnValue({
      accessToken: 'access',
      refreshToken: null,
      expiresAt: null,
      scopes: ['ai'],
    })
    spyOn(envUtils, 'isRunningOnHomespace').mockReturnValue(false)
    spyOn(featureFlags, 'feature').mockImplementation(
      flag =>
        flag === 'TRANSCRIPT_CLASSIFIER' ||
        flag === 'KAIROS' ||
        flag === 'KAIROS_CHANNELS',
    )
    spyOn(channelAllowlist, 'isChannelsEnabled').mockReturnValue(true)
    spyOn(growthbook, 'checkGate_CACHED_OR_BLOCKING').mockResolvedValue(true)
    spyOn(bootstrapState, 'getAllowedChannels').mockReturnValue([])
    const allowedChannelWrites: unknown[][] = []
    spyOn(bootstrapState, 'setAllowedChannels').mockImplementation(channels => {
      allowedChannelWrites.push(channels)
    })
    const hasDevWrites: boolean[] = []
    spyOn(bootstrapState, 'setHasDevChannels').mockImplementation(value => {
      hasDevWrites.push(value)
    })
    const requests: CrabCodeTuiSetupRequest[] = []
    const requestSetup: DirectTuiSetupRequester =
      async function requestSetup<Response>(
        request: CrabCodeTuiSetupRequest,
        responseSchema: ZodType<Response>,
      ): Promise<Response> {
        requests.push(request)
        let selected: SetupResponse
        switch (branchId(request)) {
          case 'mcp_server_approval':
            selected = response(request, {
              decision: 'select',
              selected_server_names: ['alpha'],
            })
            break
          case 'external_crabcode_md':
            selected = response(request, { decision: 'allow' })
            break
          case 'grove_terms':
            selected = response(request, { decision: 'accept_opt_in' })
            break
          case 'api_key_approval':
            selected = response(request, { decision: 'accept' })
            break
          case 'bypass_permissions_consent':
            selected = response(request, { decision: 'accept' })
            break
          case 'auto_mode_opt_in':
            selected = response(request, {
              decision: 'accept_default',
            })
            break
          case 'development_channels':
            selected = response(request, { decision: 'accept' })
            break
          default:
            throw new Error(`unexpected setup branch ${branchId(request)}`)
        }
        return responseSchema.parse(selected)
      }

    const groveDecisions: string[] = []
    let afterTrustCalls = 0
    await runDirectTuiPostTrustSetup(
      requestSetup,
      {
        permissionMode: 'auto',
        allowDangerouslySkipPermissions: true,
        devChannels: [
          { kind: 'plugin', name: 'local', marketplace: 'dev' },
          { kind: 'server', name: 'socket' },
        ],
      },
      false,
      () => {
        afterTrustCalls += 1
      },
      {
        shouldSkipSetup: () => false,
        exit(exitCode): never {
          throw new Error(`unexpected setup exit ${exitCode}`)
        },
        async runGrove(renderer, location) {
          expect(location).toBe('policy_update_modal')
          const groveResponse = await renderer.handleNativeTuiGroveTerms({
            title: 'Grove terms',
            body: ['Review these terms.'],
            links: [
              { label: 'Terms', url: 'https://docs.test/grove' },
            ],
            options: [
              { decision: 'accept_opt_in', label: 'Accept' },
            ],
            dismissable: true,
          })
          groveDecisions.push(groveResponse.decision)
          return true
        },
      },
    )

    await Promise.resolve()
    expect(afterTrustCalls).toBe(1)
    expect(requests.map(branchId)).toEqual([
      'mcp_server_approval',
      'external_crabcode_md',
      'grove_terms',
      'api_key_approval',
      'bypass_permissions_consent',
      'auto_mode_opt_in',
      'development_channels',
    ])
    expect(groveDecisions).toEqual(['accept_opt_in'])
    expect(
      requests.find(request => branchId(request) === 'grove_terms'),
    ).not.toHaveProperty('dismissable')
    expect(settingsWrites).toEqual(
      expect.arrayContaining([
        {
          source: 'localSettings',
          value: { enabledMcpjsonServers: ['alpha'] },
        },
        {
          source: 'localSettings',
          value: { disabledMcpjsonServers: ['beta'] },
        },
        {
          source: 'userSettings',
          value: { skipDangerousModePermissionPrompt: true },
        },
        {
          source: 'userSettings',
          value: {
            skipAutoPermissionPrompt: true,
            permissions: { defaultMode: 'auto' },
          },
        },
      ]),
    )
    expect(configAuthority.projectWrites).toEqual([
      {
        hasCrabcodeMdExternalIncludesApproved: true,
        hasCrabcodeMdExternalIncludesWarningShown: true,
      },
    ])
    expect(configAuthority.current()).toMatchObject({
      customApiKeyResponses: {
        approved: ['-trust-authority-key'],
      },
    })
    expect(allowedChannelWrites).toEqual([
      [
        { kind: 'plugin', name: 'local', marketplace: 'dev', dev: true },
        { kind: 'server', name: 'socket', dev: true },
      ],
    ])
    expect(hasDevWrites).toEqual([true])
  })
})
